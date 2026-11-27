use std::fmt::Write as _;

use corvid_core::navigation::Rect;
use corvid_protocol::history::{
    CommandOutputError, HistoryNavigation, ScrollOutcome, jumpable_commands,
};
use corvid_protocol::valid_clipboard_text;
use corvid_protocol::wire::TerminalHistory;
use unicode_width::UnicodeWidthChar;

use super::keys::shortcut_keys;
use super::title_motion::window;

pub(super) struct HistoryView {
    history: TerminalHistory,
    nav: HistoryNavigation,
}

pub(super) enum HistoryAction {
    Keep,
    Close,
    Copy { text: String, truncated: bool },
}

impl HistoryView {
    pub(super) fn open_previous(
        history: TerminalHistory,
        rows: usize,
    ) -> Result<Self, &'static str> {
        let body_rows = rows.saturating_sub(1).max(1);
        let nav = HistoryNavigation::open_previous(&history, body_rows)?;
        Ok(Self { history, nav })
    }

    pub(super) fn set_notice(&mut self, notice: impl Into<String>) {
        self.nav.notice = Some(notice.into());
    }

    pub(super) fn key(&mut self, bytes: &[u8], rows: usize) -> HistoryAction {
        let body_rows = rows.saturating_sub(1).max(1);
        match bytes {
            b"\x1b" | b"\x03" => return HistoryAction::Close,
            b"\x1b[A" | b"\x1bOA" => {
                self.nav.scroll_up(1);
                return HistoryAction::Keep;
            }
            b"\x1b[B" | b"\x1bOB" => return self.scroll_down(1, body_rows),
            b"\x1b[5~" => {
                self.nav.scroll_up(body_rows);
                return HistoryAction::Keep;
            }
            b"\x1b[6~" => return self.scroll_down(body_rows, body_rows),
            b"\x1b[H" | b"\x1b[1~" => {
                self.nav.scroll_to_top();
                return HistoryAction::Keep;
            }
            b"\x1b[F" | b"\x1b[4~" => return HistoryAction::Close,
            b"\x1b[1;2A" => {
                self.jump(true, body_rows);
                return HistoryAction::Keep;
            }
            b"\x1b[1;2B" => {
                self.jump(false, body_rows);
                return HistoryAction::Keep;
            }
            _ => {}
        }

        match shortcut_keys(bytes) {
            b"q" => HistoryAction::Close,
            b"[" | b"p" => {
                self.jump(true, body_rows);
                HistoryAction::Keep
            }
            b"]" | b"n" => {
                self.jump(false, body_rows);
                HistoryAction::Keep
            }
            b"c" => self.copy_selected(),
            b"k" => {
                self.nav.scroll_up(1);
                HistoryAction::Keep
            }
            b"j" => self.scroll_down(1, body_rows),
            b"g" => {
                self.nav.scroll_to_top();
                HistoryAction::Keep
            }
            b"G" => HistoryAction::Close,
            _ => HistoryAction::Keep,
        }
    }

    pub(super) fn jump(&mut self, previous: bool, body_rows: usize) {
        self.nav.jump_command(&self.history, previous, body_rows);
    }

    fn scroll_down(&mut self, rows: usize, body_rows: usize) -> HistoryAction {
        match self.nav.scroll_down(&self.history, rows, body_rows) {
            ScrollOutcome::Keep => HistoryAction::Keep,
            ScrollOutcome::ExitToLive => HistoryAction::Close,
        }
    }

    fn copy_selected(&mut self) -> HistoryAction {
        match self.nav.copy_output(&self.history) {
            Ok(output) if valid_clipboard_text(&output.text) => HistoryAction::Copy {
                text: output.text,
                truncated: output.truncated,
            },
            Ok(_) => {
                self.nav.notice =
                    Some("Last command output exceeds the 64 KiB clipboard limit.".into());
                HistoryAction::Keep
            }
            Err(CommandOutputError::NoCommands) => {
                self.nav.notice = Some("No command markers in this terminal.".into());
                HistoryAction::Keep
            }
            Err(CommandOutputError::NoOutput) => {
                self.nav.notice = Some("Last command produced no output.".into());
                HistoryAction::Keep
            }
        }
    }

    pub(super) fn paint(&self, rect: Rect) -> String {
        if rect.width == 0 || rect.height == 0 {
            return String::new();
        }
        let width = usize::from(rect.width);
        let jumpable = jumpable_commands(&self.history);
        let mut header = match self
            .nav
            .selected_command
            .and_then(|idx| jumpable.iter().position(|&item| item == idx))
        {
            Some(pos) => format!("Command {}/{}", pos + 1, jumpable.len()),
            None => "Terminal history".to_owned(),
        };
        if let Some(notice) = &self.nav.notice {
            let _ = write!(header, " · {notice}");
        }
        if self.history.truncated {
            header.push_str(" · older rows omitted");
        }
        header.push_str(" · [/] Prev/Next · c Copy · Esc Live");

        let mut ansi = String::from("\x1b[?25l\x1b[?7l");
        let clipped_header = window(&header, width, 0);
        let header_cols = clipped_header
            .chars()
            .filter_map(UnicodeWidthChar::width)
            .sum::<usize>();
        let _ = write!(
            ansi,
            "\x1b[{};{}H\x1b[0;7m{clipped_header}{}",
            rect.y + 1,
            rect.x + 1,
            " ".repeat(width.saturating_sub(header_cols))
        );

        let body_rows = usize::from(rect.height.saturating_sub(1));
        for offset in 0..body_rows {
            let screen_row = rect.y + 1 + offset as u16;
            let _ = write!(ansi, "\x1b[{};{}H\x1b[0m", screen_row + 1, rect.x + 1);
            let row_idx = self.nav.top + offset;
            let printed_cols = if let Some(line) = self.history.lines.get(row_idx) {
                write_styled_line(
                    &mut ansi,
                    line,
                    &self.history.styles,
                    &self.history.hyperlinks,
                    row_idx as u32,
                    width,
                )
            } else {
                0
            };
            if printed_cols < width {
                ansi.push_str(&" ".repeat(width - printed_cols));
            }
        }
        ansi.push_str("\x1b[0m\x1b[?7h");
        ansi
    }
}

