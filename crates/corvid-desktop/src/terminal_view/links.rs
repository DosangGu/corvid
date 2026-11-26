use std::ops::Range;
use std::sync::OnceLock;

use corvid_protocol::ansi_screen::{Hyperlink, Screen};
use url::Url;

use crate::terminal_session::Grid;

const MAX_CELLS: usize = 4096;
const MAX_BYTES: usize = 8192;
type Position = (u16, u16);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    External(String),
    LocalFile {
        uri: String,
        display: String,
    },
    RemoteFile {
        uri: String,
        host: Option<String>,
        path: String,
    },
}

impl LinkTarget {
    pub(super) fn preview_label(&self) -> String {
        match self {
            Self::External(url) => url.clone(),
            Self::LocalFile { display, .. } => display.clone(),
            Self::RemoteFile { host, path, .. } => match host {
                Some(host) => format!("remote ({host}): {path}"),
                None => format!("remote: {path}"),
            },
        }
    }

    pub(super) fn is_remote_file(&self) -> bool {
        matches!(self, Self::RemoteFile { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Link {
    pub target: LinkTarget,
    pub spans: Vec<Range<Position>>,
    pub explicit: bool,
}

pub(super) fn at(
    screen: &Screen,
    grid: Grid,
    _history: bool,
    remote_host: Option<&str>,
    mut click: Position,
) -> Option<Link> {
    let (rows, cols) = screen.size();
    let visible = LinkCells {
        screen,
        rows: rows.min(grid.rows).get(),
        cols: cols.min(grid.cols).get(),
    };
    if click.0 >= visible.rows || click.1 >= visible.cols {
        return None;
    }
    if screen.cell(click.0, click.1)?.is_wide_continuation() {
        click.1 = click.1.checked_sub(1)?;
    }
    let cell = screen.cell(click.0, click.1)?;
    if let Some(hyperlink) = cell.hyperlink() {
        let target = classify_explicit_uri(&hyperlink.uri, remote_host)?;
        let spans = explicit_spans(&visible, hyperlink, click);
        return Some(Link {
            target,
            spans,
            explicit: true,
        });
    }

    let clicked = cell.contents();
    if is_boundary(clicked) {
        return None;
    }
    let mut budget = Budget {
        cells: MAX_CELLS - 1,
        bytes: MAX_BYTES - clicked.len(),
    };
    let left = visible.scan(click, Direction::Left, &mut budget)?;
    let clicked_byte = MAX_BYTES - budget.bytes - clicked.len();
    let right = visible.scan(click, Direction::Right, &mut budget)?;
    let pieces = left
        .iter()
        .rev()
        .copied()
        .chain(std::iter::once((click, clicked)))
        .chain(right.iter().copied());
    let mut token = String::with_capacity(MAX_BYTES - budget.bytes);
    for (_, contents) in pieces.clone() {
        token.push_str(contents);
    }
    let (url, bytes) = parse(&token, clicked_byte)?;
    let mut offset = 0;
    let mut start = None;
    for (position, contents) in pieces {
        if bytes.contains(&offset) {
            start.get_or_insert(position);
        }
        offset += contents.len();
        if offset >= bytes.end {
            let (row, col) = position;
            let width = 1 + u16::from(screen.cell(row, col)?.is_wide());
            return Some(Link {
                target: LinkTarget::External(url),
                spans: split_row_spans(start?, (row, col + width), visible.cols),
                explicit: false,
            });
        }
    }
    None
}

fn explicit_spans(
    visible: &LinkCells<'_>,
    hyperlink: &Hyperlink,
    click: Position,
) -> Vec<Range<Position>> {
    if hyperlink.id.is_some() {
        let mut spans = Vec::new();
        for row in 0..visible.rows {
            let mut start: Option<u16> = None;
            let mut col = 0;
            while col < visible.cols {
                let Some(cell) = visible.screen.cell(row, col) else {
                    break;
                };
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }
                let width = 1 + u16::from(cell.is_wide());
                let matches = cell.hyperlink().is_some_and(|other| other == hyperlink);
                if matches {
                    start.get_or_insert(col);
                } else if let Some(begin) = start.take() {
                    spans.push((row, begin)..(row, col));
                }
                col += width;
            }
            if let Some(begin) = start {
                spans.push((row, begin)..(row, visible.cols));
            }
        }
        return spans;
    }

    let matches = |pos: Position| {
        visible
            .screen
            .cell(pos.0, pos.1)
            .and_then(|cell| cell.hyperlink())
            .is_some_and(|other| other.id.is_none() && other.uri == hyperlink.uri)
    };
    let mut start = click;
    let mut steps = 0;
    while steps < MAX_CELLS {
        let Next::Cell(mut prev) = visible.next(start, Direction::Left) else {
            break;
        };
        if visible
            .screen
            .cell(prev.0, prev.1)
            .is_some_and(|cell| cell.is_wide_continuation())
        {
            let Some(lead) = prev.1.checked_sub(1) else {
                break;
            };
            prev.1 = lead;
        }
        if !matches(prev) {
            break;
        }
        start = prev;
        steps += 1;
    }

    let mut end_cell = click;
    steps = 0;
    while steps < MAX_CELLS {
        let width = visible
            .screen
            .cell(end_cell.0, end_cell.1)
            .map_or(1, |cell| 1 + u16::from(cell.is_wide()));
        let probe = (end_cell.0, end_cell.1 + width - 1);
        let Next::Cell(next) = visible.next(probe, Direction::Right) else {
            break;
        };
        if !matches(next) {
            break;
        }
        end_cell = next;
        steps += 1;
    }
    let width = visible
        .screen
        .cell(end_cell.0, end_cell.1)
        .map_or(1, |cell| 1 + u16::from(cell.is_wide()));
    split_row_spans(start, (end_cell.0, end_cell.1 + width), visible.cols)
}

fn split_row_spans(start: Position, end: Position, cols: u16) -> Vec<Range<Position>> {
    if start.0 == end.0 {
        return vec![start..end];
    }
    let mut spans = Vec::with_capacity(usize::from(end.0 - start.0 + 1));
    for row in start.0..=end.0 {
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1 } else { cols };
        if to > from {
            spans.push((row, from)..(row, to));
        }
    }
    spans
}

struct Budget {
    cells: usize,
    bytes: usize,
}

struct LinkCells<'a> {
    screen: &'a Screen,
    rows: u16,
    cols: u16,
}

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
}

