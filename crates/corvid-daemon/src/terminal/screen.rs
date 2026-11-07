use std::fmt::Write;
use std::mem;

use anyhow::{Result, ensure};
use vt100::{Callbacks, Parser, Screen};

use corvid_core::Viewport;

const SCROLLBACK_ROWS: usize = 512;
const MAX_REPLY_BYTES: usize = 32 * 1024;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const RESET_DRAWING: &str = "\x1b[?6l\x1b[r\x1b[?7h\x1b[0m";
const DEVICE_ATTRIBUTES: &str = "\x1b[?6c";

#[derive(Default)]
struct QueryReplies {
    bytes: Vec<u8>,
    overflow: bool,
}

impl QueryReplies {
    fn push(&mut self, response: &str) {
        if self.bytes.len() + response.len() > MAX_REPLY_BYTES {
            self.overflow = true;
            return;
        }

        self.bytes.extend_from_slice(response.as_bytes());
    }
}

impl Callbacks for QueryReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        if first.is_some() || second.is_some() {
            return;
        }

        match (action, params) {
            ('c', &[] | &[&[0]]) => self.push(DEVICE_ATTRIBUTES),
            ('n', &[&[5]]) => self.push("\x1b[0n"),
            ('n', &[&[6]]) => {
                let (row, col) = screen.cursor_position();
                let (_, cols) = screen.size();
                self.push(&format!("\x1b[{};{}R", row + 1, (col + 1).min(cols.get())));
            }
            ('t', &[&[18]]) => {
                let (rows, cols) = screen.size();
                self.push(&format!("\x1b[8;{rows};{cols}t"));
            }
            _ => {}
        }
    }

    fn unhandled_escape(
        &mut self,
        _screen: &mut Screen,
        first: Option<u8>,
        second: Option<u8>,
        action: u8,
    ) {
        if (first, second, action) == (None, None, b'Z') {
            self.push(DEVICE_ATTRIBUTES);
        }
    }
}

pub struct TerminalScreen {
    parser: Parser<QueryReplies>,
}

impl TerminalScreen {
    pub fn new(viewport: Viewport) -> Self {
        let parser = Parser::new_with_callbacks(
            viewport.rows,
            viewport.cols,
            SCROLLBACK_ROWS,
            QueryReplies::default(),
        );

        Self { parser }
    }

    pub fn process(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        self.parser.process(bytes);

        let replies = self.parser.callbacks_mut();
        ensure!(!replies.overflow, "terminal query reply limit exceeded");
        Ok(mem::take(&mut replies.bytes))
    }

    pub fn resize(&mut self, viewport: Viewport) {
        self.parser.set_size(viewport.rows, viewport.cols);
    }

    pub fn redraw(&self) -> Result<String> {
        self.render(None)
    }

    pub(crate) fn snapshot(&self) -> Screen {
        self.parser.screen().clone()
    }

