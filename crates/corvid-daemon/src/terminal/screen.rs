use std::fmt::Write;
use std::mem;

use anyhow::{Result, ensure};
use vt100::{Callbacks, Parser, Screen};

use corvid_core::Viewport;
use corvid_protocol::wire::{TerminalHistory, WheelEncoding};
#[cfg(test)]
use corvid_protocol::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS};

#[cfg(feature = "ghostty-vt")]
mod ghostty;
#[cfg(all(test, feature = "ghostty-vt"))]
mod ghostty_qualification;
mod history;
mod sgr;
mod snapshot;

pub(crate) use snapshot::{CellWidth, ScreenSnapshot};

use history::{HistoryBuilder, HistoryLine, OlderRows};

const SCROLLBACK_ROWS: usize = 512;
const MAX_REPLY_BYTES: usize = 32 * 1024;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const RESET_DRAWING: &str = "\x1b[?6l\x1b[r\x1b[?7h\x1b[0m";
const DEVICE_ATTRIBUTES: &str = "\x1b[?6c";

#[derive(Default)]
struct QueryReplies {
    bytes: Vec<u8>,
    overflow: bool,
    title: String,
    older_rows: Option<OlderRows>,
    shell_prompt: Option<bool>,
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
    fn on_scroll(&mut self, row: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen && let Some(older) = &mut self.older_rows {
            older.push(row);
        }
    }

    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        self.title = corvid_core::layout::clean_title(&String::from_utf8_lossy(title));
    }

    fn unhandled_osc(&mut self, screen: &mut Screen, params: &[&[u8]]) {
        if let [b"133" | b"633", phase, ..] = params {
            match *phase {
                b"A" | b"B" => self.shell_prompt = Some(true),
                b"C" => self.shell_prompt = Some(false),
                b"D" => self.shell_prompt = None,
                _ => {}
            }
        }
        if matches!(params.first(), Some(&b"0" | &b"2")) {
            self.set_window_title(screen, &params[1..].join(&b';'));
        }
    }

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
    sgr: sgr::Normalizer,
    #[cfg(feature = "ghostty-vt")]
    ghostty: Option<Box<ghostty::Ghostty>>,
}

