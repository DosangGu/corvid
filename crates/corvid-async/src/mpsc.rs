//! Work queues with one receiving owner.

pub use async_channel::{Sender, WeakSender};

pub mod error {
    pub use async_channel::{TryRecvError, TrySendError};
}

pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let (send, receive) = async_channel::bounded(capacity);
    (send, Receiver(receive))
}

pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let (send, receive) = async_channel::unbounded();
    (UnboundedSender(send), Receiver(receive))
}

pub type UnboundedReceiver<T> = Receiver<T>;

#[derive(Debug)]
pub struct Receiver<T>(async_channel::Receiver<T>);

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.0.recv().await.ok()
    }

    pub fn blocking_recv(&mut self) -> Option<T> {
        self.0.recv_blocking().ok()
    }

    pub fn try_recv(&mut self) -> Result<T, error::TryRecvError> {
        self.0.try_recv()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn close(&mut self) {
        self.0.close();
    }
}

#[derive(Debug)]
pub struct UnboundedSender<T>(async_channel::Sender<T>);

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> UnboundedSender<T> {
    pub fn send(&self, value: T) -> Result<(), async_channel::SendError<T>> {
        match self.0.try_send(value) {
            Ok(()) => Ok(()),
            Err(error::TrySendError::Closed(value)) => Err(async_channel::SendError(value)),
            Err(error::TrySendError::Full(_)) => unreachable!("unbounded queue became full"),
        }
    }

    pub fn closed(&self) -> impl Future<Output = ()> + '_ {
        self.0.closed()
    }
}
