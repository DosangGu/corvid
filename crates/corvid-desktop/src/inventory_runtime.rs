use std::future::Future;
use std::thread;

use anyhow::{Context, Result};

pub(crate) use compio::time::{sleep, timeout};

pub(crate) async fn sleep_until(deadline: corvid_async::time::Instant) {
    compio::time::sleep_until(deadline).await;
}

pub(crate) async fn timeout_at<F: Future>(
    deadline: corvid_async::time::Instant,
    future: F,
) -> Result<F::Output, compio::time::Elapsed> {
    compio::time::timeout(
        deadline.saturating_duration_since(corvid_async::time::Instant::now()),
        future,
    )
    .await
}

pub(crate) fn start<F: Future<Output = ()> + 'static>(
    name: &str,
    task: impl FnOnce() -> F + Send + 'static,
) -> Result<thread::JoinHandle<()>> {
    corvid_async::task::worker(name, move |runtime| runtime.block_on(task()))
        .context("starting workspace inventory worker")
}
