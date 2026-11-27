use std::num::NonZeroU16;
use std::ops::Range;

use corvid_protocol::ansi_screen::{Decoder, Screen};
use corvid_protocol::history::{
    CommandOutput, CommandOutputError, HistoryNavigation, ScrollOutcome, jumpable_commands,
};
use corvid_protocol::wire::TerminalHistory;
use unicode_width::UnicodeWidthChar;

use crate::terminal_session::Grid;

const QUERY_LIMIT: usize = 256;
type CellRange = Range<(u16, u16)>;

pub(super) struct History {
    result: Option<Result<TerminalHistory, String>>,
    pending_back: usize,
    pub(super) wheel_browse: bool,
    pub(super) search: Option<Search>,
    pub(super) nav: HistoryNavigation,
}

#[derive(Default)]
pub(super) struct Search {
    query: String,
    hit: Option<(usize, usize)>,
}

impl History {
    pub(super) fn new(search: bool) -> Self {
        Self {
            result: None,
            pending_back: 0,
            wheel_browse: false,
            search: search.then(Search::default),
            nav: HistoryNavigation {
                top: usize::MAX,
                selected_command: None,
                notice: None,
            },
        }
    }

    pub(super) fn from_wheel(
        result: Result<TerminalHistory, String>,
        back: usize,
        height: usize,
        shell_only: bool,
    ) -> Option<Self> {
        let snapshot = result.ok()?;
        let eligible = if shell_only {
            snapshot.shell_foreground
        } else {
            snapshot.alternate_screen == Some(false)
        };
        if !eligible || back == 0 || snapshot.lines.len() <= height {
            return None;
        }
        let mut history = Self::new(false);
        history.wheel_browse = true;
        history.pending_back = back;
        history.loaded(Ok(snapshot), height);
        Some(history)
    }

    pub(super) fn loaded(&mut self, result: Result<TerminalHistory, String>, height: usize) {
        if let Ok(snapshot) = &result {
            self.nav.reset_bottom(snapshot, height, self.pending_back);
        } else {
            self.nav = HistoryNavigation::default();
        }
        self.result = Some(result);
        self.find(true);
    }

    pub(super) fn jump_command(&mut self, previous: bool, height: usize) -> bool {
        let Some(snapshot) = self.snapshot().cloned() else {
            return false;
        };
        self.nav.jump_command(&snapshot, previous, height)
    }

    pub(super) fn copy_command_output(&mut self) -> Result<CommandOutput, CommandOutputError> {
        let Some(snapshot) = self.snapshot() else {
            return Err(CommandOutputError::NoCommands);
        };
        self.nav.copy_output(snapshot)
    }

    pub(super) fn title(&self, marked: &str) -> String {
        match &self.result {
            None => "Loading terminal history…".into(),
            Some(Err(message)) => message.clone(),
            Some(Ok(snapshot)) => {
                let coverage = if snapshot.truncated {
                    " · older rows omitted"
                } else {
                    ""
                };
                if let Some(search) = &self.search {
                    let found = if !search.query.is_empty() && search.hit.is_none() {
                        " · no matches"
                    } else {
                        ""
                    };
                    format!(
                        "Find: {}{marked}{found}{coverage}    Enter / Shift+Enter · Esc to live",
                        search.query
                    )
                } else if let Some(selected) = self.nav.selected_command {
                    let jumpable = jumpable_commands(snapshot);
                    let pos = jumpable
                        .iter()
                        .position(|&item| item == selected)
                        .map_or(1, |idx| idx + 1);
                    let notice = self
                        .nav
                        .notice
                        .as_deref()
                        .map_or(String::new(), |msg| format!(" · {msg}"));
                    format!(
                        "Command {pos}/{}{notice}{coverage}    [/] Prev/Next · c Copy · Esc to live",
                        jumpable.len().max(1)
                    )
                } else {
                    let notice = self
                        .nav
                        .notice
                        .as_deref()
                        .map_or(String::new(), |msg| format!(" · {msg}"));
                    let cmd_hint = if jumpable_commands(snapshot).is_empty() {
                        ""
                    } else {
                        " · [ Prev command · c Copy"
                    };
                    format!(
                        "Terminal history{notice}{coverage}    Ctrl+] ? to find{cmd_hint} · Esc to live"
                    )
                }
            }
        }
    }

