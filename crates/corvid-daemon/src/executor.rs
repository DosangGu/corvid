#[cfg(not(feature = "compio"))]
pub(crate) type PeerTasks = tokio::task::JoinSet<anyhow::Result<()>>;

#[cfg(feature = "compio")]
mod native {
    use std::future::Future;
    use std::path::Path;

    use anyhow::Result;
    use compio::runtime::{JoinHandle, Runtime, spawn};
    use corvid_async::oneshot;
    use futures_util::FutureExt;
    use futures_util::stream::{FuturesUnordered, StreamExt};

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

        let (send_result, result) = oneshot::channel();
        std::thread::Builder::new()
            .name("corvid-daemon".into())
            .spawn(move || {
                let result = Runtime::new()
                    .map_err(anyhow::Error::from)
                    .and_then(|executor| {
                        executor.block_on(crate::run_native(
                            &runtime_path,
                            &state_path,
                            autostarted,
                            async move {
                                let _ = stopped.await;
                            },
                        ))
                    });
                let _ = send_result.send(result);
            })?;
        let mut result = result;
        tokio::select! {
            result = &mut result => result?,
            () = shutdown => {
                drop(stop);
                result.await?
            }
        }
    }
}

#[cfg(feature = "compio")]
pub(crate) use native::{PeerTasks, run};

use anyhow::Result;
use std::future::Future;

#[cfg(feature = "compio")]
use compio::runtime::{JoinHandle, spawn as spawn_task, spawn_blocking};
#[cfg(not(feature = "compio"))]
use tokio::task::{JoinHandle, spawn as spawn_task, spawn_blocking};

#[cfg(feature = "compio")]
pub(crate) use compio::time;
#[cfg(feature = "compio")]
pub(crate) use std::time::Instant;
#[cfg(not(feature = "compio"))]
pub(crate) use tokio::time;
#[cfg(not(feature = "compio"))]
pub(crate) use tokio::time::Instant;

pub(crate) struct Task(JoinHandle<()>);

impl Task {
    pub(crate) fn detach(self) {
        #[cfg(feature = "compio")]
        self.0.detach();
        #[cfg(not(feature = "compio"))]
        drop(self.0);
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    pub(crate) async fn finish(self) -> Result<()> {
        join_result(self.0).await
    }
}

#[cfg(feature = "compio")]
pub(crate) fn spawn(task: impl Future<Output = ()> + 'static) -> Task {
    Task(spawn_task(task))
}

#[cfg(not(feature = "compio"))]
pub(crate) fn spawn(task: impl Future<Output = ()> + Send + 'static) -> Task {
    Task(spawn_task(task))
}

pub(crate) fn spawn_worker(work: impl FnOnce() + Send + 'static) -> Task {
    Task(spawn_blocking(work))
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    join_result(spawn_blocking(work)).await
}

async fn join_result<T>(task: JoinHandle<T>) -> Result<T> {
    #[cfg(feature = "compio")]
    return task
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()));
    #[cfg(not(feature = "compio"))]
    Ok(task.await?)
}

#[cfg(all(test, feature = "compio"))]
pub(crate) use compio::runtime::spawn as spawn_test;
#[cfg(all(test, not(feature = "compio")))]
pub(crate) use tokio::spawn as spawn_test;
