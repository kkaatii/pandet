//! A lightweight library that helps you detect failure of spawned async tasks without having to
//! `.await` their handles.
//! Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.
//!
//! ```rust
//! # use tokio as task;
//! # #[task::main]
//! # async fn main() {
//! use pandet::{PanicAlert, OnPanic};
//!
//! let (alert, detector) = PanicAlert::new();
//!
//! // Whichever async task spawner
//! task::spawn(
//!     async move {
//!         panic!();
//!     }
//!     .on_panic(&detector) // 👈 Binds the alert's detector
//! );
//!
//! drop(detector);
//! assert!(alert.await.is_some()); // See notes below
//! # }
//! ```
//!
//! For `!Send` tasks, there is the [`UnsendOnPanic`] trait:
//! ```rust
//! # use tokio::{runtime, task};
//! # fn main() {
//! use pandet::{PanicAlert, UnsendOnPanic};
//!
//! let (alert, detector) = PanicAlert::new();
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
//! drop(detector);
//! assert!(alert.await.is_some());
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
//! let (mut monitor, detector) = PanicMonitor::<PanicInfo>::new(); // Or simply PanicMonitor::new()
//! for task_id in 0..=10 {
//!     task::spawn(
//!         async move {
//!             if task_id % 3 == 0 {
//!                 panic!();
//!             }
//!         }
//!         // Informs the monitor of which task panicked
//!         .on_panic_info(&detector, PanicInfo { task_id })
//!     );
//! }
//!
//! drop(detector);
//! while let Some(res) = monitor.next().await {
//!     let info = res.0;
//!     assert_eq!(info.task_id % 3, 0);
//! }
//! # }
//! ```

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use futures::stream::FuturesUnordered;
use futures::{
    channel::{
        mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    FutureExt, Stream,
};

/// Created by the panic detector when a panic occurs.
///
/// Its first and only field is the additional information emitted when the corresponding task panics.
/// The field defaults to `()`.
pub struct Panicked<Info = ()>(pub Info)
where
    Info: Send + 'static;

/// Notifies [`PanicAlert`]/[`PanicMonitor`] of panics.
///
/// Can be bounded to [`OnPanic`] and [`UnsendOnPanic`] types.
pub struct PanicDetector<Info = ()>(UnboundedSender<PanicHook<Info>>)
where
    Info: Send + 'static;

impl<Info> Clone for PanicDetector<Info>
where
    Info: Send + 'static,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

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
            .unbounded_send(PanicHook {
                signaler: rx,
                info: Some(info),
            })
            .expect("panic alert/monitor dropped early");
        UnsendPanicAwareFuture::new(async move {
            let ret = self.await;
            let _ = tx.send(());
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
            .unbounded_send(PanicHook {
                signaler: rx,
                info: Some(info),
            })
            .expect("panic alert/monitor dropped early");
        PanicAwareFuture::new(async move {
            let ret = self.await;
            let _ = tx.send(());
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
    type Output = Option<Panicked<Info>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = self.signaler.poll_unpin(cx);
        res.map(|r| {
            if r.is_err() {
                Some(Panicked(
                    self.info.take().expect("panic info already emitted"),
                ))
            } else {
                None
            }
        })
    }
}

/// A future that finishes with an `Some(Panicked<Info>)` when a task has panicked or `None` if no task panicked.
pub struct PanicAlert<Info = ()>
where
    Info: Send + 'static,
{
    rx: UnboundedReceiver<PanicHook<Info>>,
    hooks: FuturesUnordered<PanicHook<Info>>,
    rx_closed: bool,
}

impl<Info> PanicAlert<Info>
where
    Info: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> (Self, PanicDetector<Info>) {
        let (tx, rx) = unbounded();
        (
            PanicAlert {
                rx,
                hooks: FuturesUnordered::new(),
                rx_closed: false,
            },
            PanicDetector(tx),
        )
    }
}

impl<Info> Future for PanicAlert<Info>
where
    Info: Unpin + Send + 'static,
{
    type Output = Option<Panicked<Info>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        while !self.rx_closed {
            match Pin::new(&mut self.rx).poll_next(cx) {
                Poll::Pending => break,
                Poll::Ready(Some(r)) => self.hooks.push(r),
                Poll::Ready(None) => {
                    self.rx_closed = true;
                    break;
                }
            }
        }

        loop {
            let res = Pin::new(&mut self.hooks).poll_next(cx);
            match res {
                Poll::Ready(Some(r)) => {
                    if r.is_some() {
                        break Poll::Ready(r);
                    }
                }
                Poll::Ready(None) => {
                    if self.rx_closed {
                        break Poll::Ready(None);
                    } else {
                        break Poll::Pending;
                    }
                }
                Poll::Pending => {
                    break Poll::Pending;
                }
            }
        }
    }
}

/// A [`Stream`](https://docs.rs/futures/latest/futures/stream/trait.Stream.html#) of detected panics.
/// 
/// It finishes when all the futures that it's detecting for have finished.
pub struct PanicMonitor<Info = ()>
where
    Info: Send + 'static,
{
    rx: UnboundedReceiver<PanicHook<Info>>,
    hooks: FuturesUnordered<PanicHook<Info>>,
    rx_closed: bool,
}

impl<Info> PanicMonitor<Info>
where
    Info: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> (Self, PanicDetector<Info>) {
        let (tx, rx) = unbounded();
        (
            PanicMonitor {
                rx,
                hooks: FuturesUnordered::new(),
                rx_closed: false,
            },
            PanicDetector(tx),
        )
    }
}

