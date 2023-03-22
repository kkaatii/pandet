//! A lightweight library that helps you detect failure of spawned async tasks without having to
//! `.await` their handles.
//! Useful when you are spawning lots of detached tasks but want to fast-fail if a panic occurs.
//!
//! ```rust
//! # use tokio as task;
//! # #[task::main]
//! # async fn main() {
//! use pandet::*;
//!
//! let detector = PanicDetector::new();
//!
//! // Whichever async task spawner
//! task::spawn(
//!     async move {
//!         panic!();
//!     }
//!     .alert(&detector) // 👈 Binds the detector so it is notified of any panic from the future
//! );
//!
//! assert!(detector.await.is_some()); // See notes below
//! # }
//! ```
//!
//! `!Send` tasks implement the [`LocalAlert`] trait:
//! ```rust
//! # use tokio::{runtime, task};
//! # fn main() {
//! use pandet::*;
//!
//! let detector = PanicDetector::new();
//!
//! # let local = task::LocalSet::new();
//! # let rt = runtime::Runtime::new().unwrap();
//! # local.block_on(&rt, async {
//! task::spawn_local(
//!     async move {
//!         // Does some work without panicking...
//!     }
//!     .local_alert(&detector)
//! );
//!
//! assert!(detector.await.is_none());
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
//! use pandet::*;
//!
//! // Any Unpin + Send + 'static type works
//! struct FailureMsg {
//!     task_id: usize,
//! }
//!
//! let mut monitor = PanicMonitor::<FailureMsg>::new(); // Or simply PanicMonitor::new()
//! for task_id in 0..=10 {
//!     task::spawn(
//!         async move {
//!             if task_id % 3 == 0 {
//!                 panic!();
//!             }
//!         }
//!         // Notifies the monitor of the panicked task's ID
//!         .alert_msg(&monitor, FailureMsg { task_id })
//!     );
//! }
//!
//! while let Some(Panicked(msg)) = monitor.next().await {
//!     assert_eq!(msg.task_id % 3, 0);
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
pub struct Panicked<Msg = ()>(pub Msg)
where
    Msg: Send + 'static;

/// Notifies [`PanicDetector`]/[`PanicMonitor`] of panics.
///
/// Can be bounded to [`Alert`] and [`LocalAlert`] types.
pub struct DetectorHook<Msg = ()>(UnboundedSender<RxHandle<Msg>>)
where
    Msg: Send + 'static;

impl<Msg> Clone for DetectorHook<Msg>
where
    Msg: Send + 'static,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// Used to bind a [`PanicDetector`] onto any task. Implemented for all `Future` types.
pub trait LocalAlert<'a, T> {
    /// Consumes a task, and returns a new task with the `PanicDetector` bound to it.
    fn local_alert<A>(self, hook: &'_ A) -> LocalPanicAwareFuture<'a, T>
    where
        A: AsRef<DetectorHook>;

    /// Binds a `PanicDetector` and emits additional information when the panic is detected.
    fn local_alert_msg<Msg, A>(self, hook: &'_ A, msg: Msg) -> LocalPanicAwareFuture<'a, T>
    where
        Msg: Send + 'static,
        A: AsRef<DetectorHook<Msg>>;
}

impl<'a, F> LocalAlert<'a, F::Output> for F
where
    F: Future + 'a,
{
    fn local_alert<A>(self, hook: &'_ A) -> LocalPanicAwareFuture<'a, F::Output>
    where
        A: AsRef<DetectorHook>,
    {
        self.local_alert_msg(hook, ())
    }

    fn local_alert_msg<Msg, A>(self, hook: &'_ A, msg: Msg) -> LocalPanicAwareFuture<'a, F::Output>
    where
        Msg: Send + 'static,
        A: AsRef<DetectorHook<Msg>>,
    {
        let (tx, rx) = oneshot::channel();
        hook.as_ref()
            .0
            .unbounded_send(RxHandle {
                signaler: rx,
                msg: Some(msg),
            })
            .expect("detector dropped early");
        LocalPanicAwareFuture::new(async move {
            let ret = self.await;
            let _ = tx.send(());
            ret
        })
    }
}

#[doc(hidden)]
pub struct LocalPanicAwareFuture<'a, T> {
    inner: Pin<Box<dyn Future<Output = T> + 'a>>,
}

impl<'a, T> LocalPanicAwareFuture<'a, T> {
    fn new<Fut>(fut: Fut) -> Self
    where
        Fut: Future<Output = T> + 'a,
    {
        LocalPanicAwareFuture {
            inner: Box::pin(fut),
        }
    }
}

impl<T> Future for LocalPanicAwareFuture<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll_unpin(cx)
    }
}

