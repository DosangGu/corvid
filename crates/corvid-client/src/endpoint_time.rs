#[cfg(not(feature = "compio-endpoints"))]
pub(crate) use tokio::time::timeout;

#[cfg(feature = "compio-endpoints")]
pub(crate) async fn timeout<F: std::future::Future>(
    duration: std::time::Duration,
    future: F,
) -> anyhow::Result<F::Output> {
    let deadline = std::time::Instant::now() + duration;
    tokio::select! {
        biased;
        result = future => Ok(result),
        timer = crate::endpoint::native::wait_until(deadline) => {
            timer?;
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into())
        }
    }
}
