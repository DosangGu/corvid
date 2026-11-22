use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

const PAUSE: Duration = Duration::from_secs(1);
const CELL_INTERVAL: Duration = Duration::from_millis(125);

pub(super) type TitleTarget<'a> = (Uuid, &'a str, usize);

#[derive(Default)]
pub(super) struct TitleMotion(Option<Title>);

struct Title {
    id: Uuid,
    text: String,
    width: usize,
    travel: usize,
    started: Instant,
    offset: usize,
    visible: String,
}

impl TitleMotion {
    pub(super) fn update(&mut self, target: Option<TitleTarget<'_>>, now: Instant) -> bool {
        let Some((id, text, width)) = target else {
            return self.0.take().is_some();
        };
        if let Some(title) = &mut self.0
            && (title.id, title.text.as_str(), title.width) == (id, text, width)
        {
            let offset = title_offset(title.travel, now.saturating_duration_since(title.started));
            if title.offset == offset {
                return false;
            }
            title.offset = offset;
            let visible = window(text, width, offset);
            if visible == title.visible {
                return false;
            }
            title.visible = visible;
        } else {
            self.0 = Some(Title {
                id,
                text: text.to_owned(),
                width,
                travel: text.width().saturating_sub(width),
                started: now,
                offset: 0,
                visible: window(text, width, 0),
            });
        }
        true
    }

    pub(super) fn visible(&self) -> &str {
        self.0.as_ref().map_or("", |title| title.visible.as_str())
    }
}

fn title_offset(travel: usize, elapsed: Duration) -> usize {
    if travel == 0 {
        return 0;
    }
    let interval = CELL_INTERVAL.as_millis();
    let leg = PAUSE.as_millis() + travel as u128 * interval;
    let phase = elapsed.as_millis() % (2 * leg);
    let progress = ((phase % leg).saturating_sub(PAUSE.as_millis()) / interval) as usize;
    if phase < leg {
        progress
    } else {
        travel - progress
    }
}

pub(super) fn window(text: &str, mut width: usize, mut offset: usize) -> String {
    let mut visible = String::new();
    for grapheme in text.graphemes(true) {
        if grapheme
            .chars()
            .any(|ch| ch.is_control() || super::screen::is_bidi_control(ch))
        {
            continue;
        }
        let cells = grapheme.width();
        if offset >= cells {
            offset -= cells;
            continue;
        }
        if offset > 0 {
            let clipped = (cells - offset).min(width);
            visible.extend(std::iter::repeat_n(' ', clipped));
            width -= clipped;
            offset = 0;
            continue;
        }
        if cells > width {
            break;
        }
        visible.push_str(grapheme);
        width -= cells;
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_at_both_edges_before_returning() {
        for (millis, offset) in [
            (0, 0),
            (999, 0),
            (1125, 1),
            (1500, 4),
            (2499, 4),
            (2750, 2),
            (3000, 0),
        ] {
            assert_eq!(title_offset(4, Duration::from_millis(millis)), offset);
        }
        assert_eq!(title_offset(0, Duration::from_secs(5)), 0);
    }

    #[test]
    fn clips_full_graphemes_without_wrapping_or_changing_width() {
        let title = "한e\u{301}👩‍💻끝";
        assert_eq!(window(title, 3, 0), "한e\u{301}");
        assert_eq!(window(title, 3, 1), " e\u{301}");
        assert_eq!(window(title, 3, 2), "e\u{301}👩‍💻");
        assert_eq!(window(title, 3, 4), " 끝");
    }

    #[test]
    fn changing_the_selection_or_width_restarts_at_the_prefix() {
        let start = Instant::now();
        let mut motion = TitleMotion::default();
        let id = Uuid::new_v4();
        assert!(motion.update(Some((id, "project/review", 7)), start));
        assert!(!motion.update(Some((id, "project/review", 7)), start + PAUSE));
        let later = start + PAUSE + CELL_INTERVAL;
        assert!(motion.update(Some((id, "project/review", 7)), later));
        assert_eq!(motion.visible(), "roject/");
        assert!(motion.update(Some((id, "project/review", 8)), later));
        assert_eq!(motion.visible(), "project/");
        assert!(motion.update(Some((Uuid::new_v4(), "project/review", 8)), later));
        assert_eq!(motion.visible(), "project/");
    }
}