impl<Info> Stream for PanicMonitor<Info>
where
    Info: Unpin + Send + 'static,
{
    type Item = Panicked<Info>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while !self.rx_closed {
            match Pin::new(&mut self.rx).poll_next(cx) {
                Poll::Pending => break,
                Poll::Ready(Some(r)) => self.hooks.push(r),
                Poll::Ready(None) => {
                    self.rx_closed = true;
                    break;
                }
            }
        }

        loop {
            let res = Pin::new(&mut self.hooks).poll_next(cx);
            match res {
                Poll::Ready(Some(r)) => {
                    if r.is_some() {
                        break Poll::Ready(r);
                    }
                }
                Poll::Ready(None) => {
                    if self.rx_closed {
                        break Poll::Ready(None);
                    } else {
                        break Poll::Pending;
                    }
                }
                Poll::Pending => {
                    break Poll::Pending;
                }
            }
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
        let (alert, detector) = PanicAlert::new();

        for i in 0..=10 {
            tokio::spawn(
                async move {
                    if i == 1 {
                        panic!("What could go wrong");
                    }
                }
                .on_panic(&detector),
            );
        }
        assert!(alert.await.is_some());

        let (alert, detector) = PanicAlert::new();
        (0..=10).for_each(|_| {
            tokio::spawn((|| async move {}.on_panic(&detector))());
        });
        drop(detector);
        assert!(alert.await.is_none());
    }

    #[tokio::test]
    async fn unsend_works() {
        let (alert, detector) = PanicAlert::new();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                {
                    let _ = tokio::task::spawn_local(
                        async move {
                            // panic!();
                        }
                        .unsend_on_panic(&detector),
                    );
                }
                drop(detector);
                assert!(alert.await.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn monitor_works() {
        let (mut monitor, detector) = PanicMonitor::new();

        for i in 0..=10 {
            let detector = detector.clone();
            tokio::spawn(
                async move {
                    if i % 3 == 0 {
                        panic!();
                    }
                }
                .on_panic_info(&detector, i),
            );
        }

        drop(detector);
        let mut count = 0;
        while let Some(res) = monitor.next().await {
            let id = res.0;
            assert_eq!(id % 3, 0);
            count += 1;
        }
        assert_eq!(count, 4);
    }
}