fn write_styled_line(
    out: &mut String,
    line: &str,
    styles: &[corvid_protocol::wire::HistoryStyle],
    hyperlinks: &[corvid_protocol::wire::HistoryHyperlink],
    row_index: u32,
    max_cols: usize,
) -> usize {
    let first_style = styles.partition_point(|change| change.row < row_index);
    let row_styles = &styles[first_style..];
    let mut style_idx = 0;
    let first_link = hyperlinks.partition_point(|link| link.row < row_index);
    let row_links = &hyperlinks[first_link..];
    let mut link_idx = 0;
    let mut active_end: Option<u32> = None;
    let mut cols = 0;
    for (byte_idx, ch) in line.char_indices() {
        if let Some(end) = active_end
            && byte_idx >= end as usize
        {
            out.push_str("\x1b]8;;\x1b\\");
            active_end = None;
        }
        while let Some(link) = row_links.get(link_idx)
            && link.row == row_index
            && (link.end_offset as usize) <= byte_idx
        {
            link_idx += 1;
        }
        if active_end.is_none()
            && let Some(link) = row_links.get(link_idx)
            && link.row == row_index
            && (link.start_offset as usize) == byte_idx
        {
            out.push_str("\x1b]8;");
            if let Some(id) = &link.id {
                out.push_str("id=");
                out.push_str(id);
            }
            out.push(';');
            out.push_str(&link.uri);
            out.push_str("\x1b\\");
            active_end = Some(link.end_offset);
        }
        while let Some(change) = row_styles.get(style_idx)
            && change.row == row_index
            && (change.offset as usize) <= byte_idx
        {
            if (change.offset as usize) == byte_idx {
                let _ = write!(out, "\x1b[{}m", change.sgr);
            }
            style_idx += 1;
        }
        let ch_width = ch.width().unwrap_or(0);
        if cols + ch_width > max_cols {
            break;
        }
        if !ch.is_control() {
            out.push(ch);
            cols += ch_width;
        }
    }
    if active_end.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_protocol::wire::HistoryCommand;

    #[test]
    fn opens_navigates_copies_and_renders_within_rect() {
        let history = TerminalHistory {
            columns: 40,
            lines: vec![
                "$ echo one".into(),
                "one".into(),
                "$ echo two".into(),
                "two".into(),
                "$ ".into(),
            ],
            commands: vec![
                HistoryCommand {
                    prompt_row: Some(0),
                    output_start_row: Some(1),
                    output_end_row: Some(2),
                    finished: true,
                    ..Default::default()
                },
                HistoryCommand {
                    prompt_row: Some(2),
                    output_start_row: Some(3),
                    output_end_row: Some(4),
                    finished: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut view = HistoryView::open_previous(history, 4).unwrap();
        assert_eq!(view.nav.selected_command, Some(1));
        assert_eq!(view.nav.top, 2);

        let ansi = view.paint(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        });
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(4).unwrap(),
            std::num::NonZeroU16::new(40).unwrap(),
            0,
        );
        parser.process(ansi.as_bytes());
        let contents = parser.screen().contents();
        assert!(contents.contains("Command 2/2"));
        assert!(contents.contains("$ echo two"));

        assert!(matches!(view.key(b"[", 4), HistoryAction::Keep));
        assert_eq!(view.nav.selected_command, Some(0));
        assert_eq!(view.nav.top, 0);

        assert!(matches!(view.key(b"j", 4), HistoryAction::Keep));
        assert_eq!(view.nav.top, 1);
        assert!(matches!(view.key(b"j", 4), HistoryAction::Keep));
        assert_eq!(view.nav.top, 2);
        assert!(matches!(view.key(b"j", 4), HistoryAction::Close));

        match view.key(b"c", 4) {
            HistoryAction::Copy { text, .. } => assert_eq!(text, "one"),
            _ => panic!("expected copy action"),
        }

        assert!(matches!(view.key(b"\x1b", 4), HistoryAction::Close));
    }

    #[test]
    fn paint_emits_and_closes_osc8_hyperlinks_even_when_clipped() {
        let history = TerminalHistory {
            columns: 20,
            lines: vec!["open https://example.test".into()],
            hyperlinks: vec![corvid_protocol::wire::HistoryHyperlink {
                row: 0,
                start_offset: 5,
                end_offset: 25,
                uri: "https://example.test".into(),
                id: Some("h1".into()),
            }],
            commands: vec![corvid_protocol::wire::HistoryCommand {
                prompt_row: Some(0),
                output_start_row: Some(0),
                output_start_offset: Some(0),
                finished: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let view = HistoryView::open_previous(history, 3).unwrap();
        let ansi = view.paint(Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        });
        assert!(ansi.contains("\x1b]8;id=h1;https://example.test\x1b\\https\x1b]8;;\x1b\\"));
    }
}
