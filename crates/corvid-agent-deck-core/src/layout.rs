use serde::{Deserialize, Serialize};

pub const KEY_WIDTH: f32 = 86.0;
pub const KEY_HEIGHT: f32 = 80.0;
pub const GAP: f32 = 8.0;
pub const BAR_HEIGHT: f32 = 36.0;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
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
pub enum Orientation {
    #[default]
    Landscape,
    Portrait,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct GridLayout {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_preserves_capacity_and_swaps_axes() {
        for (preset, (capacity, landscape, portrait)) in Preset::ALL.into_iter().zip([
            (6, "3 × 2", "2 × 3"),
            (8, "4 × 2", "2 × 4"),
            (12, "4 × 3", "3 × 4"),
            (15, "5 × 3", "3 × 5"),
        ]) {
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
            assert_eq!(wide.label(), landscape);
            assert_eq!(tall.label(), portrait);
        }
    }
}
