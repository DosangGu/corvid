use std::num::NonZeroU16;

use thiserror::Error;
use uuid::Uuid;

pub mod agent;
pub mod layout;

pub const KEY_LEADER: u8 = 0x1d;
pub const PASTE_START: &[u8; 6] = b"\x1b[200~";
pub const PASTE_END: &[u8; 6] = b"\x1b[201~";
pub const MAX_PASTE_BYTES: usize = 1024 * 1024;
pub const PASTE_CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const MAX_SCREEN_CELLS: u16 = 36_000;
// The emulator retains scrollback by row, so its memory also depends on width.
const MAX_SCREEN_COLS: u16 = 1024;

#[derive(Clone, Copy, Debug, Error)]
#[error("viewport must have at least 2 columns and 2 rows")]
pub struct InvalidViewport;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub rows: NonZeroU16,
    pub cols: NonZeroU16,
}

impl Viewport {
    pub fn new(cols: u16, rows: u16) -> Result<Self, InvalidViewport> {
        if cols < 2 || rows < 2 {
            return Err(InvalidViewport);
        }

        Ok(Self {
            rows: NonZeroU16::new(rows).ok_or(InvalidViewport)?,
            cols: NonZeroU16::new(cols).ok_or(InvalidViewport)?,
        })
    }

    /// Fits a physical viewport into the PTY's screen and scrollback budget.
    pub fn fit_screen(self) -> Self {
        let cols = self.cols.get().min(MAX_SCREEN_COLS);
        let rows = self.rows.get().min(MAX_SCREEN_CELLS / cols);
        Self::new(cols, rows).expect("screen budget permits at least two rows and columns")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputRoute {
    pub client_id: Uuid,
    pub connection_generation: u64,
    pub view_revision: u64,
    pub terminal_id: Uuid,
    pub writer_lease_generation: u64,
}

#[cfg(test)]
mod tests {
    use super::Viewport;

    #[test]
    fn preserves_viewports_that_fit_and_bounds_oversized_screens() {
        for (cols, rows) in [(319, 80), (100, 200), (300, 120)] {
            let viewport = Viewport::new(cols, rows).unwrap();
            assert_eq!(viewport.fit_screen(), viewport);
        }
        assert_eq!(
            Viewport::new(500, 100).unwrap().fit_screen(),
            Viewport::new(500, 72).unwrap()
        );
        assert_eq!(
            Viewport::new(u16::MAX, u16::MAX).unwrap().fit_screen(),
            Viewport::new(1024, 35).unwrap()
        );
        assert!(Viewport::new(1, 80).is_err());
        assert!(Viewport::new(80, 0).is_err());
    }
}
