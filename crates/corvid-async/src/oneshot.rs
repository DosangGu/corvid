//! Single replies and cancellation notifications across workers.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_channel::oneshot;

pub mod error {
    pub use async_channel::TryRecvError;
}
pub use futures_channel::oneshot::Canceled;

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (send, receive) = oneshot::channel();
    (Sender(send), Receiver(receive))
}

#[derive(Debug)]
pub struct Sender<T>(oneshot::Sender<T>);

impl<T> Sender<T> {
    pub fn send(self, value: T) -> Result<(), T> {
        self.0.send(value)
    }

    pub fn closed(&mut self) -> impl Future<Output = ()> + '_ {
        self.0.cancellation()
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_canceled()
    }
}

#[derive(Debug)]
pub struct Receiver<T>(oneshot::Receiver<T>);

impl<T> Receiver<T> {
    pub fn try_recv(&mut self) -> Result<T, error::TryRecvError> {
        match self.0.try_recv() {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(error::TryRecvError::Empty),
            Err(_) => Err(error::TryRecvError::Closed),
        }
    }

    pub fn blocking_recv(self) -> Result<T, Canceled> {
        futures_lite::future::block_on(self)
    }

    pub fn close(&mut self) {
        self.0.close();
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(context)
    }
}
