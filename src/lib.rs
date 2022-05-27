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
//! let mut alert = PanicAlert::new();
//!
//! // Whichever async task spawner
//! task::spawn(
//!     async move {
//!         panic!();
//!     }
//!     .on_panic(&alert.new_detector()) // 👈 Binds the alert's detector
//! );
//!
//! assert!(alert.drop_detector().await.is_err()); // See notes below
//! # }
//! ```
//! IMPORTANT NOTE: Directly `.await`ing an alert is possible, but in that case the alert as a
//! future will only finish when a task panics. Calling `drop_detector()` allows it to finish with
//! a `Ok(())` if no task panics as long as all the other `PanicDetector`s paired with the alert
//! has gone out of scope. See [`PanicAlert::drop_detector`] and [`PanicMonitor::drop_detector`]
//! for more details.
//!
//! For `!Send` tasks, there is the [`UnsendOnPanic`] trait:
//! ```rust
//! # use tokio::{runtime, task};
//! # fn main() {
//! use pandet::{PanicAlert, UnsendOnPanic};
//!
//! let mut alert = PanicAlert::new();
//!
//! # let local = task::LocalSet::new();
//! # let rt = runtime::Runtime::new().unwrap();
//! # local.block_on(&rt, async {
//! task::spawn_local(
//!     async move {
//!         panic!();
//!     }
//!     .unsend_on_panic(&alert.new_detector())
//! );
//!
//! assert!(alert.drop_detector().await.is_err());
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
//! {
//!     let detector = monitor.new_detector();
//!     for task_id in 0..=10 {
//!         task::spawn(
//!             async move {
//!                 if task_id % 3 == 0 {
//!                     panic!();
//!                 }
//!             }
//!             // Informs the monitor of which task panicked
//!             .on_panic_info(&detector, PanicInfo { task_id })
//!         );
//!     }
//! } // detector goes out of scope, allowing the monitor to finish after calling drop_detector()
//!
//! while let Some(res) = monitor.drop_detector().next().await {
//!     let info = res.unwrap_err().0;
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

/// An error type that is returned by the panic alert/monitor when a panic occurs.
///
/// Its first and only field is the additional information emitted when the corresponding task panics.
/// It defaults to `()`.
pub struct Panicked<Info = ()>(pub Info)
where
    Info: Send + 'static;

/// Notifies [`PanicAlert`]/[`PanicMonitor`] of panics.
///
/// Can be bounded to [`OnPanic`] and [`UnsendOnPanic`] types.
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
            .unbounded_send(PanicHook {
                signaler: rx,
                info: Some(info),
            })
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
            .unbounded_send(PanicHook {
                signaler: rx,
                info: Some(info),
            })
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

/// A future that finishes with an `Err(Panicked<Info>)` when a task has panicked.
///
/// When one of the tasks that is being detected panics, the `PanicAlert`
/// will finish with an `Err(Panicked)`, otherwise it will hang on indefinitely unless
/// `drop_detector()` has been called and all `PanicDetector` instances have gone out of scope.
/// Check out [`PanicAlert::drop_detector`] for details.
pub struct PanicAlert<Info = ()>
where
    Info: Send + 'static,
{
    tx: Option<UnboundedSender<PanicHook<Info>>>,
    rx: UnboundedReceiver<PanicHook<Info>>,
    hooks: FuturesUnordered<PanicHook<Info>>,
    rx_closed: bool,
}

impl<Info> PanicAlert<Info>
where
    Info: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicAlert {
            tx: Some(tx),
            rx,
            hooks: FuturesUnordered::new(),
            rx_closed: false,
        }
    }

    /// Creates a new `PanicDetector` that pairs with this `PanicAlert`.
    pub fn new_detector(&self) -> PanicDetector<Info> {
        PanicDetector(self.tx.as_ref().expect("detector already dropped").clone())
    }

    /// After calling this method, when there is no other detector instances paired with this alert,
    /// it can finish automatically.
    ///
    /// As an alert does not know how many tasks are to be spawned, it will not stop working
    /// until:
    /// 1. A panic occurs;
    /// 2. Or there is no `PanicDetector` paired with it and all tasks have finished.
    ///
    /// Calling this method will allow the second scenario to happen by dropping the `PanicDetector`
    /// instance contained within it, so when the outstanding detectors have gone out of scope,
    /// this alert will be able to finish.
    ///
    /// # Example
    ///
    /// The below code will park indefinitely:
    /// ```ignore
    /// let alert = PanicAlert::new();
    /// task::spawn(async {}.on_panic(&alert.new_detector()));
    /// assert!(alert.await.is_ok());
    /// ```
    /// But this will not:
    /// ```no_run
    /// # use pandet::{OnPanic, PanicAlert};
    /// # use tokio as task;
    /// # task::runtime::Runtime::new().unwrap().block_on(async {
    /// let mut alert = PanicAlert::new();
    /// task::spawn(async {}.on_panic(&alert.new_detector()));
    /// assert!(alert.drop_detector().await.is_ok());
    /// # });
    /// ```
    /// Also be careful with `PanicDetector` instances:
    /// ```no_run
    /// # use pandet::{OnPanic, PanicAlert};
    /// # use tokio as task;
    /// # task::runtime::Runtime::new().unwrap().block_on(async {
    /// let mut alert = PanicAlert::new();
    /// let detector = alert.new_detector();
    /// task::spawn(async {}.on_panic(&detector));
    /// drop(detector); // Need to drop it
    /// assert!(alert.drop_detector().await.is_ok());
    /// # });
    /// ```
    pub fn drop_detector(&mut self) -> &mut Self {
        self.tx = None;
        self
    }
}

