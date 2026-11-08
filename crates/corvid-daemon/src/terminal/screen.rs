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
        Self::with_scrollback(viewport, SCROLLBACK_ROWS)
    }

    pub(super) fn with_scrollback(viewport: Viewport, rows: usize) -> Self {
        let parser =
            Parser::new_with_callbacks(viewport.rows, viewport.cols, rows, QueryReplies::default());

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

    pub fn redraw(&self, outer: Viewport) -> Result<String> {
        self.render_frame(None, self.bracketed_paste(), outer, None)
    }

    pub(super) fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    pub(crate) fn snapshot(&self) -> Screen {
        self.parser.screen().clone()
    }

    pub(crate) fn render(
        &self,
        previous: Option<&Screen>,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        self.render_frame(previous, true, outer, footer)
    }

    fn render_frame(
        &self,
        previous: Option<&Screen>,
        outer_paste: bool,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let clipped_width = cols < outer.cols;
        let mut frame = String::new();
        if previous.is_none() {
            frame.push_str(RESET_DRAWING);
            if rows < outer.rows {
                write!(frame, "\x1b[1;{rows}r")?;
            }
        }

        if clipped_width {
            Self::draw_rows(&mut frame, screen, previous)?;
        } else if let Some(previous) = previous {
            frame.push_str(&screen.contents_diff(previous));
        } else {
            frame.push_str(&screen.contents_formatted());
        }

        if let Some((row, ansi)) = footer {
            write!(frame, "\x1b[?7l\x1b[{row};1H{ansi}\x1b[0m\x1b[?7h")?;
        }
        if clipped_width {
            let (row, col) = screen.cursor_position();
            let col = (col + 1).min(cols.get());
            let visibility = if screen.hide_cursor() { 'l' } else { 'h' };
            write!(frame, "\x1b[{};{col}H\x1b[?25{visibility}", row + 1)?;
        } else if footer.is_some() {
            // Redrawing the rightmost cell also restores a pending wrap.
            frame.push_str(&screen.cursor_state_formatted());
        }
        if clipped_width || footer.is_some() {
            frame.push_str(&screen.attributes_formatted());
        }

        // The Unicode Bidi_Control characters can reorder untrusted cell text
        // in the outer terminal. Their removal leaves the VT cell widths intact.
        if !frame.is_ascii() {
            frame.retain(|ch| {
                !matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            });
        }

        // Only these input modes are part of the spike's outer terminal profile.
        let cursor = if screen.application_cursor() {
            'h'
        } else {
            'l'
        };
        let paste = if outer_paste { 'h' } else { 'l' };
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

    fn draw_rows(frame: &mut String, screen: &Screen, previous: Option<&Screen>) -> Result<()> {
        let (rows, cols) = screen.size();
        let blank;
        let baseline = if let Some(previous) = previous {
            previous
        } else {
            frame.push_str("\x1b[2J");
            blank = Parser::new(rows, cols, 0);
            blank.screen()
        };

        // Position each row explicitly when the outer TTY wraps at a later column.
        let padding_col = cols.get() + 1;
        for (row, ansi) in screen.rows_diff(baseline, 0, cols.get()).enumerate() {
            if !ansi.is_empty() {
                let row = row + 1;
                write!(
                    frame,
                    "\x1b[{row};1H\x1b[0m{ansi}\x1b[{row};{padding_col}H\x1b[0m\x1b[K"
                )?;
            }
        }
        Ok(())
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
        let frame = child.redraw(Viewport::new(80, 24).unwrap()).unwrap();
        outer.process(frame.as_bytes()).unwrap();

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
    fn status_row_preserves_pending_wrap_and_drawing_attributes() {
        let mut inner = terminal(12, 4);
        inner
            .process(b"\x1b[?1049h\x1b[31m\x1b[4;1Habcdefghijkl")
            .unwrap();
        let mut outer = terminal(12, 5);
        let viewport = Viewport::new(12, 5).unwrap();
        let footer = (5, "\x1b[0;30;46m\x1b[2KCORVID");
        let full = inner.render(None, viewport, Some(footer)).unwrap();
        outer.process(full.as_bytes()).unwrap();
        assert_eq!(
            outer.snapshot().cursor_position(),
            inner.snapshot().cursor_position()
        );
        assert_eq!(outer.snapshot().fgcolor(), Color::Idx(1));

        let previous = inner.snapshot();
        inner.process(b"Z").unwrap();
        let diff = inner.render(Some(&previous), viewport, None).unwrap();
        outer.process(diff.as_bytes()).unwrap();
        let content = inner.snapshot();
        let composed = outer.snapshot();
        for row in 0..4 {
            for col in 0..12 {
                assert_eq!(
                    composed.cell(row, col),
                    content.cell(row, col),
                    "cell {row},{col}"
                );
            }
        }
        assert_eq!(composed.cell(4, 0).unwrap().contents(), "C");
        assert_eq!(composed.cell(4, 11).unwrap().bgcolor(), Color::Idx(6));
        assert_eq!(composed.cursor_position(), content.cursor_position());
    }

    #[test]
    fn renders_wrapped_rows_and_diffs_inside_a_larger_outer_tty() {
        let viewport = Viewport::new(12, 8).unwrap();
        let mut inner = terminal(8, 4);
        let mut outer = terminal(12, 8);
        let footer = (8, "\x1b[0;30;46m\x1b[2KCORVID");
        outer.process(b"\x1b[6;1Hstale\x1b[1;9Hstale").unwrap();
        inner
            .process(b"\x1b[31mabcdefghW\x1b[4;6H\x1b[46m\x1b[K")
            .unwrap();
        let frame = inner.render(None, viewport, Some(footer)).unwrap();
        outer.process(frame.as_bytes()).unwrap();

        let previous = inner.snapshot();
        assert_eq!(outer.snapshot().cell(1, 0), previous.cell(1, 0));
        assert_eq!(
            outer.snapshot().cell(3, 8).unwrap().bgcolor(),
            Color::Default
        );

        inner.process(b"\x1b[0m\x1b[4;1HijklmnopZ").unwrap();
        let frame = inner.render(Some(&previous), viewport, None).unwrap();
        outer.process(frame.as_bytes()).unwrap();

        let expected = inner.parser.screen();
        let composed = outer.parser.screen();
        for row in 0..4 {
            for col in 0..8 {
                assert_eq!(composed.cell(row, col), expected.cell(row, col));
            }
            assert!(!composed.cell(row, 8).unwrap().has_contents());
            assert_eq!(composed.cell(row, 8).unwrap().bgcolor(), Color::Default);
        }
        assert!(!composed.cell(5, 0).unwrap().has_contents());
        assert_eq!(composed.cell(7, 0).unwrap().contents(), "C");
        assert_eq!(composed.cursor_position(), expected.cursor_position());
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
        let redraw = terminal.redraw(Viewport::new(10, 2).unwrap()).unwrap();
        assert!(redraw.contains("visible"));
        assert!(!redraw.contains("\x1b]"));
    }

    #[test]
    fn full_and_incremental_frames_omit_direction_controls_and_keep_visible_unicode() {
        let mut inner = terminal(80, 24);
        let mut outer = terminal(80, 24);
        let viewport = Viewport::new(80, 24).unwrap();
        let controls = "\u{061c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}";
        let marked: String = controls
            .chars()
            .flat_map(|control| ['X', control])
            .collect();
        let text = format!("{marked} 한글 e\u{301} שלום");
        for byte in text.as_bytes() {
            inner.process(&[*byte]).unwrap();
        }

        let full = inner.redraw(viewport).unwrap();
        assert!(!full.contains(|ch| controls.contains(ch)));
        outer.process(full.as_bytes()).unwrap();
        assert_eq!(
            outer.parser.screen().contents(),
            "XXXXXXXXXXXX 한글 e\u{301} שלום"
        );
        assert_eq!(
            outer.parser.screen().cursor_position(),
            inner.parser.screen().cursor_position()
        );

        let previous = inner.snapshot();
        inner
            .process("\x1b[2;1Hvisible\u{202e} update".as_bytes())
            .unwrap();
        let diff = inner.render(Some(&previous), viewport, None).unwrap();
        assert!(!diff.contains('\u{202e}'));
        outer.process(diff.as_bytes()).unwrap();
        assert_eq!(
            outer.parser.screen().contents(),
            "XXXXXXXXXXXX 한글 e\u{301} שלום\nvisible update"
        );
        assert_eq!(
            outer.parser.screen().cursor_position(),
            inner.parser.screen().cursor_position()
        );
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

        let frame = terminal.redraw(Viewport::new(300, 120).unwrap()).unwrap();
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