    pub(crate) fn render(&self, previous: Option<&Screen>) -> Result<String> {
        let screen = self.parser.screen();
        let mut frame = if let Some(previous) = previous {
            screen.contents_diff(previous)
        } else {
            let mut frame = String::from(RESET_DRAWING);
            frame.push_str(&screen.contents_formatted());
            frame
        };

        // Only these input modes are part of the spike's outer terminal profile.
        let cursor = if screen.application_cursor() {
            'h'
        } else {
            'l'
        };
        let paste = if screen.bracketed_paste() { 'h' } else { 'l' };
        write!(frame, "\x1b[?1{cursor}\x1b[?2004{paste}")?;
        frame.push_str(if screen.application_keypad() {
            "\x1b="
        } else {
            "\x1b>"
        });

        ensure!(
            frame.len() <= MAX_FRAME_BYTES,
            "terminal redraw exceeds frame limit"
        );
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;
    use vt100::Color;

    fn terminal(cols: u16, rows: u16) -> TerminalScreen {
        TerminalScreen::new(Viewport::new(cols, rows).unwrap())
    }

    #[test]
    fn renders_fragmented_unicode_with_wide_and_combining_cells() {
        let mut terminal = terminal(80, 24);
        for byte in "한글 e\u{301}".as_bytes() {
            terminal.process(&[*byte]).unwrap();
        }

        let screen = terminal.parser.screen();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "한");
        assert!(screen.cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(screen.cell(0, 2).unwrap().contents(), "글");
        assert_eq!(screen.cell(0, 5).unwrap().contents(), "e\u{301}");
    }

    #[test]
    fn redraw_reconstructs_grid_colors_and_input_modes() {
        let mut child = terminal(80, 24);
        child.process(b"original\x1b[?1049h").unwrap();
        child
            .process(b"\x1b[38;2;6;7;8m\x1b[48;5;42mA\x1b[?25l\x1b[?1h\x1b=\x1b[?2004h")
            .unwrap();

        let mut outer = terminal(80, 24);
        outer.process(b"stale\x1b[2;5r\x1b[?6h").unwrap();
        outer.process(child.redraw().unwrap().as_bytes()).unwrap();

        let screen = outer.parser.screen();
        let cell = screen.cell(0, 0).unwrap();
        assert_eq!(screen.contents(), "A");
        assert_eq!(cell.fgcolor(), Color::Rgb(6, 7, 8));
        assert_eq!(cell.bgcolor(), Color::Idx(42));
        assert_eq!(screen.cursor_position(), (0, 1));
        assert!(screen.hide_cursor());
        assert!(screen.application_cursor());
        assert!(screen.application_keypad());
        assert!(screen.bracketed_paste());

        child.process(b"\x1b[?1049l").unwrap();
        assert_eq!(child.parser.screen().contents(), "original");
    }

    #[test]
    fn scroll_regions_and_erase_preserve_unaffected_rows() {
        let mut terminal = terminal(8, 4);
        terminal
            .process(b"top\r\nsecond\r\nthird\r\nbottom")
            .unwrap();
        terminal.process(b"\x1b[2;3r\x1b[3;1H\nnew").unwrap();
        assert_eq!(
            terminal.parser.screen().contents(),
            "top\nthird\nnew\nbottom"
        );

        terminal.process(b"\x1b[2;1H\x1b[K").unwrap();
        assert_eq!(terminal.parser.screen().contents(), "top\n\nnew\nbottom");
    }

    #[test]
    fn replies_to_child_queries_using_the_resized_inner_screen() {
        let mut terminal = terminal(80, 24);
        terminal.resize(Viewport::new(300, 120).unwrap());
        terminal.process(b"\x1b[120;300H\x1b[").unwrap();
        let replies = terminal.process(b"6n\x1b[5n\x1b[c\x1b[18t").unwrap();
        assert_eq!(replies, b"\x1b[120;300R\x1b[0n\x1b[?6c\x1b[8;120;300t");

        terminal.resize(Viewport::new(2, 2).unwrap());
        assert_eq!(terminal.process(b"\x1b[18t").unwrap(), b"\x1b[8;2;2t");
    }

    #[test]
    fn bounds_scrollback_and_long_cell_text_without_replaying_osc() {
        let mut terminal = terminal(10, 2);
        for _ in 0..SCROLLBACK_ROWS * 2 {
            terminal.process(b"line\r\n").unwrap();
        }
        assert_eq!(terminal.parser.screen().scrollback_len(), SCROLLBACK_ROWS);

        terminal.process(b"\x1b[HA").unwrap();
        for _ in 0..8 {
            terminal.process("\u{301}".repeat(4096).as_bytes()).unwrap();
        }
        let cell = terminal.parser.screen().cell(0, 0).unwrap();
        assert!(cell.contents().len() <= 22);

        terminal.process(b"\x1b]52;c;").unwrap();
        for _ in 0..128 {
            assert!(terminal.process(&[b'x'; 8192]).unwrap().is_empty());
        }
        terminal.process(b"\x07\x1b[2;1Hvisible").unwrap();
        let redraw = terminal.redraw().unwrap();
        assert!(redraw.contains("visible"));
        assert!(!redraw.contains("\x1b]"));
    }

    #[test]
    fn redraw_fits_the_limit_at_maximum_viewport_with_dense_attributes() {
        let mut terminal = terminal(300, 120);
        let mut row = String::new();
        for col in 0..300 {
            let (intensity, red, green, blue) = if col % 2 == 0 {
                ("1;3;4;7", 123, 234, 145)
            } else {
                ("22;23;24;27", 234, 145, 123)
            };
            write!(
                row,
                "\x1b[{intensity};38;2;{red};{green};{blue};48;2;{blue};{green};{red}mA"
            )
            .unwrap();
            row.push_str(&"\u{301}".repeat(10));
        }

        for line in 1..=120 {
            terminal
                .process(format!("\x1b[{line};1H").as_bytes())
                .unwrap();
            terminal.process(row.as_bytes()).unwrap();
        }

        let frame = terminal.redraw().unwrap();
        let mut outer = Parser::new(
            NonZeroU16::new(120).unwrap(),
            NonZeroU16::new(300).unwrap(),
            0,
        );
        outer.process(frame.as_bytes());
        let inner = terminal.parser.screen();
        for line in 0..120 {
            for col in 0..300 {
                assert_eq!(inner.cell(line, col), outer.screen().cell(line, col));
            }
        }
    }
}