/// Used to bind a [`PanicDetector`] onto a `Send` task. Implemented for all types that are
/// `Future + Send`.
pub trait Alert<'a, T> {
    /// Consumes a task, and returns a new task with the `PanicDetector` bound to it.
    fn alert<A>(self, hook: &'_ A) -> PanicAwareFuture<'a, T>
    where
        A: AsRef<DetectorHook>;

    /// Binds a `PanicDetector` and emits additional information when the panic is detected.
    fn alert_msg<Msg, A>(self, hook: &'_ A, msg: Msg) -> PanicAwareFuture<'a, T>
    where
        Msg: Send + 'static,
        A: AsRef<DetectorHook<Msg>>;
}

impl<'a, F> Alert<'a, F::Output> for F
where
    F: Future + Send + 'a,
{
    fn alert<A>(self, hook: &'_ A) -> PanicAwareFuture<'a, F::Output>
    where
        A: AsRef<DetectorHook>,
    {
        self.alert_msg(hook, ())
    }

    fn alert_msg<Msg, A>(self, hook: &'_ A, msg: Msg) -> PanicAwareFuture<'a, F::Output>
    where
        Msg: Send + 'static,
        A: AsRef<DetectorHook<Msg>>,
    {
        let (tx, rx) = oneshot::channel();
        hook.as_ref()
            .0
            .unbounded_send(RxHandle {
                signaler: rx,
                msg: Some(msg),
            })
            .expect("detector dropped early");
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

struct RxHandle<Msg>
where
    Msg: Send + 'static,
{
    signaler: oneshot::Receiver<()>,
    msg: Option<Msg>,
}

impl<Msg> Future for RxHandle<Msg>
where
    Msg: Unpin + Send + 'static,
{
    type Output = Option<Panicked<Msg>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = self.signaler.poll_unpin(cx);
        res.map(|r| {
            if r.is_err() {
                Some(Panicked(self.msg.take().expect("message already read")))
            } else {
                None
            }
        })
    }
}

/// A future that finishes with an `Some(Panicked<Msg>)` when a task has panicked or `None` if no task panicked.
pub struct PanicDetector<Msg = ()>
where
    Msg: Send + 'static,
{
    detector: Option<DetectorHook<Msg>>,
    rx: UnboundedReceiver<RxHandle<Msg>>,
    hooks: FuturesUnordered<RxHandle<Msg>>,
    rx_closed: bool,
}

impl<Msg> PanicDetector<Msg>
where
    Msg: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicDetector {
            detector: Some(DetectorHook(tx)),
            rx,
            hooks: FuturesUnordered::new(),
            rx_closed: false,
        }
    }
}

impl<Msg> AsRef<DetectorHook<Msg>> for PanicDetector<Msg>
where
    Msg: Unpin + Send + 'static,
{
    fn as_ref(&self) -> &DetectorHook<Msg> {
        match self.detector {
            Some(ref det) => det,
            None => panic!(
                "This detector has been polled. Create a new detector to receive new panic alerts."
            ),
        }
    }
}

impl<Msg> Future for PanicDetector<Msg>
where
    Msg: Unpin + Send + 'static,
{
    type Output = Option<Panicked<Msg>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.detector = None;
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
pub struct PanicMonitor<Msg = ()>
where
    Msg: Send + 'static,
{
    detector: Option<DetectorHook<Msg>>,
    rx: UnboundedReceiver<RxHandle<Msg>>,
    hooks: FuturesUnordered<RxHandle<Msg>>,
    rx_closed: bool,
}

impl<Msg> PanicMonitor<Msg>
where
    Msg: Send + 'static,
{
    /// Creates a new `PanicMonitor`.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicMonitor {
            detector: Some(DetectorHook(tx)),
            rx,
            hooks: FuturesUnordered::new(),
            rx_closed: false,
        }
    }
}

impl<Msg> AsRef<DetectorHook<Msg>> for PanicMonitor<Msg>
where
    Msg: Unpin + Send + 'static,
{
    fn as_ref(&self) -> &DetectorHook<Msg> {
        match self.detector {
            Some(ref det) => det,
            None => panic!(
                "This monitor has been polled. Create a new monitor to receive new panic alerts."
            ),
        }
    }
}

impl<Msg> Stream for PanicMonitor<Msg>
where
    Msg: Unpin + Send + 'static,
{
    type Item = Panicked<Msg>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.detector = None;
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

impl<Msg: Unpin + Send + 'static> Unpin for PanicMonitor<Msg> {}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn alert_works() {
        let detector = PanicDetector::new();

        for i in 0..=10 {
            tokio::spawn(
                async move {
                    if i == 1 {
                        panic!("What could go wrong");
                    }
                }
                .alert(&detector),
            );
        }
        assert!(detector.await.is_some());

        let detector = PanicDetector::new();
        (0..=10).for_each(|_| {
            tokio::spawn((|| async move {}.alert(&detector))());
        });
        assert!(detector.await.is_none());
    }

    #[tokio::test]
    async fn unsend_works() {
        let detector = PanicDetector::new();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                {
                    let _ = tokio::task::spawn_local(
                        async move {
                            // panic!();
                        }
                        .local_alert(&detector),
                    );
                }
                assert!(detector.await.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn monitor_works() {
        let mut monitor = PanicMonitor::new();

        for i in 0..=10 {
            tokio::spawn(
                async move {
                    if i % 3 == 0 {
                        panic!();
                    }
                }
                .alert_msg(&monitor, i),
            );
        }

        let mut count = 0;
        while let Some(res) = monitor.next().await {
            let id = res.0;
            assert_eq!(id % 3, 0);
            count += 1;
        }
        assert_eq!(count, 4);
    }
}
