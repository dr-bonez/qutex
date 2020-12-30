//! A queue-backed exclusive data lock.
//!
//
// * It is unclear how many of the unsafe methods within need actually remain
//   unsafe.

use crossbeam::queue::SegQueue;
use futures::channel::oneshot::{self, Canceled, Receiver, Sender};
use std::cell::UnsafeCell;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Allows access to the data contained within a lock just like a mutex guard.
#[derive(Debug)]
pub struct Guard<T> {
    qutex: Qutex<T>,
}

impl<T> Guard<T> {
    /// Releases the lock held by a `Guard` and returns the original `Qutex`.
    pub fn unlock(guard: Guard<T>) -> Qutex<T> {
        let qutex = unsafe { ::std::ptr::read(&guard.qutex) };
        ::std::mem::forget(guard);
        unsafe { qutex.direct_unlock() }
        qutex
    }
}

impl<T> Deref for Guard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.qutex.inner.cell.get() }
    }
}

impl<T> DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.qutex.inner.cell.get() }
    }
}

impl<T> Drop for Guard<T> {
    fn drop(&mut self) {
        // unsafe { self.qutex.direct_unlock().expect("Error dropping Guard") };
        unsafe { self.qutex.direct_unlock() }
    }
}

/// A future which resolves to a `Guard`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct FutureGuard<T> {
    qutex: Option<Qutex<T>>,
    rx: Receiver<()>,
}

impl<T> FutureGuard<T> {
    /// Returns a new `FutureGuard`.
    fn new(qutex: Qutex<T>, rx: Receiver<()>) -> FutureGuard<T> {
        FutureGuard {
            qutex: Some(qutex),
            rx: rx,
        }
    }
}

impl<T> Future for FutureGuard<T> {
    type Output = Result<Guard<T>, Canceled>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        unsafe {
            let s = self.get_mut();
            if s.qutex.is_some() {
                s.qutex.as_ref().unwrap().process_queue();

                match Receiver::poll(Pin::new(&mut s.rx), cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(Guard {
                        qutex: s.qutex.take().unwrap(),
                    })),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                    Poll::Pending => Poll::Pending,
                }
            } else {
                panic!("FutureGuard::poll: Task already completed.");
            }
        }
    }
}

impl<T> Drop for FutureGuard<T> {
    /// Gracefully unlock if this guard has a lock acquired but has not yet
    /// been polled to completion.
    fn drop(&mut self) {
        if let Some(qutex) = self.qutex.take() {
            self.rx.close();

            match self.rx.try_recv() {
                Ok(status) => {
                    if status.is_some() {
                        unsafe {
                            qutex.direct_unlock();
                        }
                    }
                }
                Err(_) => (),
            }
        }
    }
}

/// A request to lock the qutex for exclusive access.
#[derive(Debug)]
pub struct Request {
    tx: Sender<()>,
}

impl Request {
    /// Returns a new `Request`.
    pub fn new(tx: Sender<()>) -> Request {
        Request { tx: tx }
    }
}

#[derive(Debug)]
struct Inner<T> {
    // TODO: Convert to `AtomicBool` if no additional states are needed:
    state: AtomicUsize,
    cell: UnsafeCell<T>,
    queue: SegQueue<Request>,
}

impl<T> From<T> for Inner<T> {
    #[inline]
    fn from(val: T) -> Inner<T> {
        Inner {
            state: AtomicUsize::new(0),
            cell: UnsafeCell::new(val),
            queue: SegQueue::new(),
        }
    }
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

/// A lock-free-queue-backed exclusive data lock.
#[derive(Debug)]
pub struct Qutex<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Qutex<T> {
    /// Creates and returns a new `Qutex`.
    #[inline]
    pub fn new(val: T) -> Qutex<T> {
        Qutex {
            inner: Arc::new(Inner::from(val)),
        }
    }

    /// Returns a new `FutureGuard` which can be used as a future and will
    /// resolve into a `Guard`.
    pub fn lock(self) -> FutureGuard<T> {
        let (tx, rx) = oneshot::channel();
        unsafe {
            self.push_request(Request::new(tx));
        }
        FutureGuard::new(self, rx)
    }

    /// Pushes a lock request onto the queue.
    ///
    //
    // TODO: Evaluate unsafe-ness.
    //
    #[inline]
    pub unsafe fn push_request(&self, req: Request) {
        self.inner.queue.push(req);
    }

