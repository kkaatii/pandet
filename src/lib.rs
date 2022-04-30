//! A lightweight library that helps you detect failure of spawned async tasks without having to `.await` their handles.
//! Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.
//!
//! ```rust
//! # use tokio as task;
//! # #[task::main]
//! # async fn main() {
//! use pandet::{PanicAlert, OnPanic};
//!
//! let alert = PanicAlert::new();
//! let detector = alert.new_detector();
//!
//! // Whichever async task spawner
//! task::spawn(
//!     async move {
//!         panic!();
//!     }
//!     .on_panic(&detector) // 👈
//! );
//!
//! assert!(alert.await.is_err());
//! # }
//! ```
//!
//! For `!Send` tasks, there is the [`UnsendOnPanic`] trait:
//! ```rust
//! # use tokio::{runtime, task};
//! # fn main() {
//! use pandet::{PanicAlert, UnsendOnPanic};
//!
//! let alert = PanicAlert::new();
//! let detector = alert.new_detector();
//!
//! # let local = task::LocalSet::new();
//! # let rt = runtime::Runtime::new().unwrap();
//! # local.block_on(&rt, async {
//! task::spawn_local(
//!     async move {
//!         panic!();
//!     }
//!     .unsend_on_panic(&detector)
//! );
//!
//! assert!(alert.await.is_err());
//! # }); }
//! ```
//!
//! Refined control over how to handle panics can also be implemented with [`PanicMonitor`]
//! which works like a stream of alerts. You may also pass some information to the alert/monitor
//! when a panic occurs:
//! ```rust
//! # use tokio as task;
//! # #[task::main]
//! # async fn main() {
//! use futures::StreamExt;
//! use pandet::{PanicMonitor, OnPanic};
//!
//! // Any Unpin + Send + 'static type works
//! struct PanicInfo {
//!     task_id: usize,
//! }
//!
//! let mut monitor = PanicMonitor::<PanicInfo>::new(); // Or simply PanicMonitor::new()
//! let detector = monitor.new_detector();
//!
//! for task_id in 0..=10 {
//!     task::spawn(
//!         async move {
//!             if task_id == 10 {
//!                 panic!();
//!             }
//!         }
//!         // Informs the monitor of which task panicked
//!         .on_panic_info(&detector, PanicInfo { task_id })
//!     );
//! }
//!
//! while let Some(res) = monitor.next().await {
//!     if let Err(e) = res {
//!         let info = e.0;
//!         assert_eq!(info.task_id, 10);
//!         break;
//!     }
//! }
//! # }
//! ```
//!

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{
    channel::{
        mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    stream::FuturesUnordered,
    FutureExt, Stream,
};

/// An error type that is returned by the panic alert/monitor when a panic occurs.
///
/// Its first and only field is the additional information emitted when the corresponding task panics.
/// It defaults to `()`.
pub struct Panicked<Info = ()>(pub Info)
where
    Info: Send + 'static;

/// Detects panics for `PanicAlert`/`PanicMonitor`s.
pub struct PanicDetector<Info = ()>(UnboundedSender<PanicHook<Info>>)
where
    Info: Send + 'static;

/// Used to bind a `PanicDetector` onto any task. Implemented for all `Future` types.
pub trait UnsendOnPanic<'a, T> {
    /// Consumes a task, and returns a new task with the `PanicDetector` bound to it.
    fn unsend_on_panic(self, detector: &PanicDetector) -> UnsendPanicAwareFuture<'a, T>;

    /// Binds a `PanicDetector` and emits additional information when the panic is detected.
    fn unsend_on_panic_info<Info>(
        self,
        detector: &PanicDetector<Info>,
        info: Info,
    ) -> UnsendPanicAwareFuture<'a, T>
    where
        Info: Send + 'static;
}

impl<'a, F> UnsendOnPanic<'a, F::Output> for F
where
    F: Future + 'a,
{
    fn unsend_on_panic(self, detector: &PanicDetector) -> UnsendPanicAwareFuture<'a, F::Output> {
        self.unsend_on_panic_info(detector, ())
    }

    fn unsend_on_panic_info<Info>(
        self,
        detector: &PanicDetector<Info>,
        info: Info,
    ) -> UnsendPanicAwareFuture<'a, F::Output>
    where
        Info: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        detector
            .0
            .unbounded_send(PanicHook { signaler: rx, info: Some(info) })
            .expect("panic alert/monitor dropped early");
        UnsendPanicAwareFuture::new(async move {
            let ret = self.await;
            tx.send(()).expect("panic alert/monitor dropped early");
            ret
        })
    }
}

#[doc(hidden)]
pub struct UnsendPanicAwareFuture<'a, T> {
    inner: Pin<Box<dyn Future<Output = T> + 'a>>,
}

impl<'a, T> UnsendPanicAwareFuture<'a, T> {
    fn new<Fut>(fut: Fut) -> Self
    where
        Fut: Future<Output = T> + 'a,
    {
        UnsendPanicAwareFuture {
            inner: Box::pin(fut),
        }
    }
}

impl<T> Future for UnsendPanicAwareFuture<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll_unpin(cx)
    }
}

/// Used to bind a `PanicDetector` onto a `Send` task. Implemented for all types that are
/// `Future + Send`.
pub trait OnPanic<'a, T> {
    /// Consumes a task, and returns a new task with the `PanicDetector` bound to it.
    fn on_panic(self, detector: &PanicDetector) -> PanicAwareFuture<'a, T>;

    /// Binds a `PanicDetector` and emits additional information when the panic is detected.
    fn on_panic_info<Info>(
        self,
        detector: &PanicDetector<Info>,
        info: Info,
    ) -> PanicAwareFuture<'a, T>
    where
        Info: Send + 'static;
}