    pub(super) fn scroll(&mut self, delta: isize, height: usize) -> ScrollOutcome {
        let Some(snapshot) = self.result.as_ref().and_then(|result| result.as_ref().ok()) else {
            self.pending_back = self
                .pending_back
                .saturating_add_signed(delta.saturating_neg());
            return ScrollOutcome::Keep;
        };
        self.nav.scroll_by(snapshot, delta, height)
    }

    pub(super) fn at_bottom(&self, height: usize) -> bool {
        match &self.result {
            None => self.pending_back == 0,
            Some(Ok(_)) => self.nav.top >= self.last_top(height),
            Some(Err(_)) => true,
        }
    }

    pub(super) fn is_loaded(&self) -> bool {
        self.result.is_some()
    }

    fn snapshot(&self) -> Option<&TerminalHistory> {
        self.result.as_ref()?.as_ref().ok()
    }

    fn last_top(&self, height: usize) -> usize {
        self.snapshot()
            .map_or(0, |snapshot| HistoryNavigation::max_top(snapshot, height))
    }

    pub(super) fn screen(&self, grid: Grid) -> Option<(Screen, Option<CellRange>)> {
        let snapshot = self.snapshot()?;
        let rows = usize::from(grid.rows.get());
        let top = self.nav.top.min(self.last_top(rows));
        let cols = NonZeroU16::new(snapshot.columns as u16)?;
        let mut parser = Decoder::new(grid.rows, cols);
        for (row, line) in snapshot.lines.iter().skip(top).take(rows).enumerate() {
            parser.process(format!("\x1b]8;;\x1b\\\x1b[0m\x1b[{};1H", row + 1).as_bytes());
            let index = (top + row) as u32;
            let mut style_idx = snapshot.styles.partition_point(|change| change.row < index);
            let mut link_idx = snapshot.hyperlinks.partition_point(|link| link.row < index);
            let mut active_link_end: Option<usize> = None;
            let mut from = 0;
            loop {
                let next_style = snapshot
                    .styles
                    .get(style_idx)
                    .filter(|change| change.row == index)
                    .map(|change| change.offset as usize);
                let next_link_start = if active_link_end.is_none() {
                    snapshot
                        .hyperlinks
                        .get(link_idx)
                        .filter(|link| link.row == index)
                        .map(|link| link.start_offset as usize)
                } else {
                    None
                };
                let next_offset = [next_style, next_link_start, active_link_end]
                    .into_iter()
                    .flatten()
                    .min();
                let Some(offset) = next_offset else {
                    break;
                };
                if offset > from {
                    parser.process(&line.as_bytes()[from..offset]);
                    from = offset;
                }
                if active_link_end == Some(offset) {
                    parser.process(b"\x1b]8;;\x1b\\");
                    active_link_end = None;
                    link_idx += 1;
                }
                while let Some(change) = snapshot
                    .styles
                    .get(style_idx)
                    .filter(|change| change.row == index && change.offset as usize == offset)
                {
                    parser.process(format!("\x1b[{}m", change.sgr).as_bytes());
                    style_idx += 1;
                }
                if active_link_end.is_none()
                    && let Some(link) = snapshot
                        .hyperlinks
                        .get(link_idx)
                        .filter(|link| link.row == index && link.start_offset as usize == offset)
                {
                    let mut osc = String::from("\x1b]8;");
                    if let Some(id) = &link.id {
                        osc.push_str("id=");
                        osc.push_str(id);
                    }
                    osc.push(';');
                    osc.push_str(&link.uri);
                    osc.push_str("\x1b\\");
                    parser.process(osc.as_bytes());
                    active_link_end = Some(link.end_offset as usize);
                }
            }
            parser.process(&line.as_bytes()[from..]);
            if active_link_end.is_some() {
                parser.process(b"\x1b]8;;\x1b\\");
            }
        }
        parser.process(b"\x1b[?25l");
        let mut screen = parser.screen().clone();
        let visible_rows = rows.min(snapshot.lines.len().saturating_sub(top));
        for row in 0..visible_rows {
            if snapshot
                .wrapped_rows
                .binary_search(&((top + row) as u32))
                .is_ok()
            {
                screen.set_row_wrapped(row as u16, true);
            }
        }
        let hit = self.search.as_ref().and_then(|search| {
            let (row, byte) = search.hit?;
            let visible = row.checked_sub(top).filter(|row| *row < rows)? as u16;
            let line = &snapshot.lines[row];
            let start = columns(&line[..byte]);
            let end = start.saturating_add(columns(&search.query).max(1));
            Some((visible, start)..(visible, end))
        });
        Some((screen, hit))
    }