impl TerminalScreen {
    pub(super) fn title(&self) -> &str {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return &engine.title;
        }
        &self.parser.callbacks().title
    }
    pub fn new(viewport: Viewport) -> Self {
        Self::with_scrollback(viewport, SCROLLBACK_ROWS, None)
    }

    pub(super) fn live(viewport: Viewport) -> Self {
        // Only the recent styled rows are cloned when computing a render diff.
        Self::with_scrollback(viewport, 128, Some(OlderRows::default()))
    }

    pub(super) fn for_session(viewport: Viewport) -> Result<Self> {
        #[cfg(feature = "ghostty-vt")]
        if std::env::var_os("CORVID_VT_ENGINE").is_some_and(|value| value == "ghostty") {
            return Self::with_ghostty(viewport);
        }
        Ok(Self::live(viewport))
    }

    #[cfg(feature = "ghostty-vt")]
    fn with_ghostty(viewport: Viewport) -> Result<Self> {
        let mut screen = Self::with_scrollback(viewport, 0, None);
        screen.ghostty = Some(Box::new(ghostty::Ghostty::new(viewport)?));
        Ok(screen)
    }

    fn with_scrollback(viewport: Viewport, rows: usize, older_rows: Option<OlderRows>) -> Self {
        let parser = Parser::new_with_callbacks(
            viewport.rows,
            viewport.cols,
            rows,
            QueryReplies {
                older_rows,
                ..Default::default()
            },
        );

        Self {
            parser,
            sgr: sgr::Normalizer::default(),
            #[cfg(feature = "ghostty-vt")]
            ghostty: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &mut self.ghostty {
            let mut replies = Vec::new();
            let mut result = Ok(());
            self.sgr.process(bytes, |chunk, _| {
                if result.is_ok() {
                    result = engine.process(chunk, &mut replies);
                }
            });
            result?;
            return Ok(replies);
        }
        self.sgr.process(bytes, |chunk, clear_history| {
            self.parser.process(chunk);
            if clear_history
                && !self.parser.screen().alternate_screen()
                && let Some(older) = &mut self.parser.callbacks_mut().older_rows
            {
                *older = OlderRows::default();
            }
        });

        let replies = self.parser.callbacks_mut();
        ensure!(!replies.overflow, "terminal query reply limit exceeded");
        Ok(mem::take(&mut replies.bytes))
    }

    pub(super) fn configure_clipboard(&mut self, enabled: bool, reset: bool) {
        self.sgr.configure_clipboard(enabled, reset);
    }

    pub(super) fn take_clipboard(&mut self) -> Option<String> {
        self.sgr.take_clipboard()
    }

    pub fn resize(&mut self, viewport: Viewport) {
        self.parser.set_size(viewport.rows, viewport.cols);
    }

    pub(super) fn resize_session(&mut self, viewport: Viewport) -> Result<()> {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &mut self.ghostty {
            return engine.resize(viewport);
        }
        self.resize(viewport);
        Ok(())
    }

    pub fn redraw(&self, outer: Viewport) -> Result<String> {
        self.render_frame(None, self.bracketed_paste(), outer, None)
    }

    pub(crate) fn viewport(&self) -> Viewport {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return engine.viewport;
        }
        let (rows, cols) = self.parser.screen().size();
        Viewport { rows, cols }
    }

    pub(super) fn bracketed_paste(&self) -> bool {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return engine.bracketed_paste();
        }
        self.parser.screen().bracketed_paste()
    }

    pub(super) fn shell_prompt(&self) -> Option<bool> {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return engine.shell_prompt();
        }
        self.parser.callbacks().shell_prompt
    }

    pub(super) fn allows_shell_history(&self) -> bool {
        !self.alternate_screen()
            && self.shell_prompt() != Some(false)
            && self.wheel_encoding() == WheelEncoding::Disabled
    }

    pub(super) fn alternate_screen(&self) -> bool {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return engine.alternate_screen();
        }
        self.parser.screen().alternate_screen()
    }

    pub(super) fn wheel_encoding(&self) -> WheelEncoding {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return engine.wheel_encoding();
        }
        let screen = self.parser.screen();
        if screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None {
            return WheelEncoding::Disabled;
        }
        match screen.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => WheelEncoding::Legacy,
            vt100::MouseProtocolEncoding::Utf8 => WheelEncoding::Utf8,
            vt100::MouseProtocolEncoding::Sgr => WheelEncoding::Sgr,
        }
    }

    pub(super) fn snapshot(&self) -> Result<ScreenSnapshot> {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return Ok(ScreenSnapshot::from_screen(engine.screen()?));
        }
        Ok(ScreenSnapshot::from_screen(self.parser.screen()))
    }

    pub(super) fn history(&mut self) -> Result<TerminalHistory> {
        #[cfg(feature = "ghostty-vt")]
        if let Some(engine) = &self.ghostty {
            return engine.history();
        }
        let screen = self.parser.screen_mut();
        let (rows, cols) = screen.size();
        let height = usize::from(rows.get());
        let mut remaining = screen.scrollback_len();
        let mut history = HistoryBuilder::new(cols.get());
        let mut take = height;
        'pages: loop {
            for row in (0..take).rev() {
                if !history.push(HistoryLine::from_screen(screen, row as u16, cols.get())) {
                    break 'pages;
                }
            }
            if remaining == 0 {
                break;
            }
            take = remaining.min(height);
            screen.set_scrollback(screen.scrollback() + take);
            remaining -= take;
        }
        screen.set_scrollback(0);
        let alternate_screen = screen.alternate_screen();
        if !alternate_screen
            && !history.truncated
            && let Some(older) = &self.parser.callbacks().older_rows
        {
            older.append(&mut history);
        }
        let mut snapshot = history.finish();
        snapshot.alternate_screen = Some(alternate_screen);
        Ok(snapshot)
    }

    pub(crate) fn render(
        &self,
        previous: Option<&ScreenSnapshot>,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        self.render_frame(previous, true, outer, footer)
    }

    fn render_frame(
        &self,
        previous: Option<&ScreenSnapshot>,
        outer_paste: bool,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        #[cfg(feature = "ghostty-vt")]
        let screen = match &self.ghostty {
            Some(engine) => engine.screen()?,
            None => self.parser.screen(),
        };
        #[cfg(not(feature = "ghostty-vt"))]
        let screen = self.parser.screen();
        let previous = previous.map(|snapshot| &snapshot.0);
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
            frame.retain(|ch| !is_bidi_control(ch));
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

pub(super) fn is_bidi_control(ch: char) -> bool {
    matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    fn clipboard_sequence(text: &[u8], terminator: &[u8]) -> Vec<u8> {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let mut sequence = b"\x1b]52;c;".to_vec();
        sequence.extend(STANDARD.encode(text).bytes());
        sequence.extend(terminator);
        sequence
    }

    #[test]
    fn copies_complete_unicode_clipboard_text_across_chunks() {
        let mut terminal = TerminalScreen::new(Viewport::new(40, 5).unwrap());
        terminal.configure_clipboard(true, false);
        let text = "한글 🐦\nsecond line\n".repeat(300);
        for chunk in clipboard_sequence(text.as_bytes(), b"\x1b\\").chunks(7) {
            assert!(terminal.process(chunk).unwrap().is_empty());
        }
        assert_eq!(terminal.take_clipboard().as_deref(), Some(text.as_str()));
        assert!(terminal.take_clipboard().is_none());

        terminal.process(b"\x1b]52;;aGVsbG8=\x07").unwrap();
        assert_eq!(terminal.take_clipboard().as_deref(), Some("hello"));
        terminal.process(b"visible").unwrap();
        assert_eq!(terminal.parser.screen().contents(), "visible");
    }

    #[test]
    fn ignores_disabled_invalid_and_stale_clipboard_requests() {
        let mut terminal = TerminalScreen::new(Viewport::new(40, 5).unwrap());
        let valid = b"\x1b]52;c;aGVsbG8=\x07";
        terminal.process(valid).unwrap();
        assert!(terminal.take_clipboard().is_none());
        terminal.configure_clipboard(true, false);
        for invalid in [
            &b"\x1b]52;c;?\x07"[..],
            &b"\x1b]52;p;aGVsbG8=\x07"[..],
            &b"\x1b]52;c;aGVsbG8\x07"[..],
            &b"\x1b]52;c;aGVs\x00bG8=\x07"[..],
            &b"\x1b]52;c;aGVsbG8=\x18"[..],
            &b"\x1b]52;c;aGVsbG8=\x1b[0m"[..],
            &b"\x1b]52;c;/w==\x07"[..],
            &b"\x1b]52;c;AA==\x07"[..],
            &b"\x1b]52;c;\x07"[..],
        ] {
            assert!(terminal.process(invalid).unwrap().is_empty());
            assert!(terminal.take_clipboard().is_none());
        }
        terminal.process(&valid[..10]).unwrap();
        terminal.configure_clipboard(true, true);
        terminal.process(&valid[10..]).unwrap();
        assert!(terminal.take_clipboard().is_none());

        terminal.process(b"\x1b]52;c;aGVsbG8=\x1b").unwrap();
        terminal.configure_clipboard(true, true);
        terminal.process(b"\\").unwrap();
        assert!(terminal.take_clipboard().is_none());
        terminal.process(valid).unwrap();
        assert_eq!(terminal.take_clipboard().as_deref(), Some("hello"));
    }

    #[test]
    fn enforces_clipboard_limit_without_copying_a_truncated_prefix() {
        use corvid_protocol::CLIPBOARD_WRITE_MAX_BYTES;

        let mut terminal = TerminalScreen::new(Viewport::new(40, 5).unwrap());
        terminal.configure_clipboard(true, false);
        for length in [
            CLIPBOARD_WRITE_MAX_BYTES + 1,
            CLIPBOARD_WRITE_MAX_BYTES * 2,
            CLIPBOARD_WRITE_MAX_BYTES,
        ] {
            let text = vec![b'x'; length];
            terminal
                .process(&clipboard_sequence(&text, b"\x07"))
                .unwrap();
            let expected =
                (length == CLIPBOARD_WRITE_MAX_BYTES).then(|| String::from_utf8(text).unwrap());
            assert_eq!(terminal.take_clipboard(), expected);
        }
    }

    use super::*;
    use std::num::NonZeroU16;
    use vt100::Color;

    fn terminal(cols: u16, rows: u16) -> TerminalScreen {
        TerminalScreen::new(Viewport::new(cols, rows).unwrap())
    }

    #[test]
    fn captures_fragmented_osc_titles_without_changing_terminal_cells() {
        let mut terminal = terminal(80, 24);
        terminal.process(b"content").unwrap();
        for byte in "\x1b]0; 한글; build 🐦 \x07".as_bytes() {
            terminal.process(&[*byte]).unwrap();
        }
        assert_eq!(terminal.title(), " 한글; build 🐦 ");
        assert_eq!(terminal.parser.screen().contents(), "content");
        terminal.process(b"\x1b]2;next").unwrap();
        assert_eq!(terminal.title(), " 한글; build 🐦 ");
        terminal.process(b"\x1b\\").unwrap();
        assert_eq!(terminal.title(), "next");
        terminal
            .process(b"\x1b]1;icon only\x07\x1b]52;c;clipboard\x07")
            .unwrap();
        assert_eq!(terminal.title(), "next");
        terminal.process(b"\x1b]2;\x07").unwrap();
        assert_eq!(terminal.title(), "");
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
    fn redraw_preserves_fragmented_colon_rgb_and_later_sgr_parameters() {
        let viewport = Viewport::new(80, 24).unwrap();
        let mut child = TerminalScreen::new(viewport);
        let ansi = b"\x1b[2;38:2::239:229:219;48:2:0:216:226:236;4mA\x1b[38:2::240:230:220;39;22mB";
        for byte in ansi {
            child.process(&[*byte]).unwrap();
        }

        let mut client = Parser::new(viewport.rows, viewport.cols, 0);
        client.process(child.redraw(viewport).unwrap().as_bytes());
        let a = client.screen().cell(0, 0).unwrap();
        assert_eq!(a.contents(), "A");
        assert_eq!(a.fgcolor(), Color::Rgb(239, 229, 219));
        assert_eq!(a.bgcolor(), Color::Rgb(216, 226, 236));
        assert!(a.dim());
        assert!(a.underline());

        let b = client.screen().cell(0, 1).unwrap();
        assert_eq!(b.contents(), "B");
        assert_eq!(b.fgcolor(), Color::Default);
        assert_eq!(b.bgcolor(), a.bgcolor());
        assert!(!b.dim());
    }

    #[test]
    fn rgb_normalization_respects_csi_cancellation_and_osc_titles() {
        let mut screen = terminal(80, 24);
        screen
            .process(b"\x1b[31m\x1b[38:2::239:229:219\x18m\x1b]2;38:2::240:230:220m\x07A")
            .unwrap();

        assert_eq!(screen.title(), "38:2::240:230:220m");
        let snapshot = screen.parser.screen();
        assert_eq!(snapshot.contents(), "mA");
        assert_eq!(snapshot.cell(0, 1).unwrap().fgcolor(), Color::Idx(1));
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
            outer.parser.screen().cursor_position(),
            inner.parser.screen().cursor_position()
        );
        assert_eq!(outer.parser.screen().fgcolor(), Color::Idx(1));

        let previous = inner.snapshot().unwrap();
        inner.process(b"Z").unwrap();
        let diff = inner.render(Some(&previous), viewport, None).unwrap();
        outer.process(diff.as_bytes()).unwrap();
        let content = inner.parser.screen();
        let composed = outer.parser.screen();
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

        let previous = inner.snapshot().unwrap();
        assert_eq!(outer.parser.screen().cell(1, 0), previous.0.cell(1, 0));
        assert_eq!(
            outer.parser.screen().cell(3, 8).unwrap().bgcolor(),
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
    fn history_contains_scrolled_rows_in_order_and_preserves_live_screen() {
        let mut terminal = terminal(20, 3);
        for row in 0..10 {
            terminal
                .process(format!("line {row}\r\n").as_bytes())
                .unwrap();
        }
        let live = terminal.parser.screen().contents();
        let history = terminal.history().unwrap();
        assert_eq!(
            history.lines,
            (0..10)
                .map(|row| format!("line {row}"))
                .chain(std::iter::once(String::new()))
                .collect::<Vec<_>>()
        );
        assert!(!history.truncated);
        assert_eq!(terminal.parser.screen().contents(), live);
        assert_eq!(terminal.parser.screen().scrollback(), 0);
    }

    #[test]
    fn live_history_keeps_older_rows_outside_the_render_snapshot() {
        let mut terminal = TerminalScreen::live(Viewport::new(20, 4).unwrap());
        let rows = (0..600)
            .map(|row| format!("turn {row} 한"))
            .collect::<Vec<_>>();
        terminal.process(rows.join("\r\n").as_bytes()).unwrap();
        assert_eq!(terminal.parser.screen().scrollback_len(), 128);
        let history = terminal.history().unwrap();
        assert_eq!(history.lines, rows);
        assert!(!history.truncated);

        terminal
            .process(b"\x1b[?1049h\x1b[3J\x1b[?1000h\x1b[?1006h")
            .unwrap();
        assert_eq!(terminal.wheel_encoding(), WheelEncoding::Sgr);
        assert!(!terminal.history().unwrap().lines.contains(&rows[0]));
        terminal.process(b"\x1b[?1000l\x1b[?1049l").unwrap();
        assert_eq!(terminal.wheel_encoding(), WheelEncoding::Disabled);
        assert_eq!(terminal.history().unwrap().lines, rows);

        terminal.process(b"\x1b[3").unwrap();
        terminal.process(b"Jafter").unwrap();
        assert_eq!(terminal.history().unwrap().lines.len(), 4);
    }

    #[test]
    fn shell_history_observes_prompt_markers_and_terminal_modes() {
        let mut terminal = TerminalScreen::new(Viewport::new(80, 24).unwrap());
        assert!(terminal.allows_shell_history());
        terminal.process(b"\x1b]133;").unwrap();
        terminal.process(b"B\x07").unwrap();
        assert_eq!(terminal.shell_prompt(), Some(true));
        terminal.process(b"\x1b]633;C\x1b\\").unwrap();
        assert!(!terminal.allows_shell_history());
        terminal.process(b"\x1b]633;D;0\x07\x1b]633;A\x07").unwrap();
        assert!(terminal.allows_shell_history());
        terminal.process(b"\x1b[?1049h").unwrap();
        assert!(!terminal.allows_shell_history());
        terminal.process(b"\x1b[?1049l\x1b[?1000h").unwrap();
        assert!(!terminal.allows_shell_history());
        terminal.process(b"\x1b[?1000l").unwrap();
        assert!(terminal.allows_shell_history());
    }

    #[test]
    fn history_preserves_rendition_changes_in_archived_and_recent_rows() {
        let mut terminal = TerminalScreen::live(Viewport::new(30, 4).unwrap());
        for i in 0..301 {
            terminal
                .process(
                    format!("row{i:03} \x1b[1;3;4;7;38;2;10;20;30;48;5;12m한글\x1b[0m end\r\n")
                        .as_bytes(),
                )
                .unwrap();
        }
        let history = terminal.history().unwrap();
        for row in [0, 300] {
            let text = &history.lines[row];
            assert_eq!(text, &format!("row{row:03} 한글 end"));
            let mut parser = Parser::new(1.try_into().unwrap(), 30.try_into().unwrap(), 0);
            let mut from = 0;
            for change in history.styles.iter().filter(|s| s.row as usize == row) {
                let next = change.offset as usize;
                parser.process(&text.as_bytes()[from..next]);
                parser.process(format!("\x1b[{}m", change.sgr).as_bytes());
                from = next;
            }
            parser.process(&text.as_bytes()[from..]);
            let color = parser.screen().cell(0, 7).unwrap();
            assert_eq!(color.fgcolor(), vt100::Color::Rgb(10, 20, 30));
            assert_eq!(color.bgcolor(), vt100::Color::Idx(12));
            assert!(color.bold() && color.italic() && color.underline() && color.inverse());
            assert_eq!(
                parser.screen().cell(0, 12).unwrap().fgcolor(),
                vt100::Color::Default
            );
        }
    }

    #[test]
    fn live_history_reports_eviction_and_reset_discards_archived_text() {
        let mut terminal = TerminalScreen::live(Viewport::new(24, 4).unwrap());
        for row in 0..3000 {
            terminal
                .process(format!("row {row}\r\n").as_bytes())
                .unwrap();
        }
        let history = terminal.history().unwrap();
        assert!(history.truncated);
        assert_eq!(history.lines.len(), TERMINAL_HISTORY_MAX_ROWS);
        assert_eq!(
            history.lines[0],
            format!("row {}", 3001 - TERMINAL_HISTORY_MAX_ROWS)
        );
        terminal.process(b"\x1bchello").unwrap();
        let history = terminal.history().unwrap();
        assert!(!history.truncated);
        assert_eq!(history.lines, ["hello", "", "", ""]);
    }

    #[test]
    fn history_limits_large_scrollback_before_encoding() {
        use prost::Message;

        let mut terminal = terminal(400, 30);
        let line = format!("{}\r\n", "x".repeat(400));
        for _ in 0..700 {
            terminal.process(line.as_bytes()).unwrap();
        }
        terminal.process(b"newest").unwrap();
        let history = terminal.history().unwrap();
        assert!(history.truncated);
        assert_eq!(history.lines.last().map(String::as_str), Some("newest"));
        assert!(history.encoded_len() < TERMINAL_HISTORY_MAX_BYTES);
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

        let previous = inner.snapshot().unwrap();
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