impl<'a, F> OnPanic<'a, F::Output> for F
where
    F: Future + Send + 'a,
{
    fn on_panic(self, detector: &PanicDetector) -> PanicAwareFuture<'a, F::Output> {
        self.on_panic_info(detector, ())
    }

    fn on_panic_info<Info>(
        self,
        detector: &PanicDetector<Info>,
        info: Info,
    ) -> PanicAwareFuture<'a, F::Output>
    where
        Info: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        detector
            .0
            .unbounded_send(PanicHook { signaler: rx, info: Some(info) })
            .expect("panic alert/monitor dropped early");
        PanicAwareFuture::new(async move {
            let ret = self.await;
            tx.send(()).expect("panic alert/monitor dropped early");
            ret
        })
    }
}

#[doc(hidden)]
pub struct PanicAwareFuture<'a, T> {
    inner: Pin<Box<dyn Future<Output = T> + Send + 'a>>,
}

impl<'a, T> PanicAwareFuture<'a, T> {
    fn new<Fut>(fut: Fut) -> Self
    where
        Fut: Future<Output = T> + Send + 'a,
    {
        PanicAwareFuture {
            inner: Box::pin(fut),
        }
    }
}

impl<T> Future for PanicAwareFuture<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll_unpin(cx)
    }
}

struct PanicHook<Info>
where
    Info: Send + 'static,
{
    signaler: oneshot::Receiver<()>,
    info: Option<Info>,
}

impl<Info> Future for PanicHook<Info>
where
    Info: Unpin + Send + 'static,
{
    type Output = Result<(), Panicked<Info>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = self.signaler.poll_unpin(cx);
        res.map_err(|_| Panicked(self.info.take().expect("panic info already emitted")))
    }
}

/// A future that indicates whether a task has panicked.
///
/// When one of the tasks that is being detected panics, the `PanicAlert`
/// will finish with an `Err(Panicked)`.
pub struct PanicAlert<Info = ()>
where
    Info: Send + 'static,
{
    tx: UnboundedSender<PanicHook<Info>>,
    rx: UnboundedReceiver<PanicHook<Info>>,
    list: FuturesUnordered<PanicHook<Info>>,
}

impl<Info> PanicAlert<Info>
where
    Info: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicAlert {
            tx,
            rx,
            list: FuturesUnordered::new(),
        }
    }

    /// Creates a new `PanicDetector` that pairs with this `PanicAlert`.
    pub fn new_detector(&self) -> PanicDetector<Info> {
        PanicDetector(self.tx.clone())
    }
}

impl<Info> Future for PanicAlert<Info>
where
    Info: Unpin + Send + 'static,
{
    type Output = Result<(), Panicked<Info>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let rx_terminated = match Pin::new(&mut self.rx).poll_next(cx) {
            Poll::Pending => false,
            Poll::Ready(Some(r)) => {
                self.list.push(r);
                false
            }
            Poll::Ready(None) => true,
        };
        match Pin::new(&mut self.list).poll_next(cx) {
            Poll::Ready(Some(r)) => {
                if r.is_err() {
                    Poll::Ready(r)
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                if rx_terminated {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A [`Stream`](https://docs.rs/futures/latest/futures/stream/trait.Stream.html#) of detected panics.
pub struct PanicMonitor<Info = ()>
where
    Info: Send + 'static,
{
    tx: UnboundedSender<PanicHook<Info>>,
    rx: UnboundedReceiver<PanicHook<Info>>,
    list: FuturesUnordered<PanicHook<Info>>,
}

impl<Info> PanicMonitor<Info>
where
    Info: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicMonitor {
            tx,
            rx,
            list: FuturesUnordered::new(),
        }
    }

    /// Creates a new `PanicDetector` that pairs with this `PanicMonitor`.
    pub fn new_detector(&self) -> PanicDetector<Info> {
        PanicDetector(self.tx.clone())
    }
}

impl<Info> Stream for PanicMonitor<Info>
where
    Info: Unpin + Send + 'static,
{
    type Item = Result<(), Panicked<Info>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let rx_terminated = match Pin::new(&mut self.rx).poll_next(cx) {
            Poll::Pending => false,
            Poll::Ready(Some(r)) => {
                self.list.push(r);
                false
            }
            Poll::Ready(None) => true,
        };
        match Pin::new(&mut self.list).poll_next(cx) {
            Poll::Ready(Some(r)) => Poll::Ready(Some(r)),
            Poll::Ready(None) => {
                if rx_terminated {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Info: Unpin + Send + 'static> Unpin for PanicMonitor<Info> {}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn alert_works() {
        let alert = PanicAlert::new();
        let detector = alert.new_detector();

        tokio::spawn(
            async move {
                panic!();
            }
            .on_panic(&detector),
        );

        assert!(alert.await.is_err());
    }

    #[tokio::test]
    async fn unsend_works() {
        let alert = PanicAlert::new();
        let detector = alert.new_detector();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let _ = tokio::task::spawn_local(
                    async move {
                        panic!();
                    }
                    .unsend_on_panic(&detector),
                )
                .await;
            })
            .await;
        assert!(alert.await.is_err());
    }

    #[tokio::test]
    async fn monitor_works() {
        let mut monitor = PanicMonitor::new();
        let detector = monitor.new_detector();

        for i in 0..=10 {
            tokio::spawn(
                async move {
                    if i == 10 {
                        panic!();
                    }
                }
                .on_panic_info(&detector, 10),
            );
        }

        while let Some(res) = monitor.next().await {
            if res.is_err() {
                let id = res.unwrap_err().0;
                assert_eq!(id, 10);
                break;
            }
        }
    }
}