enum Next {
    Cell(Position),
    Boundary,
    Clipped,
}

impl<'a> LinkCells<'a> {
    fn scan(
        &self,
        mut position: Position,
        direction: Direction,
        budget: &mut Budget,
    ) -> Option<Vec<(Position, &'a str)>> {
        let mut pieces = Vec::new();
        loop {
            position = match self.next(position, direction) {
                Next::Cell(position) => position,
                Next::Boundary => return Some(pieces),
                Next::Clipped => return None,
            };
            budget.cells = budget.cells.checked_sub(1)?;
            let cell = self.screen.cell(position.0, position.1)?;
            if cell.is_wide_continuation() {
                continue;
            }
            let contents = cell.contents();
            if is_boundary(contents) {
                return Some(pieces);
            }
            budget.bytes = budget.bytes.checked_sub(contents.len())?;
            pieces.push((position, contents));
        }
    }

    fn next(&self, (row, col): Position, direction: Direction) -> Next {
        let full_width = self.screen.size().1.get() == self.cols;
        match direction {
            Direction::Left if col > 0 => Next::Cell((row, col - 1)),
            Direction::Left if row > 0 && self.screen.row_wrapped(row - 1) => {
                if full_width {
                    Next::Cell((row - 1, self.cols - 1))
                } else {
                    Next::Clipped
                }
            }
            Direction::Left => Next::Boundary,
            Direction::Right if col + 1 < self.cols => Next::Cell((row, col + 1)),
            Direction::Right if !full_width => Next::Clipped,
            Direction::Right if !self.screen.row_wrapped(row) => Next::Boundary,
            Direction::Right if row + 1 < self.rows => Next::Cell((row + 1, 0)),
            Direction::Right => Next::Clipped,
        }
    }
}

