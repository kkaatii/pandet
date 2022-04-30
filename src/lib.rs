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

pub struct Panicked;

pub struct PanicDetector(UnboundedSender<oneshot::Receiver<()>>);

pub trait DetectPanic<Wrapper> {
    fn detect_panic(self, detector: &PanicDetector) -> Wrapper;
}

impl<'a, F> DetectPanic<PanicAwareFuture<'a, F::Output>> for F
    where
        F: Future + 'a,
{
    fn detect_panic(self, detector: &PanicDetector) -> PanicAwareFuture<'a, F::Output> {
        let (tx, rx) = oneshot::channel();
        detector
            .0
            .unbounded_send(rx)
            .expect("failed to add panic hook to PanicDetector");
        PanicAwareFuture::new(async move {
            let ret = self.await;
            tx.send(()).unwrap();
            ret
        })
    }
}

pub struct PanicAwareFuture<'a, T> {
    inner: Pin<Box<dyn Future<Output = T> + 'a>>,
}

impl<'a, T> PanicAwareFuture<'a, T> {
    fn new<F>(fut: F) -> Self
        where
            F: Future<Output = T> + 'a,
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

impl<T> Unpin for PanicAwareFuture<'_, T> {}

/// A `Future` that indicates whether a task has panicked.
///
/// When one of the tasks that is being detected fails, the `PanicDetector`'s `PanicAlert`
/// will finish with an `Err(Panicked)`.
pub struct PanicAlert {
    tx: UnboundedSender<oneshot::Receiver<()>>,
    rx: UnboundedReceiver<oneshot::Receiver<()>>,
    list: FuturesUnordered<oneshot::Receiver<()>>,
}

impl PanicAlert {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicAlert {
            tx,
            rx,
            list: FuturesUnordered::new(),
        }
    }

    pub fn new_detector(&self) -> PanicDetector {
        PanicDetector(self.tx.clone())
    }
}

impl Future for PanicAlert {
    type Output = Result<(), Panicked>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let rx = Pin::new(&mut self.rx);
        match rx.poll_next(cx) {
            Poll::Pending => (),
            Poll::Ready(Some(r)) => {
                self.list.push(r);
            }
            Poll::Ready(None) => return Poll::Ready(Ok(())),
        }
        let list = Pin::new(&mut self.list);
        match list.poll_next(cx) {
            Poll::Pending => (),
            Poll::Ready(Some(r)) => {
                if r.is_err() {
                    return Poll::Ready(Err(Panicked));
                }
            }
            Poll::Ready(None) => (),
        }
        Poll::Pending
    }
}

pub struct PanicMonitor {
    tx: UnboundedSender<oneshot::Receiver<()>>,
    rx: UnboundedReceiver<oneshot::Receiver<()>>,
    list: FuturesUnordered<oneshot::Receiver<()>>,
}

impl PanicMonitor {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        PanicMonitor {
            tx,
            rx,
            list: FuturesUnordered::new(),
        }
    }

    pub fn new_detector(&self) -> PanicDetector {
        PanicDetector(self.tx.clone())
    }
}

impl Stream for PanicMonitor {
    type Item = Result<(), Panicked>;

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
            Poll::Ready(Some(r)) => Poll::Ready(Some(r.map_err(|_| Panicked))),
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

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
