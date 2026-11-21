use tokio::sync::{mpsc, oneshot};

use super::Registry;
use crate::executor;

// Allow terminal workers, interactive clients and control clients to queue a
// transaction concurrently. The reducer never waits for a terminal or disk.
const CAPACITY: usize = 128;
type Operation = Box<dyn FnOnce(&mut Registry) + Send>;

#[derive(Clone)]
pub(crate) struct RegistryActor(mpsc::Sender<Operation>);

#[derive(Clone)]
pub(crate) struct WeakRegistry(mpsc::WeakSender<Operation>);

impl RegistryActor {
    pub(super) fn start(mut registry: Registry) -> Self {
        let (send, mut incoming) = mpsc::channel::<Operation>(CAPACITY);
        executor::spawn(async move {
            while let Some(operation) = incoming.recv().await {
                operation(&mut registry);
            }
        })
        .detach();
        Self(send)
    }

    pub(super) fn downgrade(&self) -> WeakRegistry {
        WeakRegistry(self.0.downgrade())
    }

    pub(super) async fn request<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut Registry) -> T + Send + 'static,
    ) -> T {
        let (operation, result) = operation(apply);
        self.0
            .send(operation)
            .await
            .expect("workspace registry stopped");
        result.await.expect("workspace registry stopped")
    }

    pub(super) fn defer(&self, apply: impl FnOnce(&mut Registry) + Send + 'static) {
        if let Err(mpsc::error::TrySendError::Full(operation)) = self.0.try_send(Box::new(apply)) {
            #[cfg(feature = "compio")]
            if compio::runtime::Runtime::try_current().is_none() {
                // Terminal workers run outside the executor. The registry
                // never waits on a terminal while applying an operation.
                let _ = self.0.blocking_send(operation);
                return;
            }
            let send = self.0.clone();
            executor::spawn(async move {
                let _ = send.send(operation).await;
            })
            .detach();
        }
    }
}

impl WeakRegistry {
    // Terminal workers already run on blocking threads. They may wait for
    // ordering, but do not keep the registry alive during daemon shutdown.
    pub(crate) fn request<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut Registry) -> T + Send + 'static,
    ) -> Option<T> {
        let send = self.0.upgrade()?;
        let (operation, result) = operation(apply);
        send.blocking_send(operation).ok()?;
        result.blocking_recv().ok()
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn defer(&self, apply: impl FnOnce(&mut Registry) + Send + 'static) {
        if let Some(send) = self.0.upgrade() {
            RegistryActor(send).defer(apply);
        }
    }
}

fn operation<T: Send + 'static>(
    apply: impl FnOnce(&mut Registry) -> T + Send + 'static,
) -> (Operation, oneshot::Receiver<T>) {
    let (reply, result) = oneshot::channel();
    (
        Box::new(move |registry| {
            if !reply.is_closed() {
                let _ = reply.send(apply(registry));
            }
        }),
        result,
    )
}
