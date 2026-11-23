use corvid_protocol::ansi_screen::{Cell, Style};
use corvid_protocol::wire::{HistoryStyle, TerminalHistory};
use corvid_protocol::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS};

use super::is_bidi_control;
#[derive(Default)]
pub(super) struct HistoryLine {
    text: String,
    styles: Vec<HistoryStyle>,
}

impl HistoryLine {
    pub(super) fn from_cells(cells: &[Cell]) -> Self {
        let mut line = Self::default();
        let mut style = Style::default();
        let mut blanks = 0;
        for cell in cells {
            if cell.is_wide_continuation() {
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
    truncated: bool,
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
