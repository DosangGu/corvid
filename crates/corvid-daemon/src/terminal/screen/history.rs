use corvid_protocol::ansi_screen::{Cell, Style};
use corvid_protocol::wire::{HistoryStyle, TerminalHistory};
use corvid_protocol::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS};

use super::is_bidi_control;

#[derive(Default)]
pub(super) struct HistoryLine {
    text: String,
    styles: Vec<HistoryStyle>,
}

pub(super) struct ColumnOffsets {
    pub(super) starts: Vec<usize>,
    pub(super) ends: Vec<usize>,
}

impl HistoryLine {
    pub(super) fn from_cells(
        cells: &[Cell],
        wrapped: bool,
        track_columns: bool,
    ) -> (Self, Option<ColumnOffsets>) {
        let mut line = Self::default();
        let mut style = Style::default();
        let mut blanks = 0;
        let mut starts = track_columns.then(|| Vec::with_capacity(cells.len()));
        let mut ends = track_columns.then(|| Vec::with_capacity(cells.len()));
        let mut prev_start = 0;
        let mut prev_end = 0;

        for cell in cells {
            if cell.is_wide_continuation() {
                if let (Some(starts), Some(ends)) = (&mut starts, &mut ends) {
                    starts.push(prev_start);
                    ends.push(prev_end);
                }
                continue;
            }
            let col_start = line.text.len() + blanks;
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
            if cell.has_contents() {
                line.text.extend(std::iter::repeat_n(' ', blanks));
                blanks = 0;
                line.text
                    .extend(cell.contents().chars().filter(|&ch| printable(ch)));
            } else if style == Style::default() {
                blanks += 1;
            } else {
                line.text.push(' ');
            }
            let col_end = line.text.len() + blanks;
            if let (Some(starts), Some(ends)) = (&mut starts, &mut ends) {
                starts.push(col_start);
                ends.push(col_end);
                prev_start = col_start;
                prev_end = col_end;
            }
        }
        if wrapped && blanks > 0 {
            line.text.extend(std::iter::repeat_n(' ', blanks));
        }
        // A reset just beyond the last printable cell has no effect.
        if line
            .styles
            .last()
            .is_some_and(|s| s.offset as usize == line.text.len())
        {
            line.styles.pop();
        }
        let offsets = starts.zip(ends).map(|(mut starts, mut ends)| {
            let max_len = line.text.len();
            for offset in &mut starts {
                *offset = (*offset).min(max_len);
            }
            for offset in &mut ends {
                *offset = (*offset).min(max_len);
            }
            ColumnOffsets { starts, ends }
        });
        (line, offsets)
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
    lines: Vec<(HistoryLine, bool)>,
    bytes: usize,
    truncated: bool,
}

impl HistoryBuilder {
    pub(super) fn new(columns: u16, reserved_bytes: usize) -> Self {
        Self {
            columns,
            lines: Vec::new(),
            bytes: 16 + reserved_bytes,
            truncated: false,
        }
    }

    // Collect newest first so the same wire budget covers both text and attributes.
    pub(super) fn push(&mut self, line: HistoryLine, wrapped: bool) -> bool {
        let size = line.text.len()
            + 8
            + usize::from(wrapped) * 4
            + line.styles.iter().map(|s| s.sgr.len() + 20).sum::<usize>();
        if self.lines.len() == TERMINAL_HISTORY_MAX_ROWS
            || self.bytes + size > TERMINAL_HISTORY_MAX_BYTES
        {
            self.truncated = true;
            return false;
        }
        self.bytes += size;
        self.lines.push((line, wrapped));
        true
    }

    pub(super) fn finish(self) -> (TerminalHistory, usize) {
        let count = self.lines.len();
        let mut result = TerminalHistory {
            columns: self.columns.into(),
            truncated: self.truncated,
            ..Default::default()
        };
        for (row, (line, wrapped)) in self.lines.into_iter().rev().enumerate() {
            result.lines.push(line.text);
            if wrapped {
                result.wrapped_rows.push(row as u32);
            }
            result
                .styles
                .extend(line.styles.into_iter().map(|mut change| {
                    change.row = row as u32;
                    change
                }));
        }
        (result, count)
    }
}
