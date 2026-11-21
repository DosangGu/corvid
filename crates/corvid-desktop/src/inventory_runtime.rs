use std::future::Future;
use std::thread;

use anyhow::{Context, Result};

#[cfg(feature = "compio-endpoints")]
pub(crate) use compio::time::{sleep, timeout};
#[cfg(not(feature = "compio-endpoints"))]
pub(crate) use corvid_async::time::{sleep, sleep_until, timeout, timeout_at};

#[cfg(feature = "compio-endpoints")]
pub(crate) async fn sleep_until(deadline: corvid_async::time::Instant) {
    compio::time::sleep_until(deadline).await;
}

#[cfg(feature = "compio-endpoints")]
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
    let (started, startup) = std::sync::mpsc::sync_channel(1);
    let worker = thread::Builder::new().name(name.into()).spawn(move || {
        #[cfg(feature = "compio-endpoints")]
        let runtime = compio::runtime::Runtime::new();
        #[cfg(not(feature = "compio-endpoints"))]
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => {
                if started.send(Ok(())).is_ok() {
                    runtime.block_on(task());
                }
            }
            Err(error) => {
                let _ = started.send(Err(error));
            }
        }
    })?;
    startup
        .recv()
        .context("starting workspace inventory worker")??;
    Ok(worker)
}
