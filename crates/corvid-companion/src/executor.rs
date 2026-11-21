use std::time::Duration;

pub(crate) use tokio::time::Instant;

#[cfg(not(feature = "compio-endpoints"))]
pub(crate) use tokio::time::{sleep_until, timeout_at};

#[cfg(feature = "compio-endpoints")]
pub(crate) async fn sleep_until(deadline: Instant) {
    compio::time::sleep_until(deadline.into_std()).await;
}

#[cfg(feature = "compio-endpoints")]
pub(crate) async fn timeout_at<F: std::future::Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, compio::time::Elapsed> {
    compio::time::timeout(deadline.saturating_duration_since(Instant::now()), future).await
}

#[cfg(not(feature = "compio-endpoints"))]
pub(crate) fn interval_at(start: Instant, period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(start, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

#[cfg(feature = "compio-endpoints")]
pub(crate) fn interval_at(start: Instant, period: Duration) -> Interval {
    Interval {
        next: start,
        period,
    }
}

#[cfg(feature = "compio-endpoints")]
pub(crate) struct Interval {
    next: Instant,
    period: Duration,
}

#[cfg(feature = "compio-endpoints")]
impl Interval {
    pub(crate) async fn tick(&mut self) {
        sleep_until(self.next).await;
        let now = Instant::now();
        // Match the delay policy and clock tolerance of the original interval.
        if now > self.next + Duration::from_millis(5) {
            self.next = now;
        }
        self.next += self.period;
    }
}