fn is_boundary(contents: &str) -> bool {
    contents.is_empty()
        || contents.chars().next().is_some_and(|ch| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '<' | '>' | '"' | '\'' | '\u{0060}' | '“' | '”' | '‘' | '’'
                )
        })
}

fn classify_explicit_uri(raw: &str, remote_host: Option<&str>) -> Option<LinkTarget> {
    if has_unsafe_uri_chars(raw) {
        return None;
    }
    let (_, target) = raw.split_once(':')?;
    let uri = Url::parse(raw).ok()?;
    match uri.scheme() {
        "http" | "https" | "vscode"
            if target
                .strip_prefix("//")
                .is_some_and(|host| !host.starts_with('/'))
                && uri.host_str().is_some() =>
        {
            Some(LinkTarget::External(uri.into()))
        }
        "mailto" if !uri.path().is_empty() && !target.starts_with('/') => {
            Some(LinkTarget::External(uri.into()))
        }
        "file" => {
            let path = uri
                .to_file_path()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| uri.path().to_owned());
            if path.is_empty() {
                return None;
            }
            if let Some(remote) = remote_host {
                let host = uri
                    .host_str()
                    .filter(|host| !host.is_empty())
                    .map(str::to_owned)
                    .or_else(|| Some(remote.to_owned()));
                return Some(LinkTarget::RemoteFile {
                    uri: uri.into(),
                    host,
                    path,
                });
            }
            if is_local_file_host(uri.host_str()) {
                Some(LinkTarget::LocalFile {
                    uri: uri.into(),
                    display: path,
                })
            } else {
                let host = uri.host_str().map(str::to_owned);
                Some(LinkTarget::RemoteFile {
                    uri: uri.into(),
                    host,
                    path,
                })
            }
        }
        _ => None,
    }
}

fn is_local_file_host(host: Option<&str>) -> bool {
    let Some(host) = host.filter(|host| !host.is_empty()) else {
        return true;
    };
    let host = host.trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    static LOCAL_HOST: OnceLock<Option<String>> = OnceLock::new();
    let local = LOCAL_HOST.get_or_init(|| {
        #[cfg(unix)]
        {
            std::env::var("HOSTNAME")
                .ok()
                .map(|s| s.trim().trim_end_matches('.').to_owned())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let output = std::process::Command::new("hostname").output().ok()?;
                    output.status.success().then_some(())?;
                    std::str::from_utf8(&output.stdout)
                        .ok()
                        .map(|s| s.trim().trim_end_matches('.').to_owned())
                        .filter(|s| !s.is_empty())
                })
        }
        #[cfg(not(unix))]
        {
            std::env::var("COMPUTERNAME")
                .ok()
                .map(|s| s.trim().trim_end_matches('.').to_owned())
                .filter(|s| !s.is_empty())
        }
    });
    local
        .as_deref()
        .is_some_and(|local| host.eq_ignore_ascii_case(local))
}