    /// Returns a mutable reference to the inner `Vec` if there are currently
    /// no other copies of this `Qutex`.
    ///
    /// Since this call borrows the inner lock mutably, no actual locking needs to
    /// take place---the mutable borrow statically guarantees no locks exist.
    ///
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut T> {
        Arc::get_mut(&mut self.inner).map(|inn| unsafe { &mut *inn.cell.get() })
    }

    /// Returns a reference to the inner value.
    ///
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.inner.cell.get()
    }

    /// Returns a mutable reference to the inner value.
    ///
    #[inline]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.inner.cell.get()
    }

    /// Pops the next lock request in the queue if this (the caller's) lock is
    /// unlocked.
    //
    // TODO:
    // * This is currently public due to 'derivers' (aka. sub-types). Evaluate.
    // * Consider removing unsafe qualifier.
    // * Return proper error type.
    //
    pub unsafe fn process_queue(&self) {
        match self.inner.state.compare_and_swap(0, 1, SeqCst) {
            // Unlocked:
            0 => {
                loop {
                    if let Some(req) = self.inner.queue.pop() {
                        // If there is a send error, a requester has dropped
                        // its receiver so just go to the next.
                        if req.tx.send(()).is_err() {
                            continue;
                        } else {
                            break;
                        }
                    } else {
                        self.inner.state.store(0, SeqCst);
                        break;
                    }
                }
            }
            // Already locked, leave it alone:
            1 => (),
            // Something else:
            n => panic!("Qutex::process_queue: inner.state: {}.", n),
        }
    }

    /// Unlocks this (the caller's) lock and wakes up the next task in the
    /// queue.
    //
    // TODO:
    // * Evaluate unsafe-ness.
    // * Return proper error type
    // pub unsafe fn direct_unlock(&self) -> Result<(), ()> {
    pub unsafe fn direct_unlock(&self) {
        // TODO: Consider using `Ordering::Release`.
        self.inner.state.store(0, SeqCst);
        self.process_queue()
    }
}

impl<T> From<T> for Qutex<T> {
    #[inline]
    fn from(val: T) -> Qutex<T> {
        Qutex::new(val)
    }
}

// Avoids needing `T: Clone`.
impl<T> Clone for Qutex<T> {
    #[inline]
    fn clone(&self) -> Qutex<T> {
        Qutex {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
// Woefully incomplete:
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn simple() {
        let val = Qutex::from(999i32);

        println!("Reading val...");
        {
            let future_guard = val.clone().lock();
            let guard = future_guard.await.unwrap();
            println!("val: {}", *guard);
        }

        println!("Storing new val...");
        {
            let future_guard = val.clone().lock();
            let mut guard = future_guard.await.unwrap();

            *guard = 5;
        }

        println!("Reading val...");
        {
            let future_guard = val.clone().lock();
            let guard = future_guard.await.unwrap();
            println!("val: {}", *guard);
        }
    }

    #[tokio::test]
    async fn concurrent() {
        let thread_count = 20;
        let mut threads = Vec::with_capacity(thread_count);
        let start_val = 0i32;
        let qutex = Qutex::new(start_val);

        for _ in 0..thread_count {
            let future_guard = qutex.clone().lock();

            let future_write = future_guard.map(|res| {
                res.and_then(|mut guard| {
                    *guard += 1;
                    Ok(())
                })
            });

            threads.push(tokio::spawn(async { future_write.await.unwrap() }));
        }

        for _ in 0..thread_count {
            let future_guard = qutex.clone().lock();

            threads.push(tokio::spawn(async {
                let mut guard = future_guard.await.unwrap();
                *guard -= 1;
            }));
        }

        for thread in threads {
            thread.await.unwrap();
        }

        let guard = qutex.clone().lock().await.unwrap();
        assert_eq!(*guard, start_val);
    }

    #[test]
    fn future_guard_drop() {
        let lock = Qutex::from(true);
        let _future_guard_0 = lock.clone().lock();
        let _future_guard_1 = lock.clone().lock();
        let _future_guard_2 = lock.clone().lock();

        // TODO: FINISH ME
    }

    #[tokio::test]
    async fn explicit_unlock() {
        let lock = Qutex::from(true);

        let mut guard_0 = lock.clone().lock().await.unwrap();
        *guard_0 = false;
        let _ = Guard::unlock(guard_0);
        // Will deadlock if this doesn't work:
        let guard_1 = lock.clone().lock().await.unwrap();
        assert!(*guard_1 == false);
    }
}
