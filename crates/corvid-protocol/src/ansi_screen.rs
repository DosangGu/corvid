//! Owned text frames for the bounded ANSI render profile.

use std::fmt::Write;
use std::num::NonZeroU16;
use std::sync::Arc;

use smol_str::SmolStr;

mod decoder;

pub use decoder::Decoder;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Style {
    pub fn paint(self, ansi: &mut String) {
        ansi.push_str("\x1b[");
        self.write_sgr(ansi);
        ansi.push('m');
    }

    pub fn write_sgr(self, sgr: &mut String) {
        sgr.push('0');
        for (enabled, parameter) in [
            (self.bold, 1),
            (self.dim, 2),
            (self.italic, 3),
            (self.underline, 4),
            (self.inverse, 7),
        ] {
            if enabled {
                write!(sgr, ";{parameter}").unwrap();
            }
        }
        for (prefix, color) in [(38, self.fg), (48, self.bg)] {
            match color {
                Color::Default => {}
                Color::Indexed(index) => write!(sgr, ";{prefix};5;{index}").unwrap(),
                Color::Rgb(r, g, b) => write!(sgr, ";{prefix};2;{r};{g};{b}").unwrap(),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Width {
    #[default]
    Single,
    Wide,
    Continuation,
}

pub const MAX_HYPERLINK_URI_BYTES: usize = 2048;
pub const MAX_HYPERLINK_ID_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hyperlink {
    pub uri: String,
    pub id: Option<String>,
}

pub fn sanitize_hyperlink(uri: &str, id: Option<&str>) -> Option<Hyperlink> {
    if uri.is_empty() || uri.len() > MAX_HYPERLINK_URI_BYTES || uri.chars().any(is_unsafe_char) {
        return None;
    }
    let id = match id.filter(|id| !id.is_empty()) {
        Some(id) => {
            if id.len() > MAX_HYPERLINK_ID_BYTES
                || id
                    .chars()
                    .any(|ch| is_unsafe_char(ch) || ch == ';' || ch == ':')
            {
                return None;
            }
            Some(id.to_owned())
        }
        None => None,
    };
    Some(Hyperlink {
        uri: uri.to_owned(),
        id,
    })
}

fn is_unsafe_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    text: SmolStr,
    pub width: Width,
    pub style: Style,
    hyperlink: Option<Arc<Hyperlink>>,
}

impl Cell {
    pub fn new(text: &str, width: Width, style: Style) -> Self {
        Self {
            text: SmolStr::new(text),
            width,
            style,
            hyperlink: None,
        }
    }

    pub fn with_hyperlink(mut self, hyperlink: Option<Arc<Hyperlink>>) -> Self {
        self.hyperlink = hyperlink;
        self
    }

    pub fn hyperlink(&self) -> Option<&Hyperlink> {
        self.hyperlink.as_deref()
    }

    pub fn hyperlink_arc(&self) -> Option<&Arc<Hyperlink>> {
        self.hyperlink.as_ref()
    }

    pub fn contents(&self) -> &str {
        &self.text
    }

    pub fn has_contents(&self) -> bool {
        !self.text.is_empty()
    }

    pub fn is_wide(&self) -> bool {
        self.width == Width::Wide
    }

    pub fn is_wide_continuation(&self) -> bool {
        self.width == Width::Continuation
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub wrap_pending: bool,
    pub hidden: bool,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    cells: Vec<Cell>,
    wrapped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    rows: Vec<Arc<Row>>,
    cols: NonZeroU16,
    pub cursor: Cursor,
    pub application_cursor: bool,
    pub application_keypad: bool,
}

impl Screen {
    pub fn new(rows: NonZeroU16, cols: NonZeroU16) -> Self {
        let row = Arc::new(Row {
            cells: vec![Cell::default(); usize::from(cols.get())],
            wrapped: false,
        });
        Self {
            rows: vec![row; usize::from(rows.get())],
            cols,
            cursor: Cursor::default(),
            application_cursor: false,
            application_keypad: false,
        }
    }

    pub fn size(&self) -> (NonZeroU16, NonZeroU16) {
        (NonZeroU16::new(self.rows.len() as u16).unwrap(), self.cols)
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.row(row)?.get(usize::from(col))
    }

    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        self.rows
            .get(usize::from(row))
            .map(|row| row.cells.as_slice())
    }

    pub fn replace_row(&mut self, row: u16, cells: Vec<Cell>, wrapped: bool) {
        debug_assert_eq!(cells.len(), usize::from(self.cols.get()));
        self.rows[usize::from(row)] = Arc::new(Row { cells, wrapped });
    }

    pub fn same_row(&self, other: &Self, row: u16) -> bool {
        let index = usize::from(row);
        match (self.rows.get(index), other.rows.get(index)) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }

    pub fn row_wrapped(&self, row: u16) -> bool {
        self.rows
            .get(usize::from(row))
            .is_some_and(|row| row.wrapped)
    }

    pub fn set_row_wrapped(&mut self, row: u16, wrapped: bool) {
        if let Some(row) = self.rows.get_mut(usize::from(row)) {
            Arc::make_mut(row).wrapped = wrapped;
        }
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        (
            self.cursor.row,
            self.cursor.col + u16::from(self.cursor.wrap_pending),
        )
    }

    pub fn hide_cursor(&self) -> bool {
        self.cursor.hidden
    }

    pub fn contents(&self) -> String {
        let rows = self.size().0.get();
        let mut text = self.contents_between(0, 0, rows - 1, self.cols.get());
        text.truncate(text.trim_end_matches('\n').len());
        text
    }

    pub fn contents_between(
        &self,
        first_row: u16,
        first_col: u16,
        last_row: u16,
        last_col: u16,
    ) -> String {
        let mut text = String::new();
        for index in first_row..=last_row.min(self.size().0.get() - 1) {
            let row = &self.rows[usize::from(index)];
            let from = if index == first_row { first_col } else { 0 }.min(self.cols.get());
            let to = if index == last_row {
                last_col
            } else {
                self.cols.get()
            }
            .min(self.cols.get());
            let mut blanks = 0;
            for column in from..to {
                let cell = &row.cells[usize::from(column)];
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.has_contents() {
                    text.extend(std::iter::repeat_n(' ', blanks));
                    blanks = 0;
                    text.push_str(cell.contents());
                } else {
                    blanks += 1;
                }
            }
            if index < last_row && !row.wrapped {
                text.push('\n');
            }
        }
        text
    }

    /// Clip a decoded frame to the visible preview rectangle.
    pub fn clip(&mut self, rows: NonZeroU16, cols: NonZeroU16) {
        self.rows.truncate(usize::from(rows.get()));
        if cols < self.cols {
            for row in &mut self.rows {
                let row = Arc::make_mut(row);
                row.cells.truncate(usize::from(cols.get()));
                if let Some(last) = row.cells.last_mut().filter(|cell| cell.is_wide()) {
                    *last = Cell::default();
                }
                row.wrapped = false;
            }
            self.cols = cols;
        }
        if self.cursor.col >= self.cols.get() {
            self.cursor.col = self.cols.get() - 1;
            self.cursor.wrap_pending = false;
        }
        self.cursor.row = self.cursor.row.min(self.size().0.get() - 1);
    }
}
