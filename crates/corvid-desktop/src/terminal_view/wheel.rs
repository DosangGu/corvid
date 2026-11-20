use std::time::{Duration, Instant};

use gpui::TouchPhase;

use crate::terminal_session::WheelTarget;

// Windows and X11 expose wheel events without a start/end pair. macOS can
// deliver momentum as Moved after Ended, so retain ownership through that tail.
const GESTURE_GAP: Duration = Duration::from_millis(180);

#[derive(Default)]
pub(super) struct WheelGesture {
    last: Option<Instant>,
    route: Option<(u64, WheelTarget)>,
    remainder: f32,
    finished: bool,
    touching: bool,
}

impl WheelGesture {
    pub(super) fn rows(
        &mut self,
        now: Instant,
        phase: TouchPhase,
        route: (u64, WheelTarget),
        delta: f32,
    ) -> Option<isize> {
        if phase == TouchPhase::Cancelled {
            *self = Self::default();
            return None;
        }
        if matches!(phase, TouchPhase::Started)
            || !self.touching
                && self
                    .last
                    .is_none_or(|last| now.duration_since(last) >= GESTURE_GAP)
        {
            self.route = Some(route);
            self.remainder = 0.;
            self.finished = false;
        }
        match phase {
            TouchPhase::Started => self.touching = true,
            TouchPhase::Ended | TouchPhase::Cancelled => self.touching = false,
            TouchPhase::Moved => {}
        }
        self.last = Some(now);
        if self.route != Some(route) || self.finished {
            self.finished = true;
            return None;
        }
        self.remainder += delta;
        let rows = self.remainder.trunc();
        self.remainder -= rows;
        (rows != 0.).then_some(rows.clamp(-32767., 32767.) as isize)
    }

    pub(super) fn finish_at_bottom(&mut self) {
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn momentum_cannot_reach_the_application_after_leaving_history() {
        let mut wheel = WheelGesture::default();
        let start = Instant::now();
        let history = (1, WheelTarget::Scrollback);
        let app = (1, WheelTarget::Cursor { application: true });
        assert_eq!(wheel.rows(start, TouchPhase::Started, history, 1.), Some(1));
        wheel.finish_at_bottom();
        assert_eq!(wheel.rows(start, TouchPhase::Ended, app, 0.), None);
        assert_eq!(wheel.rows(start, TouchPhase::Moved, app, 2.), None);
        assert_eq!(wheel.rows(start, TouchPhase::Started, app, 2.), Some(2));
    }

    #[test]
    fn fractional_wheel_steps_keep_their_route_until_the_next_burst() {
        let mut wheel = WheelGesture::default();
        let now = Instant::now();
        let history = (1, WheelTarget::Scrollback);
        let other = (2, WheelTarget::Scrollback);
        assert_eq!(wheel.rows(now, TouchPhase::Moved, history, -0.5), None);
        assert_eq!(wheel.rows(now, TouchPhase::Moved, history, -0.75), Some(-1));
        assert_eq!(wheel.rows(now, TouchPhase::Moved, other, 2.), None);
        assert_eq!(
            wheel.rows(now + GESTURE_GAP, TouchPhase::Moved, other, 1.),
            Some(1)
        );
    }
}