    pub(super) fn edit_query(&mut self, text: Option<&str>) {
        let Some(search) = &mut self.search else {
            return;
        };
        if let Some(text) = text {
            let available = QUERY_LIMIT.saturating_sub(search.query.chars().count());
            search
                .query
                .extend(text.chars().filter(|ch| !ch.is_control()).take(available));
        } else {
            search.query.pop();
        }
        search.hit = None;
        self.find(true);
    }

    pub(super) fn find(&mut self, reverse: bool) {
        let Some(snapshot) = self.result.as_ref().and_then(|result| result.as_ref().ok()) else {
            return;
        };
        let Some(search) = self
            .search
            .as_mut()
            .filter(|search| !search.query.is_empty())
        else {
            return;
        };
        let mut boundary = None;
        let mut closest = None;
        for (row, line) in snapshot.lines.iter().enumerate() {
            for (byte, _) in line.match_indices(search.query.as_str()) {
                let hit = (row, byte);
                if reverse {
                    boundary = Some(hit);
                    if search.hit.is_none_or(|current| hit < current) {
                        closest = Some(hit);
                    }
                } else {
                    boundary.get_or_insert(hit);
                    if search.hit.is_none_or(|current| hit > current) {
                        closest.get_or_insert(hit);
                    }
                }
            }
        }
        search.hit = closest.or(boundary);
        if let Some((row, _)) = search.hit {
            self.nav.top = row.saturating_sub(2);
        }
    }
}

