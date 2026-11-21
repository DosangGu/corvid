#[cfg(not(feature = "compio"))]
pub(crate) type PeerTasks = tokio::task::JoinSet<anyhow::Result<()>>;

#[cfg(feature = "compio")]
mod native {
    use std::future::Future;
    use std::path::Path;

    use anyhow::Result;
    use compio::runtime::{JoinHandle, Runtime, spawn};
    use futures_util::FutureExt;
    use futures_util::stream::{FuturesUnordered, StreamExt};
    use tokio::sync::oneshot;

    pub(crate) struct PeerTasks(FuturesUnordered<JoinHandle<Result<()>>>);

    impl PeerTasks {
        pub(crate) fn new() -> Self {
            Self(FuturesUnordered::new())
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        pub(crate) fn len(&self) -> usize {
            self.0.len()
        }

        pub(crate) fn spawn(&mut self, task: impl Future<Output = Result<()>> + 'static) {
            self.0.push(spawn(task));
        }

        pub(crate) async fn join_next(&mut self) -> Option<()> {
            self.0.next().await.map(drop)
        }

        pub(crate) fn try_join_next(&mut self) -> Option<()> {
            self.0.next().now_or_never().flatten().map(drop)
        }

        pub(crate) async fn shutdown(&mut self) {
            // Cancellation must finish before terminal handoff releases state.
            let pending: FuturesUnordered<_> = std::mem::take(&mut self.0)
                .into_iter()
                .map(JoinHandle::cancel)
                .collect();
            pending.for_each(|_| async {}).await;
        }
    }

    pub(crate) async fn run(
        runtime_path: &Path,
        state_path: &Path,
        autostarted: bool,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()> {
        let runtime_path = runtime_path.to_owned();
        let state_path = state_path.to_owned();
        let (stop, stopped) = oneshot::channel::<()>();

        // Keep the completion driver and all its connection tasks on one
        // thread. The caller's Tokio runtime still drives the workspace,
        // persistence, and terminal-host tasks during migration.
        let mut worker = tokio::task::spawn_blocking(move || {
            let executor = Runtime::new()?;
            executor.block_on(crate::serve_daemon(
                &runtime_path,
                &state_path,
                autostarted,
                async move {
                    let _ = stopped.await;
                },
            ))
        });
        tokio::select! {
            result = &mut worker => result?,
            () = shutdown => {
                drop(stop);
                worker.await?
            }
        }
    }
}

#[cfg(feature = "compio")]
pub(crate) use native::{PeerTasks, run};
