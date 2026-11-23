use std::collections::VecDeque;

use corvid_protocol::wire::{HistoryStyle, TerminalHistory};
use corvid_protocol::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS};
use unicode_width::UnicodeWidthChar;
use vt100::{Screen, capture::RowContents};

use super::is_bidi_control;
use super::snapshot::{CellStyle, CellWidth, ScreenCell};

#[derive(Default)]
pub(super) struct OlderRows {
    // These strings come from the emulator's SGR-only row serializer.
    lines: VecDeque<String>,
    bytes: usize,
    truncated: bool,
}

impl OlderRows {
    pub(super) fn push(&mut self, row: RowContents<'_>) {
        let mut line = String::new();
        row.write_formatted_basic(&mut line, &mut Default::default())
            .unwrap();
        self.bytes += line.len() + 8;
        self.lines.push_back(line);
        while self.lines.len() > TERMINAL_HISTORY_MAX_ROWS
            || self.bytes > TERMINAL_HISTORY_MAX_BYTES
        {
            let oldest = self.lines.pop_front().expect("history is over budget");
            self.bytes -= oldest.len() + 8;
            self.truncated = true;
        }
    }

    pub(super) fn append(&self, snapshot: &mut HistoryBuilder) {
        for line in self.lines.iter().rev() {
            if !snapshot.push(HistoryLine::from_formatted(line, snapshot.columns)) {
                break;
            }
        }
        snapshot.truncated |= self.truncated;
    }
}

#[derive(Default)]
pub(super) struct HistoryLine {
    text: String,
    styles: Vec<HistoryStyle>,
}

impl HistoryLine {
    pub(super) fn from_screen(screen: &Screen, row: u16, columns: u16) -> Self {
        Self::from_cells((0..columns).filter_map(|col| screen.cell(row, col).map(ScreenCell::from)))
    }

    pub(super) fn from_cells<'a>(cells: impl IntoIterator<Item = ScreenCell<'a>>) -> Self {
        let mut line = Self::default();
        let mut style = CellStyle::default();
        let mut blanks = 0;
        for cell in cells {
            if cell.width == CellWidth::Continuation {
                continue;
            }
            let next = cell.style;
            if next != style {
                // Empty cells with the previous style still belong before the change.
                line.text.extend(std::iter::repeat_n(' ', blanks));
                blanks = 0;
                let mut sgr = String::new();
                next.write_sgr(&mut sgr);
                line.change(sgr);
                style = next;
            }
            if !cell.text.is_empty() {
                line.text.extend(std::iter::repeat_n(' ', blanks));
                blanks = 0;
                line.text
                    .extend(cell.text.chars().filter(|&ch| printable(ch)));
            } else if style == CellStyle::default() {
                blanks += 1;
            } else {
                line.text.push(' ');
            }
        }
        // A reset just beyond the last printable cell has no effect.
        if line
            .styles
            .last()
            .is_some_and(|s| s.offset as usize == line.text.len())
        {
            line.styles.pop();
        }
        line
    }

    fn from_formatted(formatted: &str, columns: u16) -> Self {
        let mut line = Self::default();
        let mut width = 0;
        let mut parts = formatted.split("\x1b[");
        let mut text = parts.next().unwrap_or_default();
        loop {
            for ch in text.chars().filter(|&ch| printable(ch)) {
                width += ch.width().unwrap_or(0);
                if width > usize::from(columns) {
                    return line;
                }
                line.text.push(ch);
            }
            let Some((sgr, rest)) = parts.next().and_then(|part| part.split_once('m')) else {
                break;
            };
            if width < usize::from(columns) {
                line.change(sgr.to_owned());
            }
            text = rest;
        }
        line
    }

    fn change(&mut self, sgr: String) {
        self.styles.push(HistoryStyle {
            row: 0,
            offset: self.text.len() as u32,
            sgr,
        });
    }
}

fn printable(ch: char) -> bool {
    !ch.is_control() && !is_bidi_control(ch)
}

pub(super) struct HistoryBuilder {
    columns: u16,
    lines: Vec<HistoryLine>,
    bytes: usize,
    pub(super) truncated: bool,
}

impl HistoryBuilder {
    pub(super) fn new(columns: u16) -> Self {
        Self {
            columns,
            lines: Vec::new(),
            bytes: 16,
            truncated: false,
        }
    }

    // Collect newest first so the same wire budget covers both text and attributes.
    pub(super) fn push(&mut self, line: HistoryLine) -> bool {
        let size =
            line.text.len() + 8 + line.styles.iter().map(|s| s.sgr.len() + 20).sum::<usize>();
        if self.lines.len() == TERMINAL_HISTORY_MAX_ROWS
            || self.bytes + size > TERMINAL_HISTORY_MAX_BYTES
        {
            self.truncated = true;
            return false;
        }
        self.bytes += size;
        self.lines.push(line);
        true
    }

    pub(super) fn finish(self) -> TerminalHistory {
        let mut result = TerminalHistory {
            columns: self.columns.into(),
            truncated: self.truncated,
            ..Default::default()
        };
        for (row, line) in self.lines.into_iter().rev().enumerate() {
            result.lines.push(line.text);
            result
                .styles
                .extend(line.styles.into_iter().map(|mut change| {
                    change.row = row as u32;
                    change
                }));
        }
        result
    }
}
