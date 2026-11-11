use std::num::NonZeroU16;
use std::ops::Range;

use corvid_protocol::wire::TerminalHistory;
use unicode_width::UnicodeWidthChar;
use vt100::{Parser, Screen};

use crate::terminal_session::Grid;

const QUERY_LIMIT: usize = 256;
type CellRange = Range<(u16, u16)>;

pub(super) struct History {
    result: Option<Result<TerminalHistory, String>>,
    first: usize,
    pub(super) search: Option<Search>,
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
            first: usize::MAX,
            search: search.then(Search::default),
        }
    }

    pub(super) fn loaded(&mut self, result: Result<TerminalHistory, String>) {
        self.result = Some(result);
        self.find(true);
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
                } else {
                    format!("Terminal history{coverage}    Ctrl+Shift+F to find · Esc to live")
                }
            }
        }
    }

    pub(super) fn scroll(&mut self, delta: isize, height: usize) {
        if self.snapshot().is_none() {
            return;
        }
        let end = self.last_top(height);
        self.first = self.first.min(end).saturating_add_signed(delta).min(end);
    }

    fn snapshot(&self) -> Option<&TerminalHistory> {
        self.result.as_ref()?.as_ref().ok()
    }

    fn last_top(&self, height: usize) -> usize {
        self.snapshot()
            .map_or(0, |snapshot| snapshot.lines.len().saturating_sub(height))
    }

    pub(super) fn screen(&self, grid: Grid) -> Option<(Screen, Option<CellRange>)> {
        let snapshot = self.snapshot()?;
        let rows = usize::from(grid.rows.get());
        let top = self.first.min(self.last_top(rows));
        let cols = NonZeroU16::new(snapshot.columns as u16)?;
        let mut parser = Parser::new(grid.rows, cols, 0);
        for (row, line) in snapshot.lines.iter().skip(top).take(rows).enumerate() {
            parser.process(format!("\x1b[{};1H", row + 1).as_bytes());
            parser.process(line.as_bytes());
        }
        parser.process(b"\x1b[?25l");
        let hit = self.search.as_ref().and_then(|search| {
            let (row, byte) = search.hit?;
            let visible = row.checked_sub(top).filter(|row| *row < rows)? as u16;
            let line = &snapshot.lines[row];
            let start = columns(&line[..byte]);
            let end = start.saturating_add(columns(&search.query).max(1));
            Some((visible, start)..(visible, end))
        });
        Some((parser.screen().clone(), hit))
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
            self.first = row.saturating_sub(2);
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
    fn searches_snapshot_and_wraps_between_matches_after_wide_characters() {
        let mut history = History::new(true);
        history.loaded(Ok(TerminalHistory {
            lines: ["한글 hello", "earlier", "hello again", "latest"]
                .map(str::to_owned)
                .into(),
            columns: 30,
            truncated: false,
        }));
        history.edit_query(Some("hello"));
        assert_eq!(history.search.as_ref().unwrap().hit, Some((2, 0)));
        history.find(false);
        let (_, hit) = history.screen(Grid::new(2, 30)).unwrap();
        assert_eq!(hit, Some((0, 5)..(0, 10)));
        history.find(true);
        assert_eq!(history.search.as_ref().unwrap().hit, Some((2, 0)));
    }
}
