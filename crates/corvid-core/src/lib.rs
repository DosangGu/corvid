use std::num::NonZeroU16;

use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Error)]
#[error("viewport must be between 2×2 and 300×120 cells")]
pub struct InvalidViewport;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub rows: NonZeroU16,
    pub cols: NonZeroU16,
}

impl Viewport {
    pub fn new(cols: u16, rows: u16) -> Result<Self, InvalidViewport> {
        if !(2..=300).contains(&cols) || !(2..=120).contains(&rows) {
            return Err(InvalidViewport);
        }

        Ok(Self {
            rows: NonZeroU16::new(rows).ok_or(InvalidViewport)?,
            cols: NonZeroU16::new(cols).ok_or(InvalidViewport)?,
        })
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