impl<Info> Future for PanicAlert<Info>
where
    Info: Unpin + Send + 'static,
{
    type Output = Result<(), Panicked<Info>>;

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
                    if r.is_err() {
                        break Poll::Ready(r);
                    }
                }
                Poll::Ready(None) => {
                    if self.rx_closed {
                        break Poll::Ready(Ok(()));
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
/// This stream emits `Err(Panicked<Info>)` when tasks panic, and nothing otherwise.
/// Check out [`PanicMonitor::drop_detector`] for details about when this stream will finish.
pub struct PanicMonitor<Info = ()>
where
    Info: Send + 'static,
{
    tx: Option<UnboundedSender<PanicHook<Info>>>,
    rx: UnboundedReceiver<PanicHook<Info>>,
    hooks: FuturesUnordered<PanicHook<Info>>,
    rx_closed: bool,
}

impl<Info> PanicMonitor<Info>
where
    Info: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicMonitor {
            tx: Some(tx),
            rx,
            hooks: FuturesUnordered::new(),
            rx_closed: false,
        }
    }

    /// Creates a new `PanicDetector` that pairs with this `PanicMonitor`.
    pub fn new_detector(&self) -> PanicDetector<Info> {
        PanicDetector(self.tx.as_ref().expect("detector already dropped").clone())
    }

    /// After calling this method, when there is no other detector instances paired with this monitor,
    /// it can finish automatically.
    ///
    /// As a monitor does not know how many tasks are to be spawned, it will not stop working
    /// until there is no `PanicDetector` paired with it and all tasks have finished.
    ///
    /// Calling this method will drop the `PanicDetector` instance contained within it, so when the
    /// outstanding detectors have gone out of scope, this monitor will be able to finish.
    ///
    /// # Example
    ///
    /// The below code will park indefinitely:
    /// ```ignore
    /// let mut monitor = PanicMonitor::new();
    /// task::spawn(async {}.on_panic(&monitor.new_detector()));
    /// while let Some(_) = monitor.next().await {}
    /// ```
    /// But this will not:
    /// ```no_run
    /// # use pandet::{OnPanic, PanicMonitor};
    /// # use tokio as task;
    /// # use futures::StreamExt;
    /// # task::runtime::Runtime::new().unwrap().block_on(async {
    /// let mut monitor = PanicMonitor::new();
    /// task::spawn(async {}.on_panic(&monitor.new_detector()));
    /// while let Some(_) = monitor.drop_detector().next().await {}
    /// # });
    /// ```
    /// Also be careful with `PanicDetector` instances:
    /// ```no_run
    /// # use pandet::{OnPanic, PanicMonitor};
    /// # use tokio as task;
    /// # use futures::StreamExt;
    /// # task::runtime::Runtime::new().unwrap().block_on(async {
    /// let mut monitor = PanicMonitor::new();
    /// let detector = monitor.new_detector();
    /// task::spawn(async {}.on_panic(&detector));
    /// drop(detector); // Need to drop it
    /// while let Some(_) = monitor.drop_detector().next().await {}
    /// # });
    /// ```
    pub fn drop_detector(&mut self) -> &mut Self {
        self.tx = None;
        self
    }
}

impl<Info> Stream for PanicMonitor<Info>
where
    Info: Unpin + Send + 'static,
{
    type Item = Result<(), Panicked<Info>>;

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
                    if r.is_err() {
                        break Poll::Ready(Some(r));
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
        let alert = PanicAlert::new();
        let detector = alert.new_detector();

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
        assert!(alert.await.is_err());

        let mut alert = PanicAlert::new();
        (0..=10).for_each(|_| {
            let detector = alert.new_detector();
            tokio::spawn((move || async move {}.on_panic(&detector))());
        });
        assert!(alert.drop_detector().await.is_ok());
    }

    #[tokio::test]
    async fn unsend_works() {
        let mut alert = PanicAlert::new();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                {
                    let detector = alert.new_detector();
                    let _ = tokio::task::spawn_local(
                        async move {
                            // panic!();
                        }
                        .unsend_on_panic(&detector),
                    );
                }
                assert!(alert.drop_detector().await.is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn monitor_works() {
        let mut monitor = PanicMonitor::new();

        for i in 0..=10 {
            tokio::spawn(
                async move {
                    if i % 5 == 0 {
                        panic!();
                    }
                }
                .on_panic_info(&monitor.new_detector(), i),
            );
        }

        while let Some(res) = monitor.drop_detector().next().await {
            let id = res.unwrap_err().0;
            assert_eq!(id % 5, 0);
        }
    }
}
