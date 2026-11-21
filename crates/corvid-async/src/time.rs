//! Deadlines that can be polled across executor and thread boundaries.

use std::fmt;
use std::time::Duration;

use async_io::Timer;

pub use std::time::Instant;

pub fn sleep(duration: Duration) -> impl Future<Output = ()> + Send {
    sleep_until(Instant::now() + duration)
}

pub async fn sleep_until(deadline: Instant) {
    Timer::at(deadline).await;
}

#[derive(Debug, Clone, Copy)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    timeout_at(Instant::now() + duration, future).await
}

pub async fn timeout_at<F: Future>(deadline: Instant, future: F) -> Result<F::Output, Elapsed> {
    crate::select! {
        biased;
        output = future => Ok(output),
        () = sleep_until(deadline) => Err(Elapsed),
    }
}

#[derive(Clone, Copy)]
pub enum MissedTickBehavior {
    Burst,
    Delay,
}

pub struct Interval {
    next: Instant,
    period: Duration,
    behavior: MissedTickBehavior,
}

pub fn interval(period: Duration) -> Interval {
    interval_at(Instant::now(), period)
}

pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(!period.is_zero(), "interval period must be positive");
    Interval {
        next: start,
        period,
        behavior: MissedTickBehavior::Burst,
    }
}

impl Interval {
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.behavior = behavior;
    }

    pub async fn tick(&mut self) -> Instant {
        let scheduled = self.next;
        sleep_until(scheduled).await;
        let now = Instant::now();
        let start = match self.behavior {
            MissedTickBehavior::Delay if now > scheduled + Duration::from_millis(5) => now,
            _ => scheduled,
        };
        self.next = start + self.period;
        scheduled
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future::{block_on, poll_once};

    use super::*;

    #[test]
    fn a_cancelled_tick_keeps_the_original_deadline() {
        block_on(async {
            let mut ticker = interval(Duration::from_secs(1));
            let first = ticker.tick().await;
            assert!(poll_once(ticker.tick()).await.is_none());
            assert_eq!(ticker.next, first + Duration::from_secs(1));
        });
    }

    #[test]
    fn an_elapsed_deadline_keeps_a_ready_result_and_cancels_pending_work() {
        block_on(async {
            let deadline = Instant::now();
            assert_eq!(timeout_at(deadline, async { 7 }).await.unwrap(), 7);
            assert!(
                timeout_at(deadline, std::future::pending::<()>())
                    .await
                    .is_err()
            );
        });
    }
}
