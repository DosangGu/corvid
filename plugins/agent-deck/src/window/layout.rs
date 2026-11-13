use gpui::{Pixels, Size, px, size};
use serde::{Deserialize, Serialize};

pub(crate) const KEY_WIDTH: f32 = 86.0;
pub(crate) const KEY_HEIGHT: f32 = 80.0;
pub(crate) const GAP: f32 = 8.0;
pub(crate) const BAR_HEIGHT: f32 = 36.0;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Preset {
    Six,
    #[default]
    Eight,
    Twelve,
    Fifteen,
}

impl Preset {
    pub const ALL: [Self; 4] = [Self::Six, Self::Eight, Self::Twelve, Self::Fifteen];

    fn landscape(self) -> (usize, usize) {
        match self {
            Self::Six => (3, 2),
            Self::Eight => (4, 2),
            Self::Twelve => (4, 3),
            Self::Fifteen => (5, 3),
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Orientation {
    #[default]
    Landscape,
    Portrait,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct GridLayout {
    pub preset: Preset,
    pub orientation: Orientation,
}

impl GridLayout {
    pub fn dimensions(self) -> (usize, usize) {
        let (columns, rows) = self.preset.landscape();
        match self.orientation {
            Orientation::Landscape => (columns, rows),
            Orientation::Portrait => (rows, columns),
        }
    }

    pub fn capacity(self) -> usize {
        let (columns, rows) = self.dimensions();
        columns * rows
    }

    pub fn label(self) -> String {
        let (columns, rows) = self.dimensions();
        format!("{columns} × {rows}")
    }

    pub fn size(self) -> Size<Pixels> {
        let (columns, rows) = self.dimensions();
        size(
            px(24.0 + columns as f32 * KEY_WIDTH + (columns - 1) as f32 * GAP),
            px(2.0 * BAR_HEIGHT + 16.0 + rows as f32 * KEY_HEIGHT + (rows - 1) as f32 * GAP),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_preserves_capacity_and_swaps_axes() {
        for (preset, capacity) in Preset::ALL.into_iter().zip([6, 8, 12, 15]) {
            let wide = GridLayout {
                preset,
                orientation: Orientation::Landscape,
            };
            let tall = GridLayout {
                orientation: Orientation::Portrait,
                ..wide
            };
            let (columns, rows) = wide.dimensions();
            assert_eq!(tall.dimensions(), (rows, columns));
            assert_eq!(wide.capacity(), capacity);
            assert_eq!(tall.capacity(), capacity);
            assert!(wide.size().width > wide.size().height);
            assert!(tall.size().height > tall.size().width);
        }
    }
}