fn columns(text: &str) -> u16 {
    text.chars()
        .filter_map(UnicodeWidthChar::width)
        .sum::<usize>()
        .try_into()
        .unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_wheel_history_only_for_a_confirmed_shell_with_older_rows() {
        let mut snapshot = TerminalHistory {
            columns: 40,
            lines: (0..12).map(|row| row.to_string()).collect(),
            ..Default::default()
        };
        assert!(History::from_wheel(Ok(snapshot.clone()), 3, 4, true).is_none());
        snapshot.shell_foreground = true;
        assert!(History::from_wheel(Ok(snapshot.clone()), 3, 12, true).is_none());
        assert!(History::from_wheel(Ok(snapshot.clone()), 0, 4, true).is_none());
        let mut history = History::from_wheel(Ok(snapshot), 3, 4, true).unwrap();
        assert_eq!(history.nav.top, 5);
        assert_eq!(history.scroll(3, 4), ScrollOutcome::Keep);
        assert!(history.at_bottom(4));
        assert_eq!(history.scroll(1, 4), ScrollOutcome::ExitToLive);
    }

    #[test]
    fn inline_program_history_opens_with_colored_wide_text_but_alternate_history_does_not() {
        let mut snapshot = TerminalHistory {
            columns: 20,
            lines: vec!["한글 diff".into(), "next".into(), "last".into()],
            alternate_screen: Some(true),
            styles: vec![corvid_protocol::wire::HistoryStyle {
                row: 0,
                offset: 0,
                sgr: "31;4".into(),
            }],
            ..Default::default()
        };
        assert!(History::from_wheel(Ok(snapshot.clone()), 1, 2, false).is_none());
        snapshot.alternate_screen = Some(false);
        let mut history = History::from_wheel(Ok(snapshot), 1, 2, false).unwrap();
        assert!(history.wheel_browse);
        history.search = Some(Search::default());
        history.edit_query(Some("diff"));
        let (screen, hit) = history.screen(Grid::new(2, 20)).unwrap();
        assert_eq!(hit.unwrap(), (0, 5)..(0, 9));
        assert_eq!(
            screen.cell(0, 5).unwrap().style.fg,
            corvid_protocol::ansi_screen::Color::Indexed(1)
        );
        assert!(screen.cell(0, 5).unwrap().style.underline);
        assert_eq!(
            screen.cell(1, 0).unwrap().style.fg,
            corvid_protocol::ansi_screen::Color::Default
        );
    }

    #[test]
    fn scrolls_by_the_entire_gesture_while_waiting_for_history() {
        let mut history = History::new(false);
        for delta in [-3, -5, -8, 2] {
            history.scroll(delta, 4);
        }
        history.loaded(
            Ok(TerminalHistory {
                columns: 20,
                lines: (0..80).map(|row| format!("row {row}")).collect(),
                ..Default::default()
            }),
            4,
        );
        assert_eq!(history.nav.top, 62);
        assert!(!history.at_bottom(4));
        history.scroll(13, 4);
        assert!(!history.at_bottom(4));
        history.scroll(1, 4);
        assert!(history.at_bottom(4));
        history.scroll(isize::MIN, 4);
        assert_eq!(
            history.screen(Grid::new(4, 20)).unwrap().0.contents(),
            "row 0\nrow 1\nrow 2\nrow 3"
        );
    }

    #[test]
    fn searches_snapshot_and_wraps_between_matches_after_wide_characters() {
        let mut history = History::new(true);
        history.loaded(
            Ok(TerminalHistory {
                lines: ["한글 hello", "earlier", "hello again", "latest"]
                    .map(str::to_owned)
                    .into(),
                columns: 30,
                ..Default::default()
            }),
            2,
        );
        history.edit_query(Some("hello"));
        assert_eq!(history.search.as_ref().unwrap().hit, Some((2, 0)));
        history.find(false);
        let (_, hit) = history.screen(Grid::new(2, 30)).unwrap();
        assert_eq!(hit, Some((0, 5)..(0, 10)));
        history.find(true);
        assert_eq!(history.search.as_ref().unwrap().hit, Some((2, 0)));
    }

    #[test]
    fn jumps_between_commands_and_copies_selected_output() {
        let mut history = History::new(false);
        history.loaded(
            Ok(TerminalHistory {
                columns: 40,
                lines: vec![
                    "$ echo one".into(),
                    "one".into(),
                    "$ echo two".into(),
                    "two".into(),
                    "$ ".into(),
                ],
                commands: vec![
                    corvid_protocol::wire::HistoryCommand {
                        prompt_row: Some(0),
                        output_start_row: Some(1),
                        output_end_row: Some(2),
                        finished: true,
                        ..Default::default()
                    },
                    corvid_protocol::wire::HistoryCommand {
                        prompt_row: Some(2),
                        output_start_row: Some(3),
                        output_end_row: Some(4),
                        finished: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            2,
        );
        assert!(history.jump_command(true, 2));
        assert_eq!(history.nav.selected_command, Some(1));
        assert_eq!(history.nav.top, 2);
        assert!(history.title("").contains("Command 2/2"));

        assert!(history.jump_command(true, 2));
        assert_eq!(history.nav.selected_command, Some(0));
        assert_eq!(history.nav.top, 0);
        assert_eq!(history.copy_command_output().unwrap().text, "one");

        assert!(!history.jump_command(true, 2));
        assert_eq!(history.nav.notice.as_deref(), Some("At oldest command"));
    }

    #[test]
    fn screen_replays_hyperlinks_and_wrapped_rows() {
        let mut history = History::new(false);
        history.loaded(
            Ok(TerminalHistory {
                columns: 16,
                lines: vec!["https://example.".into(), "test/path".into()],
                wrapped_rows: vec![0],
                hyperlinks: vec![
                    corvid_protocol::wire::HistoryHyperlink {
                        row: 0,
                        start_offset: 0,
                        end_offset: 16,
                        uri: "https://example.test/path".into(),
                        id: None,
                    },
                    corvid_protocol::wire::HistoryHyperlink {
                        row: 1,
                        start_offset: 0,
                        end_offset: 9,
                        uri: "https://example.test/path".into(),
                        id: None,
                    },
                ],
                ..Default::default()
            }),
            2,
        );
        let (screen, _) = history.screen(Grid::new(2, 16)).unwrap();
        assert!(screen.row_wrapped(0));
        assert_eq!(
            screen
                .cell(0, 0)
                .unwrap()
                .hyperlink()
                .map(|l| l.uri.as_str()),
            Some("https://example.test/path")
        );
        assert_eq!(
            screen
                .cell(1, 3)
                .unwrap()
                .hyperlink()
                .map(|l| l.uri.as_str()),
            Some("https://example.test/path")
        );
    }
}
