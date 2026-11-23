//! Screen data used by frame acknowledgements and pane composition.

use std::num::NonZeroU16;

use corvid_core::Viewport;

#[derive(Clone)]
pub(crate) struct ScreenSnapshot(pub(super) vt100::Screen);

impl ScreenSnapshot {
    pub(super) fn from_screen(screen: &vt100::Screen) -> Self {
        Self(screen.clone())
    }

    pub(crate) fn from_ansi(viewport: Viewport, ansi: &[u8], clip: Option<Viewport>) -> Self {
        let mut parser = vt100::Parser::new(viewport.rows, viewport.cols, 0);
        parser.process(ansi);
        if let Some(clip) = clip.filter(|clip| *clip != viewport) {
            parser.set_size(clip.rows, clip.cols);
        }
        Self::from_screen(parser.screen())
    }

    pub(crate) fn matches(&self, previous: &Self) -> bool {
        self.0.contents_diff(&previous.0).is_empty()
    }

    pub(crate) fn size(&self) -> (NonZeroU16, NonZeroU16) {
        self.0.size()
    }

    pub(crate) fn cursor_position(&self) -> (u16, u16) {
        self.0.cursor_position()
    }

    pub(crate) fn application_cursor(&self) -> bool {
        self.0.application_cursor()
    }

    pub(crate) fn application_keypad(&self) -> bool {
        self.0.application_keypad()
    }

    pub(crate) fn hide_cursor(&self) -> bool {
        self.0.hide_cursor()
    }

    pub(crate) fn cell(&self, row: u16, column: u16) -> Option<ScreenCell<'_>> {
        let cell = self.0.cell(row, column)?;
        let width = if cell.is_wide_continuation() {
            CellWidth::Continuation
        } else if cell.is_wide() {
            CellWidth::Wide
        } else {
            CellWidth::Single
        };
        Some(ScreenCell {
            text: cell.contents(),
            width,
            style: CellStyle {
                fg: cell.fgcolor().into(),
                bg: cell.bgcolor().into(),
                flags: [
                    cell.bold(),
                    cell.dim(),
                    cell.italic(),
                    cell.underline(),
                    cell.inverse(),
                ],
            },
        })
    }
}

pub(crate) struct ScreenCell<'a> {
    pub(crate) text: &'a str,
    pub(crate) width: CellWidth,
    pub(crate) style: CellStyle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellWidth {
    Single,
    Wide,
    Continuation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellStyle {
    pub(crate) fg: CellColor,
    pub(crate) bg: CellColor,
    pub(crate) flags: [bool; 5],
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for CellColor {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(index) => Self::Indexed(index),
            vt100::Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}
