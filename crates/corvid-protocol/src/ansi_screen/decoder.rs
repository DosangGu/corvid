use std::num::NonZeroU16;
use std::ops::Range;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vte::{Params, Parser, Perform};

use super::{Cell, Color, Cursor, Hyperlink, Row, Screen, Style, Width, sanitize_hyperlink};

const MAX_GRAPHEME_BYTES: usize = 96;

/// Apply the text commands emitted by the current and legacy ANSI frame encoders.
/// OSC 8 hyperlinks are recorded on cells; other OSC, DCS, and queries have no side effects.
pub struct Decoder {
    parser: Parser<4096>,
    state: DecodeState,
}

impl Decoder {
    pub fn new(rows: NonZeroU16, cols: NonZeroU16) -> Self {
        Self {
            parser: Parser::new_with_size(),
            state: DecodeState {
                screen: Screen::new(rows, cols),
                saved: Cursor::default(),
                margins: 0..rows.get(),
                wrap: true,
                origin: false,
                active_hyperlink: None,
            },
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    pub fn screen(&self) -> &Screen {
        &self.state.screen
    }
}

struct DecodeState {
    screen: Screen,
    saved: Cursor,
    margins: Range<u16>,
    wrap: bool,
    origin: bool,
    active_hyperlink: Option<Arc<Hyperlink>>,
}

impl DecodeState {
    fn cols(&self) -> u16 {
        self.screen.cols.get()
    }

    fn rows(&self) -> u16 {
        self.screen.size().0.get()
    }

    fn row(&mut self, index: u16) -> &mut Row {
        Arc::make_mut(&mut self.screen.rows[usize::from(index)])
    }

    fn go_to(&mut self, row: u16, col: u16) {
        self.screen.cursor.row = row.min(self.rows() - 1);
        self.screen.cursor.col = col.min(self.cols() - 1);
        self.screen.cursor.wrap_pending = false;
    }

    fn position(&mut self, row: u16, col: u16) {
        let row = if self.origin {
            row.saturating_add(self.margins.start)
                .min(self.margins.end - 1)
        } else {
            row
        };
        self.go_to(row, col);
    }

    fn set_cursor_after(&mut self, col: u16) {
        self.screen.cursor.col = col.min(self.cols() - 1);
        self.screen.cursor.wrap_pending = self.wrap && col >= self.cols();
    }

    fn blank(&self) -> Cell {
        Cell::new(
            "",
            Width::Single,
            Style {
                bg: self.screen.cursor.style.bg,
                ..Style::default()
            },
        )
    }

    fn clear(&mut self, row: u16, mut range: Range<u16>) {
        if range.is_empty() {
            return;
        }
        let blank = self.blank();
        let cols = self.cols();
        let row = self.row(row);
        if row.cells[usize::from(range.start)].is_wide_continuation() {
            range.start = range.start.saturating_sub(1);
        }
        if range.end < cols && row.cells[usize::from(range.end)].is_wide_continuation() {
            range.end += 1;
        }
        if range.end == cols {
            row.wrapped = false;
        }
        row.cells[usize::from(range.start)..usize::from(range.end)].fill(blank);
    }

    fn scroll_up(&mut self, count: u16) {
        let start = usize::from(self.margins.start);
        let end = usize::from(self.margins.end);
        let count = usize::from(count).min(end - start);
        let blank = Arc::new(Row {
            cells: vec![self.blank(); usize::from(self.cols())],
            wrapped: false,
        });
        self.screen.rows[start..end].rotate_left(count);
        self.screen.rows[end - count..end].fill(blank);
    }

    fn line_feed(&mut self) {
        let cursor = self.screen.cursor;
        if cursor.row + 1 == self.margins.end {
            self.scroll_up(1);
            self.go_to(cursor.row, cursor.col);
        } else {
            self.go_to(cursor.row.saturating_add(1), cursor.col);
        }
    }

    fn wrap_line(&mut self) {
        let row = self.screen.cursor.row;
        self.row(row).wrapped = true;
        self.line_feed();
        self.screen.cursor.col = 0;
    }

    fn extend_grapheme(&mut self, ch: char, width: usize) -> bool {
        let cursor = self.screen.cursor;
        let end = cursor.col + u16::from(cursor.wrap_pending);
        let Some(mut col) = end.checked_sub(1) else {
            return false;
        };
        if self
            .screen
            .cell(cursor.row, col)
            .unwrap()
            .is_wide_continuation()
        {
            col -= 1;
        }
        let cell = self.screen.cell(cursor.row, col).unwrap();
        let text = cell.contents();
        if text.is_empty() {
            return false;
        }
        if text.len() + ch.len_utf8() > MAX_GRAPHEME_BYTES {
            return width == 0;
        }
        let mut combined = String::with_capacity(text.len() + ch.len_utf8());
        combined.push_str(text);
        combined.push(ch);
        if width != 0 && combined.graphemes(true).nth(1).is_some() {
            return false;
        }
        let next_wide = combined.width() == 2 && col + 1 < self.cols();
        let was_wide = cell.is_wide();
        let style = cell.style;
        let hyperlink = cell.hyperlink_arc().cloned();
        if next_wide && !was_wide {
            self.clear(cursor.row, col + 1..col + 2);
        }
        let cells = &mut self.row(cursor.row).cells;
        let width = if next_wide {
            Width::Wide
        } else {
            Width::Single
        };
        cells[usize::from(col)] =
            Cell::new(&combined, width, style).with_hyperlink(hyperlink.clone());
        if next_wide {
            cells[usize::from(col + 1)] =
                Cell::new("", Width::Continuation, style).with_hyperlink(hyperlink);
        } else if was_wide {
            cells[usize::from(col + 1)] = Cell::default();
        }
        self.set_cursor_after(col + if next_wide { 2 } else { 1 });
        true
    }

    fn erase_display(&mut self, kind: u16) {
        let cursor = self.screen.cursor;
        let cols = self.cols();
        match kind {
            0 => {
                self.clear(cursor.row, cursor.col..cols);
                for row in cursor.row + 1..self.rows() {
                    self.clear(row, 0..cols);
                }
            }
            1 => {
                for row in 0..cursor.row {
                    self.clear(row, 0..cols);
                }
                self.clear(cursor.row, 0..cursor.col + 1);
            }
            2 => {
                for row in 0..self.rows() {
                    self.clear(row, 0..cols);
                }
            }
            _ => {}
        }
    }

    fn set_private_mode(&mut self, mode: u16, set: bool) {
        match mode {
            1 => self.screen.application_cursor = set,
            6 => {
                self.origin = set;
                self.position(0, 0);
            }
            7 => {
                self.wrap = set;
                if !set {
                    self.screen.cursor.wrap_pending = false;
                }
            }
            25 => self.screen.cursor.hidden = !set,
            _ => {}
        }
    }
}

impl Perform for DecodeState {
    fn print(&mut self, ch: char) {
        let Some(width) = ch.width() else { return };
        if !ch.is_ascii() && self.extend_grapheme(ch, width) {
            return;
        }
        if width == 0 {
            return;
        }
        let width = width as u16;
        if self.screen.cursor.wrap_pending
            || (self.wrap && self.screen.cursor.col + width > self.cols() && width <= self.cols())
        {
            self.wrap_line();
        }
        let cursor = self.screen.cursor;
        let end = cursor.col + width;
        if end > self.cols() {
            return;
        }
        self.clear(cursor.row, cursor.col..end);
        let hyperlink = self.active_hyperlink.clone();
        let cells = &mut self.row(cursor.row).cells;
        let kind = if width == 2 {
            Width::Wide
        } else {
            Width::Single
        };
        cells[usize::from(cursor.col)] = Cell::new(ch.encode_utf8(&mut [0; 4]), kind, cursor.style)
            .with_hyperlink(hyperlink.clone());
        if width == 2 {
            cells[usize::from(cursor.col + 1)] =
                Cell::new("", Width::Continuation, cursor.style).with_hyperlink(hyperlink);
        }
        self.set_cursor_after(end);
    }

    fn execute(&mut self, byte: u8) {
        let cursor = self.screen.cursor;
        match byte {
            0x08 => self.go_to(cursor.row, cursor.col.saturating_sub(1)),
            b'\t' => self.go_to(cursor.row, (cursor.col | 7).saturating_add(1)),
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            b'\r' => self.go_to(cursor.row, 0),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, command: char) {
        if ignore {
            return;
        }
        let mut values = params.iter().map(|part| part[0]);
        let first = values.next().unwrap_or(0);
        let count = first.max(1);
        let cursor = self.screen.cursor;
        let cols = self.cols();
        match (intermediates, command) {
            (b"", 'H' | 'f') => self.position(count - 1, values.next().unwrap_or(1).max(1) - 1),
            (b"", 'A') => self.go_to(cursor.row.saturating_sub(count), cursor.col),
            (b"", 'B') => self.go_to(cursor.row.saturating_add(count), cursor.col),
            (b"", 'C') => self.go_to(cursor.row, cursor.col.saturating_add(count)),
            (b"", 'D') => self.go_to(cursor.row, cursor.col.saturating_sub(count)),
            (b"", 'G') => self.go_to(cursor.row, count - 1),
            (b"", 'd') => self.position(count - 1, cursor.col),
            (b"" | b"?", 'J') => self.erase_display(first),
            (b"" | b"?", 'K') => match first {
                0 => self.clear(cursor.row, cursor.col..cols),
                1 => self.clear(cursor.row, 0..cursor.col + 1),
                2 => self.clear(cursor.row, 0..cols),
                _ => {}
            },
            (b"", 'X') => self.clear(
                cursor.row,
                cursor.col..cursor.col.saturating_add(count).min(cols),
            ),
            (b"", 'm') => apply_sgr(&mut self.screen.cursor.style, params),
            (b"", 'r') => {
                let end = values
                    .next()
                    .filter(|end| *end > 0)
                    .unwrap_or(self.rows())
                    .min(self.rows());
                if count < end {
                    self.margins = count - 1..end;
                    self.position(0, 0);
                }
            }
            (b"?", 'h' | 'l') => {
                for mode in std::iter::once(first).chain(values) {
                    self.set_private_mode(mode, command == 'h');
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !intermediates.is_empty() || ignore {
            return;
        }
        match byte {
            b'7' => self.saved = self.screen.cursor,
            b'8' => {
                let hidden = self.screen.cursor.hidden;
                self.screen.cursor = Cursor {
                    hidden,
                    ..self.saved
                };
            }
            b'=' => self.screen.application_keypad = true,
            b'>' => self.screen.application_keypad = false,
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        if params.first() != Some(&b"8".as_slice()) {
            return;
        }
        let meta = params.get(1).copied().unwrap_or(b"");
        let uri = params.get(2).copied().unwrap_or(b"");
        if uri.is_empty() {
            self.active_hyperlink = None;
            return;
        }
        let Ok(uri) = std::str::from_utf8(uri) else {
            self.active_hyperlink = None;
            return;
        };
        let id = meta
            .split(|&byte| byte == b':')
            .find_map(|part| part.strip_prefix(b"id="))
            .and_then(|val| std::str::from_utf8(val).ok())
            .filter(|id| !id.is_empty());
        self.active_hyperlink = sanitize_hyperlink(uri, id).map(Arc::new);
    }
}

fn apply_sgr(style: &mut Style, params: &Params) {
    let mut groups = params.iter();
    while let Some(group) = groups.next() {
        match group[0] {
            0 => *style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = group.get(1) != Some(&0),
            7 => style.inverse = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            27 => style.inverse = false,
            n @ 30..=37 => style.fg = Color::Indexed((n - 30) as u8),
            n @ 40..=47 => style.bg = Color::Indexed((n - 40) as u8),
            n @ 90..=97 => style.fg = Color::Indexed((n - 82) as u8),
            n @ 100..=107 => style.bg = Color::Indexed((n - 92) as u8),
            n @ (38 | 48) => {
                let color = if group.len() == 1 {
                    let mut values = groups.by_ref().map(|part| part[0]);
                    values.next().and_then(|kind| read_color(kind, values))
                } else {
                    let start = if group.len() == 6 && group[1] == 2 {
                        3
                    } else {
                        2
                    };
                    read_color(group[1], group[start..].iter().copied())
                };
                if let Some(color) = color {
                    if n == 38 {
                        style.fg = color;
                    } else {
                        style.bg = color;
                    }
                }
            }
            39 => style.fg = Color::Default,
            49 => style.bg = Color::Default,
            _ => {}
        }
    }
}

fn read_color(kind: u16, mut values: impl Iterator<Item = u16>) -> Option<Color> {
    let mut channel = || u8::try_from(values.next()?).ok();
    match kind {
        5 => Some(Color::Indexed(channel()?)),
        2 => Some(Color::Rgb(channel()?, channel()?, channel()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(rows: u16, cols: u16) -> Decoder {
        Decoder::new(rows.try_into().unwrap(), cols.try_into().unwrap())
    }

    #[test]
    fn applies_fragments_and_wide_graphemes_without_mutating_old_frames() {
        let mut decoder = decoder(2, 12);
        for byte in "A한e\u{301}👩\u{200d}💻B".as_bytes() {
            decoder.process(std::slice::from_ref(byte));
        }
        let old = decoder.screen().clone();
        assert_eq!(old.contents(), "A한e\u{301}👩\u{200d}💻B");
        assert!(old.cell(0, 2).unwrap().is_wide_continuation());
        assert!(old.cell(0, 5).unwrap().is_wide_continuation());
        decoder.process(b"\x1b[1;3H\x1b[X");
        assert_eq!(decoder.screen().contents(), "A  e\u{301}👩\u{200d}💻B");
        assert_eq!(old.cell(0, 1).unwrap().contents(), "한");
    }

    #[test]
    fn consumes_legacy_cursor_and_rendition_sequences() {
        let mut decoder = decoder(3, 8);
        decoder.process(b"\x1b[H\x1b[J\x1b[1;38;5;42mA\x1b[2C\x1b[48;2;4;5;6mB");
        decoder.process(b"\x1b7\x1b[3;1H\x1b[mfooter\x1b8\x1b[?1h\x1b=\x1b[?25l");
        let screen = decoder.screen();
        assert_eq!(screen.contents(), "A  B\n\nfooter");
        assert_eq!(screen.cursor_position(), (0, 4));
        assert_eq!(screen.cursor.style.bg, Color::Rgb(4, 5, 6));
        assert_eq!(screen.cell(0, 0).unwrap().style.fg, Color::Indexed(42));
        assert!(screen.cell(0, 0).unwrap().style.bold);
        assert!(screen.application_keypad && screen.application_cursor && screen.hide_cursor());
    }

    #[test]
    fn retains_soft_wrap_and_resets_the_rightmost_cells_on_erase() {
        let mut decoder = decoder(3, 4);
        decoder.process(b"abcd");
        assert_eq!(decoder.screen().cursor_position(), (0, 4));
        decoder.process(b"e");
        assert!(decoder.screen().row_wrapped(0));
        assert_eq!(decoder.screen().contents_between(0, 1, 1, 1), "bcde");
        decoder.process(b"\x1b[1;3H\x1b[K");
        assert!(!decoder.screen().row_wrapped(0));
        assert_eq!(decoder.screen().contents(), "ab\ne");
    }

    #[test]
    fn decodes_osc8_hyperlinks_including_long_uris_and_ids() {
        let mut decoder = decoder(2, 20);
        let long_uri = format!("https://example.test/{}", "a".repeat(180));
        let seq = format!("\x1b]8;id=grp1:foo=bar;{long_uri}\x07link\x1b]8;;\x1b\\ plain");
        decoder.process(seq.as_bytes());
        let screen = decoder.screen();
        let expected = super::Hyperlink {
            uri: long_uri,
            id: Some("grp1".into()),
        };
        for col in 0..4 {
            assert_eq!(screen.cell(0, col).unwrap().hyperlink(), Some(&expected));
        }
        assert_eq!(screen.cell(0, 4).unwrap().hyperlink(), None);
        assert_eq!(screen.cell(0, 5).unwrap().hyperlink(), None);
    }
}
