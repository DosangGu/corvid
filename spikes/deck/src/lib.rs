pub mod art;
#[cfg(all(windows, feature = "desktop"))]
pub mod deck;
#[cfg(all(windows, feature = "desktop"))]
pub mod icons;
mod keys;
#[cfg(feature = "live")]
pub mod live;

use std::collections::VecDeque;

pub use keys::{Behavior, COLUMNS, KEY_COUNT, Key, PAGES, Page, Tone};

const HISTORY_LIMIT: usize = 5;

#[derive(Default)]
pub struct Deck {
    page: usize,
    layout_revision: usize,
    toggles: [[bool; KEY_COUNT]; PAGES.len()],
    history: VecDeque<Press>,
    press_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pressed,
    Enabled,
    Disabled,
    PageChanged,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pressed => "Pressed",
            Self::Enabled => "Enabled",
            Self::Disabled => "Disabled",
            Self::PageChanged => "Page changed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub page: usize,
    pub index: usize,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyFace {
    pub key: Key,
    pub enabled: Option<bool>,
    pub latest: bool,
}

impl KeyFace {
    pub fn highlighted(self) -> bool {
        self.enabled == Some(true) || self.latest
    }
}

#[derive(Clone, Copy)]
pub struct Frame {
    pub page: usize,
    pub layout_revision: usize,
    pub keys: [KeyFace; KEY_COUNT],
}

#[derive(Clone, Copy)]
pub struct KeyPress {
    layout_revision: usize,
    index: usize,
}

impl Frame {
    pub fn capture(&self, index: usize) -> Option<KeyPress> {
        (index < self.keys.len()).then_some(KeyPress {
            layout_revision: self.layout_revision,
            index,
        })
    }
}

impl Deck {
    pub fn frame(&self) -> Frame {
        let latest = self.history.front().filter(|press| press.page == self.page);
        let keys = std::array::from_fn(|index| {
            let key = PAGES[self.page].keys[index];
            KeyFace {
                key,
                enabled: (key.behavior == Behavior::Toggle)
                    .then_some(self.toggles[self.page][index]),
                latest: latest.is_some_and(|press| press.index == index),
            }
        });
        Frame {
            page: self.page,
            layout_revision: self.layout_revision,
            keys,
        }
    }

    pub fn history(&self) -> &VecDeque<Press> {
        &self.history
    }

    pub fn press_count(&self) -> usize {
        self.press_count
    }

    pub fn select_page(&mut self, page: usize) -> bool {
        if page >= PAGES.len() || page == self.page {
            return false;
        }
        self.page = page;
        self.layout_revision += 1;
        true
    }

    pub fn shift_page(&mut self, forward: bool) -> bool {
        let offset = if forward { 1 } else { PAGES.len() - 1 };
        self.select_page((self.page + offset) % PAGES.len())
    }

    pub fn press(&mut self, capture: KeyPress) -> Option<Press> {
        if capture.layout_revision != self.layout_revision {
            return None;
        }

        let page = self.page;
        let index = capture.index;
        let outcome = match PAGES[page].keys[index].behavior {
            Behavior::Momentary => Outcome::Pressed,
            Behavior::Toggle => {
                let enabled = &mut self.toggles[page][index];
                *enabled = !*enabled;
                if *enabled {
                    Outcome::Enabled
                } else {
                    Outcome::Disabled
                }
            }
            Behavior::NextPage => {
                self.shift_page(true);
                Outcome::PageChanged
            }
        };
        let press = Press {
            page,
            index,
            outcome,
        };
        self.history.push_front(press);
        self.history.truncate(HISTORY_LIMIT);
        self.press_count += 1;
        Some(press)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_key_captured_before_a_page_change() {
        let mut deck = Deck::default();
        let press = deck.frame().capture(0).unwrap();

        assert!(deck.select_page(1));
        assert!(deck.select_page(0));
        assert_eq!(deck.press(press), None);
        assert_eq!(deck.press_count(), 0);
        assert!(deck.frame().capture(KEY_COUNT).is_none());
    }

    #[test]
    fn on_deck_navigation_preserves_page_local_toggles() {
        let mut deck = Deck::default();
        let workspace = deck.frame();
        let focus = deck.press(workspace.capture(0).unwrap()).unwrap();
        assert_eq!(focus.outcome, Outcome::Enabled);
        let next = deck
            .press(workspace.capture(KEY_COUNT - 1).unwrap())
            .unwrap();
        assert_eq!(next.outcome, Outcome::PageChanged);
        let system = deck.frame();
        assert_eq!(system.page, 1);
        assert_eq!(system.keys[0].enabled, None);

        deck.press(system.capture(KEY_COUNT - 1).unwrap()).unwrap();
        assert_eq!(deck.frame().keys[0].enabled, Some(true));
        assert_eq!(deck.press_count(), 3);
    }

    #[test]
    fn every_key_has_a_device_neutral_glyph() {
        for page in PAGES {
            assert!(art::outline(page.glyph).is_some());
            for key in page.keys {
                assert!(art::outline(key.glyph).is_some(), "{}", key.glyph);
            }
        }
    }
}
