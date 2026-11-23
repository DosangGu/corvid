//! Screen data used by frame acknowledgements and pane composition.

use std::fmt::Write;
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

    #[cfg(all(test, windows))]
    pub(crate) fn contents(&self) -> String {
        self.0.contents()
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
        self.0.cell(row, column).map(ScreenCell::from)
    }
}

pub(crate) struct ScreenCell<'a> {
    pub(crate) text: &'a str,
    pub(crate) width: CellWidth,
    pub(crate) style: CellStyle,
}

impl<'a> From<&'a vt100::Cell> for ScreenCell<'a> {
    fn from(cell: &'a vt100::Cell) -> Self {
        let width = if cell.is_wide_continuation() {
            CellWidth::Continuation
        } else if cell.is_wide() {
            CellWidth::Wide
        } else {
            CellWidth::Single
        };
        Self {
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
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellWidth {
    Single,
    Wide,
    Continuation,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CellStyle {
    pub(crate) fg: CellColor,
    pub(crate) bg: CellColor,
    pub(crate) flags: [bool; 5],
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CellColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl CellStyle {
    pub(crate) fn paint(self, ansi: &mut String) {
        ansi.push_str("\x1b[");
        self.write_sgr(ansi);
        ansi.push('m');
    }

    pub(super) fn write_sgr(self, sgr: &mut String) {
        sgr.push('0');
        for (enabled, parameter) in self.flags.into_iter().zip([1, 2, 3, 4, 7]) {
            if enabled {
                write!(sgr, ";{parameter}").unwrap();
            }
        }
        for (prefix, color) in [(38, self.fg), (48, self.bg)] {
            match color {
                CellColor::Default => {}
                CellColor::Indexed(index) => write!(sgr, ";{prefix};5;{index}").unwrap(),
                CellColor::Rgb(r, g, b) => write!(sgr, ";{prefix};2;{r};{g};{b}").unwrap(),
            }
        }
    }
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
