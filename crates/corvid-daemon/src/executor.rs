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

use std::future::Future;

use anyhow::Result;

pub(crate) enum Task {
    Tokio(tokio::task::JoinHandle<()>),
    #[cfg(feature = "compio")]
    Compio(compio::runtime::JoinHandle<()>),
}

impl Task {
    pub(crate) fn detach(self) {
        match self {
            Self::Tokio(task) => drop(task),
            #[cfg(feature = "compio")]
            Self::Compio(task) => task.detach(),
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        match self {
            Self::Tokio(task) => task.is_finished(),
            #[cfg(feature = "compio")]
            Self::Compio(task) => task.is_finished(),
        }
    }

    pub(crate) async fn finish(self) -> Result<()> {
        match self {
            Self::Tokio(task) => Ok(task.await?),
            #[cfg(feature = "compio")]
            Self::Compio(task) => task
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string())),
        }
    }
}

pub(crate) fn spawn(task: impl Future<Output = ()> + Send + 'static) -> Task {
    #[cfg(feature = "compio")]
    if let Some(runtime) = compio::runtime::Runtime::try_current() {
        return Task::Compio(runtime.spawn(task));
    }
    Task::Tokio(tokio::spawn(task))
}

pub(crate) fn spawn_worker(work: impl FnOnce() + Send + 'static) -> Task {
    #[cfg(feature = "compio")]
    if let Some(runtime) = compio::runtime::Runtime::try_current() {
        return Task::Compio(runtime.spawn_blocking(work));
    }
    Task::Tokio(tokio::task::spawn_blocking(work))
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    #[cfg(feature = "compio")]
    if compio::runtime::Runtime::try_current().is_some() {
        return compio::runtime::spawn_blocking(work)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()));
    }
    Ok(tokio::task::spawn_blocking(work).await?)
}
