use std::num::NonZeroU16;

use anyhow::{Result, ensure};
use portable_pty::PtySize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub rows: NonZeroU16,
    pub cols: NonZeroU16,
}

impl Viewport {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        ensure!(
            (2..=300).contains(&cols) && (2..=120).contains(&rows),
            "viewport must be between 2×2 and 300×120 cells"
        );

        Ok(Self {
            rows: NonZeroU16::try_from(rows)?,
            cols: NonZeroU16::try_from(cols)?,
        })
    }
}

impl From<Viewport> for PtySize {
    fn from(viewport: Viewport) -> Self {
        Self {
            rows: viewport.rows.get(),
            cols: viewport.cols.get(),
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}
