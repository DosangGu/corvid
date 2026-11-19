use std::ops::Range;

use url::Url;
use vt100::Screen;

use crate::terminal_session::Grid;

const MAX_CELLS: usize = 4096;
const MAX_BYTES: usize = 8192;
type Position = (u16, u16);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Link {
    pub url: String,
    pub cells: Range<Position>,
}

pub(super) fn at(screen: &Screen, grid: Grid, history: bool, mut click: Position) -> Option<Link> {
    let (rows, cols) = screen.size();
    let visible = LinkCells {
        screen,
        rows: rows.min(grid.rows).get(),
        cols: cols.min(grid.cols).get(),
        history,
    };
    if click.0 >= visible.rows || click.1 >= visible.cols {
        return None;
    }
    if screen.cell(click.0, click.1)?.is_wide_continuation() {
        click.1 = click.1.checked_sub(1)?;
    }
    let cell = screen.cell(click.0, click.1)?;
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
                url,
                cells: start?..(row, col + width),
            });
        }
    }
    None
}

struct Budget {
    cells: usize,
    bytes: usize,
}

struct LinkCells<'a> {
    screen: &'a Screen,
    rows: u16,
    cols: u16,
    history: bool,
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
            Direction::Left if row > 0 && !self.history && self.screen.row_wrapped(row - 1) => {
                if full_width {
                    Next::Cell((row - 1, self.cols - 1))
                } else {
                    Next::Clipped
                }
            }
            Direction::Left => Next::Boundary,
            Direction::Right if col + 1 < self.cols => Next::Cell((row, col + 1)),
            Direction::Right if !full_width || self.history => Next::Clipped,
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
    if !bytes.contains(&clicked_byte)
        || candidate.chars().any(|ch| {
            ch.is_control()
                || matches!(
                    ch,
                    '\\' | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
        })
    {
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
        let mut parser = vt100::Parser::new(grid.rows, grid.cols, 0);
        parser.process(text.as_bytes());
        parser.screen().clone()
    }

    fn url_at(screen: &Screen, grid: Grid, history: bool, cell: Position) -> Option<String> {
        at(screen, grid, history, cell).map(|link| link.url)
    }

    #[test]
    fn reports_visible_cells_for_wrapped_wide_links_without_surrounding_punctuation() {
        let grid = Grid::new(4, 16);
        let display = screen(grid, "[go](https://example.test/한글).");
        assert!(display.row_wrapped(0));
        let link = at(&display, grid, false, (1, 13)).unwrap();
        assert_eq!(link.url, "https://example.test/%ED%95%9C%EA%B8%80");
        assert_eq!(link.cells, (0, 5)..(1, 14));
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
            assert_eq!(at(&display, grid, false, (0, col)), None);
        }
    }

    #[test]
    fn stops_at_an_unmatched_closing_parenthesis_before_adjacent_text() {
        let grid = Grid::new(3, 100);
        let url = "https://example.test/a_(b)";
        for suffix in [")", ")입니다", ").(see next)"] {
            let display = screen(grid, &format!("({url}{suffix}"));
            assert_eq!(url_at(&display, grid, false, (0, 12)).as_deref(), Some(url));
            assert_eq!(at(&display, grid, false, (0, url.len() as u16 + 1)), None);
        }
    }

    #[test]
    fn follows_only_confirmed_soft_wraps_within_the_view() {
        let grid = Grid::new(4, 16);
        let display = screen(grid, "https://example.test/path ");
        assert!(display.row_wrapped(0));
        for click in [(0, 8), (1, 4)] {
            assert_eq!(
                url_at(&display, grid, false, click).as_deref(),
                Some("https://example.test/path")
            );
        }
        assert_eq!(at(&display, grid, true, (0, 8)), None);
        assert_eq!(at(&display, Grid::new(4, 12), false, (0, 8)), None);

        let display = screen(grid, "https://e.test/a\r\nsecond");
        assert_eq!(
            url_at(&display, grid, false, (0, 8)).as_deref(),
            Some("https://e.test/a")
        );
        assert_eq!(at(&display, grid, true, (0, 8)), None);
        assert_eq!(at(&display, grid, false, (1, 4)), None);
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
                assert_eq!(at(&display, grid, false, click), None);
            }
        }
    }
}
