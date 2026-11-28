use anyhow::Result;
use corvid_core::Viewport;
use corvid_protocol::wire::{TerminalHistory, WheelEncoding};
#[cfg(test)]
use corvid_protocol::{TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS};

#[cfg(test)]
mod benchmark;
mod ghostty;
#[cfg(test)]
mod ghostty_qualification;
mod history;
mod sgr;
mod snapshot;

pub(crate) use corvid_protocol::ansi_screen::Width as CellWidth;
pub(crate) use snapshot::ScreenSnapshot;

const MAX_REPLY_BYTES: usize = 32 * 1024;
const DEVICE_ATTRIBUTES: &str = "\x1b[?6c";

pub struct TerminalScreen {
    engine: ghostty::Ghostty,
    sgr: sgr::Normalizer,
}

impl TerminalScreen {
    pub fn new(viewport: Viewport) -> Result<Self> {
        Ok(Self {
            engine: ghostty::Ghostty::new(viewport)?,
            sgr: sgr::Normalizer::default(),
        })
    }

    pub(super) fn title(&self) -> &str {
        &self.engine.title
    }

    pub(super) fn reported_directory(&self) -> Option<corvid_protocol::wire::ReportedDirectory> {
        self.engine.reported_directory.clone()
    }

    pub fn process(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut replies = Vec::new();
        let mut result = Ok(());
        self.sgr.process(bytes, |chunk| {
            if result.is_ok() {
                result = self.engine.process(chunk, &mut replies);
            }
        });
        result?;
        let program_replies = self.sgr.take_program_replies();
        anyhow::ensure!(
            replies.len() + program_replies.len() <= MAX_REPLY_BYTES,
            "terminal query reply limit exceeded"
        );
        replies.extend(program_replies);
        Ok(replies)
    }

    pub(super) fn configure_clipboard(&mut self, enabled: bool, reset: bool) {
        self.sgr.configure_clipboard(enabled, reset);
    }

    pub(super) fn set_colors(
        &mut self,
        colors: &corvid_protocol::wire::TerminalColors,
    ) -> Result<()> {
        self.engine.set_colors(colors)
    }

    pub(super) fn take_clipboard(&mut self) -> Option<String> {
        self.sgr.take_clipboard()
    }

    pub(super) fn program_status(&self) -> Option<corvid_protocol::wire::ProgramStatus> {
        self.sgr.program_status()
    }

    pub(super) fn expire_program_status(&mut self) {
        self.sgr.expire_program_status();
    }

    #[cfg(target_os = "linux")]
    pub(super) fn compress_idle(&mut self, now: std::time::Instant) {
        self.engine.compress_idle(now);
    }

    pub fn resize(&mut self, viewport: Viewport) -> Result<()> {
        self.engine.resize(viewport)
    }

    pub fn redraw(&self, outer: Viewport) -> Result<String> {
        self.snapshot()?
            .encode(None, self.bracketed_paste(), outer, None)
    }

    pub(crate) fn viewport(&self) -> Viewport {
        self.engine.viewport
    }

    pub(super) fn bracketed_paste(&self) -> bool {
        self.engine.bracketed_paste()
    }

    pub(super) fn shell_prompt(&self) -> Option<bool> {
        self.engine.shell_prompt()
    }

    pub(super) fn allows_shell_history(&self) -> bool {
        !self.alternate_screen()
            && self.shell_prompt() != Some(false)
            && self.wheel_encoding() == WheelEncoding::Disabled
    }

    pub(super) fn alternate_screen(&self) -> bool {
        self.engine.alternate_screen()
    }

    pub(super) fn wheel_encoding(&self) -> WheelEncoding {
        self.engine.wheel_encoding()
    }

    pub(super) fn snapshot(&self) -> Result<ScreenSnapshot> {
        self.engine.screen().map(ScreenSnapshot)
    }

    pub(super) fn history(&mut self) -> Result<TerminalHistory> {
        self.engine.history()
    }

    pub(crate) fn render(
        &self,
        previous: Option<&ScreenSnapshot>,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        self.snapshot()?.encode(previous, true, outer, footer)
    }
}