fn has_unsafe_uri_chars(text: &str) -> bool {
    text.chars().any(|ch| {
        ch.is_control()
            || matches!(
                ch,
                '\\' | '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

fn parse(token: &str, clicked_byte: usize) -> Option<(String, Range<usize>)> {
    let unwrapped = token
        .strip_prefix('[')
        .and_then(|markdown| markdown.split_once("]("))
        .map_or_else(
            || token.trim_start_matches(['(', '[', '{']),
            |(_, link)| link,
        );
    let candidate = trim_end(unwrapped);
    let start = token.len() - unwrapped.len();
    let bytes = start..start + candidate.len();
    if !bytes.contains(&clicked_byte) || has_unsafe_uri_chars(candidate) {
        return None;
    }
    let (_, target) = candidate.split_once(':')?;
    let uri = Url::parse(candidate).ok()?;
    match uri.scheme() {
        "http" | "https" | "vscode"
            if target
                .strip_prefix("//")
                .is_some_and(|host| !host.starts_with('/'))
                && uri.host_str().is_some() =>
        {
            Some((uri.into(), bytes))
        }
        "mailto" if !uri.path().is_empty() && !target.starts_with('/') => Some((uri.into(), bytes)),
        _ => None,
    }
}

fn trim_end(text: &str) -> &str {
    let mut balance = [0_i32; 3];
    let mut end = text.len();
    for (index, ch) in text.bytes().enumerate() {
        let (bracket, change) = match ch {
            b'(' => (0, 1),
            b')' => (0, -1),
            b'[' => (1, 1),
            b']' => (1, -1),
            b'{' => (2, 1),
            b'}' => (2, -1),
            _ => continue,
        };
        balance[bracket] += change;
        if balance[bracket] < 0 {
            end = index;
            break;
        }
    }
    text[..end].trim_end_matches(['.', ',', ';', ':', '!', '?'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(grid: Grid, text: &str) -> Screen {
        let mut parser = corvid_protocol::ansi_screen::Decoder::new(grid.rows, grid.cols);
        parser.process(text.as_bytes());
        parser.screen().clone()
    }

    fn url_at(screen: &Screen, grid: Grid, history: bool, cell: Position) -> Option<String> {
        at(screen, grid, history, None, cell).map(|link| match link.target {
            LinkTarget::External(url) | LinkTarget::LocalFile { uri: url, .. } => url,
            LinkTarget::RemoteFile { uri, .. } => uri,
        })
    }

    #[test]
    fn reports_visible_cells_for_wrapped_wide_links_without_surrounding_punctuation() {
        let grid = Grid::new(4, 16);
        let display = screen(grid, "[go](https://example.test/한글).");
        assert!(display.row_wrapped(0));
        let link = at(&display, grid, false, None, (1, 13)).unwrap();
        assert_eq!(
            link.target,
            LinkTarget::External("https://example.test/%ED%95%9C%EA%B8%80".into())
        );
        assert_eq!(link.spans, vec![(0, 5)..(0, 16), (1, 0)..(1, 14)]);
        assert!(!link.explicit);
    }

    #[test]
    fn opens_the_url_under_the_click_and_keeps_balanced_punctuation() {
        let grid = Grid::new(3, 100);
        let display = screen(
            grid,
            "한 [go](https://example.test/a_(b)), <https://other.test/>.",
        );
        assert_eq!(
            url_at(&display, grid, false, (0, 13)).as_deref(),
            Some("https://example.test/a_(b)")
        );
        assert_eq!(
            url_at(&display, grid, false, (0, 45)).as_deref(),
            Some("https://other.test/")
        );
        for col in [1, 3, 34, 35] {
            assert_eq!(at(&display, grid, false, None, (0, col)), None);
        }
    }

    #[test]
    fn stops_at_an_unmatched_closing_parenthesis_before_adjacent_text() {
        let grid = Grid::new(3, 100);
        let url = "https://example.test/a_(b)";
        for suffix in [")", ")입니다", ").(see next)"] {
            let display = screen(grid, &format!("({url}{suffix}"));
            assert_eq!(url_at(&display, grid, false, (0, 12)).as_deref(), Some(url));
            assert_eq!(
                at(&display, grid, false, None, (0, url.len() as u16 + 1)),
                None
            );
        }
    }

    #[test]
    fn follows_only_confirmed_soft_wraps_within_the_view() {
        let grid = Grid::new(4, 16);
        let display = screen(grid, "https://example.test/path ");
        assert!(display.row_wrapped(0));
        for history in [false, true] {
            for click in [(0, 8), (1, 4)] {
                assert_eq!(
                    url_at(&display, grid, history, click).as_deref(),
                    Some("https://example.test/path")
                );
            }
        }
        assert_eq!(at(&display, Grid::new(4, 12), false, None, (0, 8)), None);

        let display = screen(grid, "https://e.test/a\r\nsecond");
        assert_eq!(
            url_at(&display, grid, false, (0, 8)).as_deref(),
            Some("https://e.test/a")
        );
        assert_eq!(at(&display, grid, false, None, (1, 4)), None);
    }

    #[test]
    fn handles_wide_paths_and_registered_app_uris() {
        let grid = Grid::new(4, 80);
        let display = screen(
            grid,
            "https://example.test/한글\r\nmailto:hi@example.test\r\nvscode://file/home/dev/foo.rs:42",
        );
        assert_eq!(
            url_at(&display, grid, true, (0, 22)).as_deref(),
            Some("https://example.test/%ED%95%9C%EA%B8%80")
        );
        assert_eq!(
            url_at(&display, grid, true, (1, 3)).as_deref(),
            Some("mailto:hi@example.test")
        );
        assert_eq!(
            url_at(&display, grid, true, (2, 12)).as_deref(),
            Some("vscode://file/home/dev/foo.rs:42")
        );
    }

    #[test]
    fn rejects_unsupported_or_malformed_uris() {
        for text in [
            "javascript:alert(1)",
            "file:///tmp/readme",
            "https:///example.test",
            "https://invalid\\example.test",
            "https://%broken",
            "mailto:",
            "vscode:///file",
        ] {
            assert_eq!(parse(text, 1), None, "{text}");
        }
    }

    #[test]
    fn rejects_candidates_exceeding_either_scan_limit() {
        let grid = Grid::new(64, 120);
        for suffix in ["a".repeat(MAX_CELLS), "€".repeat(MAX_BYTES / 3)] {
            let display = screen(grid, &format!("https://example.test/{suffix} "));
            for click in [(0, 12), (10, 10)] {
                assert_eq!(at(&display, grid, false, None, click), None);
            }
        }
    }

    #[test]
    fn prefers_explicit_osc8_over_visible_text_and_handles_grouped_ids() {
        let grid = Grid::new(4, 30);
        let display = screen(
            grid,
            "\x1b]8;id=g1;https://actual.test/path\x1b\\https://visible.test\x1b]8;;\x1b\\ gap \x1b]8;id=g1;https://actual.test/path\x1b\\more\x1b]8;;\x1b\\",
        );
        let link = at(&display, grid, false, None, (0, 4)).unwrap();
        assert!(link.explicit);
        assert_eq!(
            link.target,
            LinkTarget::External("https://actual.test/path".into())
        );
        assert_eq!(link.spans, vec![(0, 0)..(0, 20), (0, 25)..(0, 29)]);
    }

    #[test]
    fn distinguishes_local_and_remote_file_osc8_links_and_blocks_unsafe_schemes() {
        let grid = Grid::new(3, 60);
        let display = screen(
            grid,
            "\x1b]8;;file:///tmp/src/main.rs\x1b\\src/main.rs\x1b]8;;\x1b\\ \x1b]8;;javascript:alert(1)\x1b\\https://safe.test\x1b]8;;\x1b\\",
        );
        let local = at(&display, grid, false, None, (0, 2)).unwrap();
        assert_eq!(
            local.target,
            LinkTarget::LocalFile {
                uri: "file:///tmp/src/main.rs".into(),
                display: "/tmp/src/main.rs".into(),
            }
        );
        let remote = at(&display, grid, false, Some("prod-box"), (0, 2)).unwrap();
        assert_eq!(
            remote.target,
            LinkTarget::RemoteFile {
                uri: "file:///tmp/src/main.rs".into(),
                host: Some("prod-box".into()),
                path: "/tmp/src/main.rs".into(),
            }
        );
        assert_eq!(at(&display, grid, false, None, (0, 15)), None);
    }
}
