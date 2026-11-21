//! A shared snapshot with independent change tracking for each subscriber.

use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use event_listener::Event;

pub use async_channel::{RecvError, SendError};

struct Versioned<T> {
    value: T,
    version: usize,
}

struct Shared<T> {
    value: RwLock<Versioned<T>>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
    changed: Event,
    unobserved: Event,
}

pub struct Sender<T>(Arc<Shared<T>>);

pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    seen: usize,
}

pub struct Ref<'a, T>(RwLockReadGuard<'a, Versioned<T>>);

impl<T> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0.value
    }
}

pub fn channel<T>(value: T) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        value: RwLock::new(Versioned { value, version: 0 }),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        changed: Event::new(),
        unobserved: Event::new(),
    });
    (Sender(Arc::clone(&shared)), Receiver { shared, seen: 0 })
}

impl<T> Sender<T> {
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref(self.0.value.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn subscribe(&self) -> Receiver<T> {
        self.0.receivers.fetch_add(1, Ordering::Relaxed);
        Receiver {
            shared: Arc::clone(&self.0),
            seen: self.borrow().0.version,
        }
    }

    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.is_closed() {
            return Err(SendError(value));
        }
        self.send_replace(value);
        Ok(())
    }

    pub fn send_replace(&self, value: T) -> T {
        let mut previous = value;
        self.send_modify(|stored| std::mem::swap(stored, &mut previous));
        previous
    }

    pub fn send_modify(&self, edit: impl FnOnce(&mut T)) {
        self.send_if_modified(|value| {
            edit(value);
            true
        });
    }

    pub fn send_if_modified(&self, edit: impl FnOnce(&mut T) -> bool) -> bool {
        let mut stored = self.0.value.write().unwrap_or_else(PoisonError::into_inner);
        if !edit(&mut stored.value) {
            return false;
        }
        stored.version = stored.version.wrapping_add(1);
        drop(stored);
        self.0.changed.notify(usize::MAX);
        true
    }

    pub fn is_closed(&self) -> bool {
        self.0.receivers.load(Ordering::Acquire) == 0
    }

    pub async fn closed(&self) {
        loop {
            let dropped = self.0.unobserved.listen();
            if self.is_closed() {
                return;
            }
            dropped.await;
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.senders.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0))
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.changed.notify(usize::MAX);
        }
    }
}

impl<T> Receiver<T> {
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref(self
            .shared
            .value
            .read()
            .unwrap_or_else(PoisonError::into_inner))
    }

    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        let value = self
            .shared
            .value
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        self.seen = value.version;
        Ref(value)
    }

    pub fn has_changed(&self) -> Result<bool, RecvError> {
        if self.shared.senders.load(Ordering::Acquire) == 0 {
            return Err(RecvError);
        }
        Ok(self.borrow().0.version != self.seen)
    }

    pub async fn wait_for(
        &mut self,
        mut predicate: impl FnMut(&T) -> bool,
    ) -> Result<Ref<'_, T>, RecvError> {
        loop {
            let notified = self.shared.changed.listen();
            let closed = self.shared.senders.load(Ordering::Acquire) == 0;
            {
                let value = self
                    .shared
                    .value
                    .read()
                    .unwrap_or_else(PoisonError::into_inner);
                self.seen = value.version;
                if predicate(&value.value) {
                    return Ok(Ref(value));
                }
            }
            if closed {
                return Err(RecvError);
            }
            notified.await;
        }
    }

    pub async fn changed(&mut self) -> Result<(), RecvError> {
        loop {
            // Subscribe before checking either condition so concurrent sends or
            // the last sender's drop cannot pass between the check and the wait.
            let notified = self.shared.changed.listen();
            let closed = self.shared.senders.load(Ordering::Acquire) == 0;
            let version = self.borrow().0.version;
            if version != self.seen {
                self.seen = version;
                return Ok(());
            }
            if closed {
                return Err(RecvError);
            }
            notified.await;
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.receivers.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
            seen: self.seen,
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.shared.receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.unobserved.notify(usize::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future::{block_on, poll_once};

    use super::*;

    #[test]
    fn subscribers_keep_independent_positions_and_coalesce_values() {
        block_on(async {
            let (sender, mut first) = channel(0);
            let mut second = first.clone();

            assert!(poll_once(first.changed()).await.is_none());
            sender.send_replace(1);
            sender.send_replace(2);
            first.changed().await.unwrap();
            assert!(!first.has_changed().unwrap());
            assert!(second.has_changed().unwrap());
            assert_eq!(*second.borrow_and_update(), 2);
            assert!(poll_once(second.changed()).await.is_none());
            assert!(!sender.subscribe().has_changed().unwrap());
        });
    }

    #[test]
    fn receives_unseen_value_after_last_sender_closes() {
        block_on(async {
            let (sender, mut receiver) = channel(0);
            let clone = sender.clone();
            drop(sender);
            clone.send_replace(1);
            drop(clone);
            assert!(receiver.has_changed().is_err());
            receiver.changed().await.unwrap();
            assert_eq!(*receiver.borrow(), 1);
            assert!(receiver.changed().await.is_err());
        });
    }

    #[test]
    fn closing_last_receiver_unblocks_sender_and_allows_resubscribing() {
        block_on(async {
            let (sender, receiver) = channel(0);
            let clone = receiver.clone();
            drop(receiver);
            assert!(poll_once(sender.closed()).await.is_none());
            drop(clone);
            sender.closed().await;
            assert!(sender.send(1).is_err());
            assert_eq!(sender.send_replace(2), 0);
            assert_eq!(*sender.subscribe().borrow(), 2);
        });
    }
}