pub(super) fn is_bidi_control(ch: char) -> bool {
    matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    fn clipboard_sequence(text: &[u8], terminator: &[u8]) -> Vec<u8> {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let mut sequence = b"\x1b]52;c;".to_vec();
        sequence.extend(STANDARD.encode(text).bytes());
        sequence.extend(terminator);
        sequence
    }

    #[test]
    fn program_status_prefers_a_blocked_child_and_restores_progress_after_reset() {
        let mut terminal = terminal(80, 24);
        terminal.process(b"\x1b]9;4;1;35\x07").unwrap();
        assert_eq!(terminal.program_status().unwrap().progress, Some(35));
        terminal
            .process(b"\x1b]7501;state=idle:app=build\x07")
            .unwrap();
        for bytes in
            b"\x1b]7501;state=blocked:id=check/tests:kind=auth:msg=c2lnbiBpbg\x1b\\".chunks(3)
        {
            terminal.process(bytes).unwrap();
        }
        let blocked = terminal.program_status().unwrap();
        assert_eq!(blocked.label(), "App waiting for login");
        assert_eq!(blocked.message, "sign in");
        assert_eq!(blocked.app, "build");
        terminal
            .process(b"\x1b]9;4;1;80\x07\x1b]7501;state=clear:id=check\x07")
            .unwrap();
        assert_eq!(terminal.program_status().unwrap().label(), "App idle");

        terminal
            .process(b"\x1b]7501;state=working:id=test\x07\x1b]133;A\x07")
            .unwrap();
        assert_eq!(terminal.program_status().unwrap().label(), "App idle");
        terminal
            .process(b"\x1b]7501;state=done:id=test\x07\x1b]133;A\x07")
            .unwrap();
        assert_eq!(terminal.program_status().unwrap().label(), "App finished");
        terminal.process(b"\x1bc\x1b]9;4;1;80\x07").unwrap();
        assert_eq!(terminal.program_status().unwrap().progress, Some(80));
    }

    #[test]
    fn program_status_rejects_invalid_reports_and_replies_once_on_complete_queries() {
        let mut terminal = terminal(80, 24);
        assert!(terminal.process(b"\x1b]7501;?\x1b").unwrap().is_empty());
        let reply = terminal.process(b"\\").unwrap();
        assert_eq!(reply, b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\");
        terminal.process(b"\x1b]7501;state=done\x07").unwrap();
        terminal
            .process(b"\x1b]7501;state=blocked:id=/bad\x07")
            .unwrap();
        terminal
            .process(b"\x1b]7501;state=blocked:msg=AA==\x07")
            .unwrap();
        terminal.process(b"\x1b]7501;state=blocked\x1b[0m").unwrap();
        let oversized = format!("\x1b]7501;state=blocked:msg={}\x07", "A".repeat(4096));
        terminal.process(oversized.as_bytes()).unwrap();
        assert_eq!(terminal.program_status().unwrap().label(), "App finished");
    }

    #[test]
    fn copies_complete_unicode_clipboard_text_across_chunks() {
        let mut terminal = TerminalScreen::new(Viewport::new(40, 5).unwrap()).unwrap();
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
        assert_eq!(terminal.engine.screen().unwrap().contents(), "visible");
    }

    #[test]
    fn ignores_disabled_invalid_and_stale_clipboard_requests() {
        let mut terminal = TerminalScreen::new(Viewport::new(40, 5).unwrap()).unwrap();
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

        let mut terminal = TerminalScreen::new(Viewport::new(40, 5).unwrap()).unwrap();
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
    use corvid_protocol::ansi_screen::Decoder;
    use std::num::NonZeroU16;
    use vt100::{Color, Parser};

    fn terminal(cols: u16, rows: u16) -> TerminalScreen {
        TerminalScreen::new(Viewport::new(cols, rows).unwrap()).unwrap()
    }

    #[test]
    fn captures_fragmented_osc_titles_without_changing_terminal_cells() {
        let mut terminal = terminal(80, 24);
        terminal.process(b"content").unwrap();
        for byte in "\x1b]0; 한글; build 🐦 \x07".as_bytes() {
            terminal.process(&[*byte]).unwrap();
        }
        assert_eq!(terminal.title(), " 한글; build 🐦 ");
        assert_eq!(terminal.engine.screen().unwrap().contents(), "content");
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

        let screen = terminal.engine.screen().unwrap();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "한");
        assert!(screen.cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(screen.cell(0, 2).unwrap().contents(), "글");
        assert_eq!(screen.cell(0, 5).unwrap().contents(), "e\u{301}");
    }

    #[test]
    fn redraw_reconstructs_grid_colors_and_input_modes() {
        let mut child = terminal(80, 24);
        child.process(b"original\x1b[?1049h\x1b[H").unwrap();
        child
            .process(b"\x1b[38;2;6;7;8m\x1b[48;5;42mA\x1b[?25l\x1b[?1h\x1b=\x1b[?2004h")
            .unwrap();

        let viewport = Viewport::new(80, 24).unwrap();
        let mut outer = Parser::new(viewport.rows, viewport.cols, 0);
        outer.process(b"stale\x1b[2;5r\x1b[?6h");
        let frame = child.redraw(Viewport::new(80, 24).unwrap()).unwrap();
        outer.process(frame.as_bytes());

        let screen = outer.screen();
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
        assert_eq!(child.engine.screen().unwrap().contents(), "original");
    }

    #[test]
    fn redraw_preserves_fragmented_colon_rgb_and_later_sgr_parameters() {
        let viewport = Viewport::new(80, 24).unwrap();
        let mut child = TerminalScreen::new(viewport).unwrap();
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
        let snapshot = screen.engine.screen().unwrap();
        assert_eq!(snapshot.contents(), "mA");
        assert_eq!(
            snapshot.cell(0, 1).unwrap().style.fg,
            corvid_protocol::ansi_screen::Color::Indexed(1)
        );
    }

    #[test]
    fn status_row_preserves_pending_wrap_and_drawing_attributes() {
        let mut inner = terminal(12, 4);
        inner
            .process(b"\x1b[?1049h\x1b[31m\x1b[4;1Habcdefghijkl")
            .unwrap();
        let viewport = Viewport::new(12, 5).unwrap();
        let mut outer = Decoder::new(viewport.rows, viewport.cols);
        let footer = (5, "\x1b[0;30;46m\x1b[2KCORVID");
        let full = inner.render(None, viewport, Some(footer)).unwrap();
        outer.process(full.as_bytes());
        assert_eq!(
            outer.screen().cursor_position(),
            inner.engine.screen().unwrap().cursor_position()
        );
        assert_eq!(
            outer.screen().cursor.style.fg,
            corvid_protocol::ansi_screen::Color::Indexed(1)
        );

        let previous = inner.snapshot().unwrap();
        inner.process(b"Z").unwrap();
        let diff = inner.render(Some(&previous), viewport, None).unwrap();
        outer.process(diff.as_bytes());
        let content = inner.engine.screen().unwrap();
        let composed = outer.screen();
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
        assert_eq!(
            composed.cell(4, 11).unwrap().style.bg,
            corvid_protocol::ansi_screen::Color::Indexed(6)
        );
        assert_eq!(composed.cursor_position(), content.cursor_position());
    }

    #[test]
    fn renders_wrapped_rows_and_diffs_inside_a_larger_outer_tty() {
        let viewport = Viewport::new(12, 8).unwrap();
        let mut inner = terminal(8, 4);
        let mut outer = Decoder::new(viewport.rows, viewport.cols);
        let footer = (8, "\x1b[0;30;46m\x1b[2KCORVID");
        outer.process(b"\x1b[6;1Hstale\x1b[1;9Hstale");
        inner
            .process(b"\x1b[31mabcdefghW\x1b[4;6H\x1b[46m\x1b[K")
            .unwrap();
        let frame = inner.render(None, viewport, Some(footer)).unwrap();
        outer.process(frame.as_bytes());

        let previous = inner.snapshot().unwrap();
        assert_eq!(outer.screen().cell(1, 0), previous.0.cell(1, 0));
        assert_eq!(
            outer.screen().cell(3, 8).unwrap().style.bg,
            corvid_protocol::ansi_screen::Color::Default
        );

        inner.process(b"\x1b[0m\x1b[4;1HijklmnopZ").unwrap();
        let frame = inner.render(Some(&previous), viewport, None).unwrap();
        outer.process(frame.as_bytes());

        let expected = inner.engine.screen().unwrap();
        let composed = outer.screen();
        for row in 0..4 {
            for col in 0..8 {
                assert_eq!(composed.cell(row, col), expected.cell(row, col));
            }
            assert!(!composed.cell(row, 8).unwrap().has_contents());
            assert_eq!(
                composed.cell(row, 8).unwrap().style.bg,
                corvid_protocol::ansi_screen::Color::Default
            );
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
            terminal.engine.screen().unwrap().contents(),
            "top\nthird\nnew\nbottom"
        );

        terminal.process(b"\x1b[2;1H\x1b[K").unwrap();
        assert_eq!(
            terminal.engine.screen().unwrap().contents(),
            "top\n\nnew\nbottom"
        );
    }

    #[test]
    fn replies_to_child_queries_using_the_resized_inner_screen() {
        let mut terminal = terminal(80, 24);
        terminal.resize(Viewport::new(300, 120).unwrap()).unwrap();
        terminal.process(b"\x1b[120;300H\x1b[").unwrap();
        let replies = terminal.process(b"6n\x1b[5n\x1b[c\x1b[18t").unwrap();
        assert_eq!(replies, b"\x1b[120;300R\x1b[0n\x1b[?6c\x1b[8;120;300t");

        terminal.resize(Viewport::new(2, 2).unwrap()).unwrap();
        assert_eq!(terminal.process(b"\x1b[18t").unwrap(), b"\x1b[8;2;2t");
    }

    #[test]
    fn bounds_scrollback_and_long_cell_text_without_replaying_osc() {
        let mut terminal = terminal(10, 2);
        for _ in 0..TERMINAL_HISTORY_MAX_ROWS * 2 {
            terminal.process(b"line\r\n").unwrap();
        }
        assert_eq!(
            terminal.history().unwrap().lines.len(),
            TERMINAL_HISTORY_MAX_ROWS
        );

        terminal.process(b"\x1b[HA").unwrap();
        for _ in 0..8 {
            terminal.process("\u{301}".repeat(4096).as_bytes()).unwrap();
        }
        let screen = terminal.engine.screen().unwrap();
        assert!(screen.cell(0, 0).unwrap().contents().len() <= 96);

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
        let live = terminal.engine.screen().unwrap().contents();
        let history = terminal.history().unwrap();
        assert_eq!(
            history.lines,
            (0..10)
                .map(|row| format!("line {row}"))
                .chain(std::iter::once(String::new()))
                .collect::<Vec<_>>()
        );
        assert!(!history.truncated);
        assert_eq!(terminal.engine.screen().unwrap().contents(), live);
    }

    #[test]
    fn live_history_keeps_older_rows_outside_the_render_snapshot() {
        let mut terminal = TerminalScreen::new(Viewport::new(20, 4).unwrap()).unwrap();
        let rows = (0..600)
            .map(|row| format!("turn {row} 한"))
            .collect::<Vec<_>>();
        terminal.process(rows.join("\r\n").as_bytes()).unwrap();
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
        let mut terminal = TerminalScreen::new(Viewport::new(80, 24).unwrap()).unwrap();
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
        let mut terminal = TerminalScreen::new(Viewport::new(30, 4).unwrap()).unwrap();
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
        let mut terminal = TerminalScreen::new(Viewport::new(24, 4).unwrap()).unwrap();
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
        let viewport = Viewport::new(80, 24).unwrap();
        let mut outer = Decoder::new(viewport.rows, viewport.cols);
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
        outer.process(full.as_bytes());
        assert_eq!(outer.screen().contents(), "XXXXXXXXXXXX 한글 e\u{301} שלום");
        assert_eq!(
            outer.screen().cursor_position(),
            inner.engine.screen().unwrap().cursor_position()
        );

        let previous = inner.snapshot().unwrap();
        inner
            .process("\x1b[2;1Hvisible\u{202e} update".as_bytes())
            .unwrap();
        let diff = inner.render(Some(&previous), viewport, None).unwrap();
        assert!(!diff.contains('\u{202e}'));
        outer.process(diff.as_bytes());
        assert_eq!(
            outer.screen().contents(),
            "XXXXXXXXXXXX 한글 e\u{301} שלום\nvisible update"
        );
        assert_eq!(
            outer.screen().cursor_position(),
            inner.engine.screen().unwrap().cursor_position()
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
        let mut outer = Decoder::new(NonZeroU16::new(120).unwrap(), NonZeroU16::new(300).unwrap());
        outer.process(frame.as_bytes());
        let inner = terminal.engine.screen().unwrap();
        for line in 0..120 {
            for col in 0..300 {
                assert_eq!(inner.cell(line, col), outer.screen().cell(line, col));
            }
        }
    }

    #[test]
    fn preserves_osc8_hyperlinks_in_live_frames_and_scrollback_history() {
        let mut terminal = TerminalScreen::new(Viewport::new(24, 3).unwrap()).unwrap();
        terminal
            .process(
                b"\x1b]8;;https://example.test/doc\x1b\\open docs\x1b]8;;\x1b\\ plain\r\nsecond\r\nthird\r\nfourth",
            )
            .unwrap();

        let history = terminal.history().unwrap();
        assert_eq!(history.hyperlinks.len(), 1);
        assert_eq!(history.hyperlinks[0].row, 0);
        assert_eq!(history.hyperlinks[0].start_offset, 0);
        assert_eq!(history.hyperlinks[0].end_offset, 9);
        assert_eq!(history.hyperlinks[0].uri, "https://example.test/doc");

        let mut live = TerminalScreen::new(Viewport::new(24, 3).unwrap()).unwrap();
        live.process(b"\x1b]8;;https://example.test/live\x1b\\click\x1b]8;;\x1b\\")
            .unwrap();
        let frame = live.redraw(Viewport::new(24, 3).unwrap()).unwrap();
        assert!(frame.contains("\x1b]8;;https://example.test/live\x1b\\"));
        assert!(frame.contains("\x1b]8;;\x1b\\"));

        let decoded =
            ScreenSnapshot::from_ansi(Viewport::new(24, 3).unwrap(), frame.as_bytes(), None);
        assert_eq!(
            decoded
                .cell(0, 0)
                .unwrap()
                .hyperlink()
                .map(|l| l.uri.as_str()),
            Some("https://example.test/live")
        );
        assert_eq!(decoded.cell(0, 5).unwrap().hyperlink(), None);
    }

    #[test]
    fn answers_osc_10_11_and_4_color_queries_and_updates_with_client_colors() {
        let mut terminal = terminal(80, 24);
        let reply = terminal
            .process(b"\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]4;0;?\x07\x1b]4;196;?\x1b\\")
            .unwrap();
        let text = String::from_utf8(reply).unwrap();
        assert!(text.contains("\x1b]10;rgb:e4e4/e4e4/e7e7"), "{text:?}");
        assert!(text.contains("\x1b]11;rgb:1111/1111/1313"), "{text:?}");
        assert!(text.contains("\x1b]4;0;rgb:0000/0000/0000"), "{text:?}");
        assert!(text.contains("\x1b]4;196;rgb:ffff/0000/0000"), "{text:?}");

        let mut palette = corvid_protocol::DEFAULT_ANSI_PALETTE_16.to_vec();
        palette[0] = 0x11_22_33;
        terminal
            .set_colors(&corvid_protocol::wire::TerminalColors {
                foreground_rgb: Some(0x10_20_30),
                background_rgb: Some(0xf5_f6_f7),
                ansi_palette: palette,
            })
            .unwrap();

        let updated = terminal
            .process(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;0;?\x07")
            .unwrap();
        let updated_text = String::from_utf8(updated).unwrap();
        assert!(
            updated_text.contains("\x1b]10;rgb:1010/2020/3030"),
            "{updated_text:?}"
        );
        assert!(
            updated_text.contains("\x1b]11;rgb:f5f5/f6f6/f7f7"),
            "{updated_text:?}"
        );
        assert!(
            updated_text.contains("\x1b]4;0;rgb:1111/2222/3333"),
            "{updated_text:?}"
        );
    }

    #[test]
    fn tracks_osc7_osc9_9_and_osc633_cwd_and_clears_on_reset() {
        let mut terminal = terminal(80, 24);
        assert_eq!(terminal.reported_directory(), None);

        terminal
            .process(b"\x1b]7;file://prod-box/srv/my%20app\x07")
            .unwrap();
        let reported = terminal.reported_directory().unwrap();
        assert_eq!(reported.host.as_deref(), Some("prod-box"));
        assert_eq!(reported.path, "/srv/my app");

        terminal.process(b"\x1b]9;9;/tmp/win-style\x1b\\").unwrap();
        let reported = terminal.reported_directory().unwrap();
        assert_eq!(reported.host, None);
        assert_eq!(reported.path, "/tmp/win-style");

        terminal
            .process(b"\x1b]633;P;Cwd=/Users/dev/corvid\x07")
            .unwrap();
        let reported = terminal.reported_directory().unwrap();
        assert_eq!(reported.host, None);
        assert_eq!(reported.path, "/Users/dev/corvid");

        terminal.process(b"\x1bc").unwrap();
        assert_eq!(terminal.reported_directory(), None);
    }
}
