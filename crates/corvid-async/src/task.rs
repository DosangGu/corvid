//! Structured tasks on a Compio worker.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use compio::runtime::Runtime;
use futures_util::future::{AbortHandle, Abortable};
use futures_util::stream::{FuturesUnordered, StreamExt};

type NativeHandle<T> = compio::runtime::JoinHandle<Result<T, futures_util::future::Aborted>>;

#[derive(Clone, Copy, Debug)]
pub enum JoinError {
    Cancelled,
    Panicked,
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "task was cancelled",
            Self::Panicked => "task panicked",
        })
    }
}

impl std::error::Error for JoinError {}

impl From<compio::runtime::JoinError> for JoinError {
    fn from(error: compio::runtime::JoinError) -> Self {
        match error {
            compio::runtime::JoinError::Cancelled => Self::Cancelled,
            compio::runtime::JoinError::Panicked(_) => Self::Panicked,
        }
    }
}

pub fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl Future<Output = Result<T, JoinError>> {
    let task = compio::runtime::spawn_blocking(work);
    async move { task.await.map_err(JoinError::from) }
}

pub struct JoinHandle<T> {
    task: Option<NativeHandle<T>>,
    abort: AbortHandle,
}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.abort.abort();
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(NativeHandle::is_finished)
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let task = self.task.as_mut().expect("task already joined");
        Pin::new(task).poll(cx).map(|result| {
            result
                .map_err(JoinError::from)
                .and_then(|value| value.map_err(|_| JoinError::Cancelled))
        })
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let (abort, registration) = AbortHandle::new_pair();
    let task = compio::runtime::spawn(Abortable::new(future, registration));
    JoinHandle {
        task: Some(task),
        abort,
    }
}

pub async fn yield_now() {
    futures_lite::future::yield_now().await;
}

pub struct JoinSet<T>(FuturesUnordered<JoinHandle<T>>);

impl<T> Default for JoinSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> JoinSet<T> {
    pub fn new() -> Self {
        Self(FuturesUnordered::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn spawn(&mut self, future: impl Future<Output = T> + 'static) {
        self.0.push(spawn(future));
    }

    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        self.0.next().await
    }

    pub fn abort_all(&self) {
        for task in self.0.iter() {
            task.abort();
        }
    }

    pub async fn shutdown(&mut self) {
        self.abort_all();
        while self.join_next().await.is_some() {}
    }
}

impl<T> Drop for JoinSet<T> {
    fn drop(&mut self) {
        self.abort_all();
    }
}

pub fn block_on<F: Future>(future: F) -> io::Result<F::Output> {
    Ok(Runtime::new()?.block_on(future))
}

/// Start a local Compio worker and report initialization before returning.
pub fn worker(
    name: &str,
    run: impl FnOnce(Runtime) + Send + 'static,
) -> io::Result<std::thread::JoinHandle<()>> {
    let (ready, started) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name(name.into())
        .stack_size(4 * 1024 * 1024)
        .spawn(move || match Runtime::new() {
            Ok(runtime) => {
                if ready.send(Ok(())).is_ok() {
                    run(runtime);
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        })?;
    started.recv().map_err(io::Error::other)??;
    Ok(worker)
}

type Work = Box<dyn FnOnce() + Send>;

pub struct SharedWorker(crate::mpsc::UnboundedSender<Work>);

impl SharedWorker {
    pub fn new(name: &str) -> io::Result<Self> {
        let (send, mut requests) = crate::mpsc::unbounded_channel::<Work>();
        worker(name, move |runtime| {
            runtime.block_on(async {
                while let Some(start) = requests.recv().await {
                    start();
                }
            });
        })?;
        Ok(Self(send))
    }

    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let _ = self
            .0
            .send(Box::new(move || compio::runtime::spawn(task).detach()));
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::time::Duration;

    use super::*;

    #[compio::test]
    async fn dropping_a_handle_keeps_the_task_running() {
        let (complete, completed) = crate::oneshot::channel();
        drop(spawn(async move {
            complete.send(()).unwrap();
        }));

        compio::time::timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
    }

    #[compio::test]
    async fn shutting_down_a_join_set_drops_running_tasks() {
        let (started, ready) = crate::oneshot::channel();
        let (guard, cancelled) = crate::oneshot::channel::<()>();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            started.send(()).unwrap();
            let _guard = guard;
            pending::<()>().await;
        });

        compio::time::timeout(Duration::from_secs(1), async {
            ready.await.unwrap();
            tasks.shutdown().await;
            assert!(tasks.is_empty());
            assert!(cancelled.await.is_err());
        })
        .await
        .unwrap();
    }
}
