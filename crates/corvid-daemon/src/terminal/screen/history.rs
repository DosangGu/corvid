use std::collections::VecDeque;

use corvid_protocol::wire::TerminalHistory;
use corvid_protocol::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS};
use unicode_width::UnicodeWidthChar;
use vt100::capture::{RowContents, basic_formatted_to_plain};

use super::is_bidi_control;

#[derive(Default)]
pub(super) struct OlderRows {
    lines: VecDeque<String>,
    bytes: usize,
    truncated: bool,
}

impl OlderRows {
    pub(super) fn push(&mut self, row: RowContents<'_>) {
        let mut formatted = String::new();
        row.write_formatted_basic(&mut formatted, &mut Default::default())
            .unwrap();
        let mut line: String = basic_formatted_to_plain(&formatted).collect();
        line.retain(|ch| !ch.is_control() && !is_bidi_control(ch));
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

    pub(super) fn append_newest_first(&self, history: &mut TerminalHistory, mut bytes: usize) {
        history.truncated = self.truncated;
        for line in self.lines.iter().rev() {
            // Match the emulator's row projection after the pane is narrowed.
            let mut width = 0;
            let end = line
                .char_indices()
                .find_map(|(index, ch)| {
                    width += ch.width().unwrap_or(0);
                    (width > history.columns as usize).then_some(index)
                })
                .unwrap_or(line.len());
            bytes += end + 8;
            if bytes > TERMINAL_HISTORY_MAX_BYTES
                || history.lines.len() == TERMINAL_HISTORY_MAX_ROWS
            {
                history.truncated = true;
                break;
            }
            history.lines.push(line[..end].to_owned());
        }
    }
}
