use std::time::Duration;

pub(crate) use corvid_async::time::Instant;

pub(crate) async fn sleep_until(deadline: Instant) {
    compio::time::sleep_until(deadline).await;
}

pub(crate) async fn timeout_at<F: std::future::Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, compio::time::Elapsed> {
    compio::time::timeout(deadline.saturating_duration_since(Instant::now()), future).await
}

pub(crate) fn interval_at(start: Instant, period: Duration) -> Interval {
    Interval {
        next: start,
        period,
    }
}

pub(crate) struct Interval {
    next: Instant,
    period: Duration,
}

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
