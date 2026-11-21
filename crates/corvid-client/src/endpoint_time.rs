#[cfg(not(feature = "compio-endpoints"))]
pub(crate) use tokio::time::timeout;

#[cfg(feature = "compio-endpoints")]
/// Apply a deadline using the shared Compio endpoint driver.
pub async fn timeout<F: std::future::Future>(
    duration: std::time::Duration,
    future: F,
) -> anyhow::Result<F::Output> {
    let deadline = std::time::Instant::now() + duration;
    tokio::select! {
        biased;
        result = future => Ok(result),
        timer = wait_until(deadline) => {
            timer?;
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into())
        }
    }
}

/// Wait on the shared Compio driver from an embedding application's executor.
#[cfg(feature = "compio-endpoints")]
pub async fn wait_until(deadline: std::time::Instant) -> anyhow::Result<()> {
    crate::endpoint::native::wait_until(deadline).await
}
