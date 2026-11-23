use ghostty_vt::render::{CellIterator, RenderState, RowIterator};
use ghostty_vt::screen::{CellWide, Screen};
use ghostty_vt::style::{RgbColor, StyleColor};
use ghostty_vt::terminal::{ClipboardWritePolicy, Event, Mode, Options, Terminal};

use super::*;

fn ghostty(viewport: Viewport) -> Terminal {
    let mut terminal = Terminal::new(Options {
        cols: viewport.cols.get(),
        rows: viewport.rows.get(),
        max_scrollback: TERMINAL_HISTORY_MAX_BYTES,
    })
    .unwrap();
    terminal
        .set_scrollback_max_lines(Some(TERMINAL_HISTORY_MAX_ROWS))
        .unwrap();
    terminal.set_clipboard_write_policy(ClipboardWritePolicy::Deny);
    terminal
}

fn query_replies(terminal: &mut Terminal) -> Vec<u8> {
    terminal
        .events()
        .filter_map(|event| match event {
            Event::PtyWrite(bytes) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn reads_fragmented_unicode_attributes_and_cursor_from_render_state() {
    let viewport = Viewport::new(12, 4).unwrap();
    let mut terminal = ghostty(viewport);
    let mut current = TerminalScreen::new(viewport);
    let payload = "\x1b[1;38;2;10;20;30m한e\u{301}\x1b[0m\x1b[?25l";
    for chunk in payload.as_bytes().chunks(2) {
        terminal.vt_write(chunk);
        current.process(chunk).unwrap();
    }

    let dimensions = current.snapshot().unwrap().size();
    let mut state = RenderState::new().unwrap();
    let snapshot = state.update(&terminal).unwrap();
    assert_eq!(snapshot.rows().unwrap(), dimensions.0.get());
    assert_eq!(snapshot.cols().unwrap(), dimensions.1.get());
    assert_eq!(
        snapshot.cursor_visible().unwrap(),
        !current.snapshot().unwrap().hide_cursor()
    );
    let cursor = snapshot.cursor_viewport().unwrap().unwrap();
    assert_eq!(
        (cursor.y, cursor.x),
        current.snapshot().unwrap().cursor_position()
    );

    let mut rows = RowIterator::new().unwrap();
    let mut cells = CellIterator::new().unwrap();
    let mut viewport_rows = rows.update(&snapshot).unwrap();
    let mut first = cells.update(viewport_rows.next().unwrap()).unwrap();
    let mut text = String::new();
    let wide = first.next().unwrap();
    wide.graphemes_utf8(&mut text).unwrap();
    assert_eq!(text, "한");
    assert_eq!(wide.raw_cell().unwrap().wide().unwrap(), CellWide::Wide);
    let style = wide.style().unwrap();
    assert!(style.bold);
    assert_eq!(
        style.fg_color,
        StyleColor::Rgb(RgbColor {
            r: 10,
            g: 20,
            b: 30
        })
    );
    let spacer = first.next().unwrap();
    assert_eq!(
        spacer.raw_cell().unwrap().wide().unwrap(),
        CellWide::SpacerTail
    );
    text.clear();
    first.next().unwrap().graphemes_utf8(&mut text).unwrap();
    assert_eq!(text, "e\u{301}");
}

#[test]
fn reports_resized_inner_geometry_and_cursor_to_the_child() {
    let viewport = Viewport::new(16, 6).unwrap();
    let resized = Viewport::new(20, 8).unwrap();
    let mut terminal = ghostty(viewport);
    let mut current = TerminalScreen::new(viewport);
    terminal
        .resize(resized.cols.get(), resized.rows.get(), 0, 0)
        .unwrap();
    current.resize(resized);
    let queries = b"\x1b[4;10H\x1b[6n\x1b[18t";
    terminal.vt_write(queries);
    let expected = current.process(queries).unwrap();
    assert_eq!(query_replies(&mut terminal), expected);
    assert_eq!(expected, b"\x1b[4;10R\x1b[8;8;20t");
}

#[test]
fn tracks_modes_alternate_screen_and_scrollback_without_clipboard_events() {
    let mut terminal = ghostty(Viewport::new(8, 3).unwrap());
    terminal.vt_write(b"A\r\nB\r\nC\r\nD\x1b[?1h\x1b[?2004h");
    assert_eq!(terminal.scrollback_rows().unwrap(), 1);
    assert!(terminal.mode(Mode::BRACKETED_PASTE).unwrap());
    assert!(
        terminal
            .mode(Mode::new(1, ghostty_vt::terminal::ModeKind::Dec))
            .unwrap()
    );
    terminal.vt_write(b"\x1b[?1049hinner\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(terminal.active_screen().unwrap(), Screen::Alternate);
    assert!(
        !terminal
            .events()
            .any(|event| matches!(event, Event::ClipboardWrite(_)))
    );
    terminal.vt_write(b"\x1b[?1049l");
    assert_eq!(terminal.active_screen().unwrap(), Screen::Primary);
    assert_eq!(terminal.scrollback_rows().unwrap(), 1);
}

#[test]
fn keeps_pending_wrap_and_distinguishes_it_from_the_last_column() {
    let mut terminal = ghostty(Viewport::new(4, 3).unwrap());
    terminal.vt_write(b"1234");
    assert_eq!(terminal.cursor_x().unwrap(), 3);
    assert!(terminal.is_cursor_pending_wrap().unwrap());
    terminal.vt_write(b"\x1b[1;4H");
    assert_eq!(terminal.cursor_x().unwrap(), 3);
    assert!(!terminal.is_cursor_pending_wrap().unwrap());
}

#[test]
fn adapter_renders_full_frames_and_diffs_against_each_view_baseline() {
    let viewport = Viewport::new(12, 4).unwrap();
    let mut terminal = TerminalScreen::with_ghostty(viewport).unwrap();
    let sequence = "\x1b[1;38;2;10;20;30m한e\u{301}\x1b[0m\x1b[?1h\x1b[?25l";
    for byte in sequence.as_bytes() {
        terminal.process(std::slice::from_ref(byte)).unwrap();
    }
    let first = terminal.snapshot().unwrap();
    let mut viewer = vt100::Parser::new(viewport.rows, viewport.cols, 0);
    viewer.process(terminal.redraw(viewport).unwrap().as_bytes());
    let screen = viewer.screen();
    assert!(ScreenSnapshot::from_screen(screen).matches(&first));
    assert_eq!(screen.cell(0, 0).unwrap().contents(), "한");
    assert_eq!(
        screen.cell(0, 0).unwrap().fgcolor(),
        vt100::Color::Rgb(10, 20, 30)
    );
    assert!(screen.cell(0, 1).unwrap().is_wide_continuation());
    assert_eq!(screen.cell(0, 2).unwrap().contents(), "e\u{301}");
    assert!(screen.application_cursor());
    assert!(screen.hide_cursor());

    terminal.process(b"\x1b[2;1Hintermediate").unwrap();
    let intermediate = terminal.snapshot().unwrap();
    let mut faster = vt100::Parser::new(viewport.rows, viewport.cols, 0);
    faster.process(terminal.redraw(viewport).unwrap().as_bytes());

    terminal.process(b"\x1b[2;1Hfinal\x1b[K\x1b[?25h").unwrap();
    viewer.process(
        terminal
            .render(Some(&first), viewport, None)
            .unwrap()
            .as_bytes(),
    );
    faster.process(
        terminal
            .render(Some(&intermediate), viewport, None)
            .unwrap()
            .as_bytes(),
    );
    assert_eq!(viewer.screen().contents(), "한e\u{301}\nfinal");
    assert_eq!(faster.screen().contents(), viewer.screen().contents());
    assert_eq!(
        faster.screen().cursor_position(),
        viewer.screen().cursor_position()
    );
    assert!(!viewer.screen().hide_cursor());
}

#[test]
fn adapter_uses_ghostty_reflow_and_keeps_history_with_styles() {
    let viewport = Viewport::new(6, 3).unwrap();
    let mut terminal = TerminalScreen::with_ghostty(viewport).unwrap();
    terminal
        .process(b"\x1b[31mabcdefgh\r\n\x1b[0mnext\r\nlast")
        .unwrap();
    let history = terminal.history().unwrap();
    assert_eq!(history.lines, ["abcdef", "gh", "next", "last"]);
    assert!(
        history
            .styles
            .iter()
            .any(|style| style.sgr.contains(";38;5;1"))
    );
    terminal
        .resize_session(Viewport::new(8, 4).unwrap())
        .unwrap();
    let history = terminal.history().unwrap();
    assert!(history.lines.iter().any(|line| line == "abcdefgh"));
    terminal.process(b"\x1b[?1049hinner").unwrap();
    assert!(terminal.alternate_screen());
    assert_eq!(terminal.history().unwrap().alternate_screen, Some(true));
    assert!(
        !terminal
            .history()
            .unwrap()
            .lines
            .iter()
            .any(|line| line == "abcdefgh")
    );
    terminal.process(b"\x1b[?1049l").unwrap();
    assert!(!terminal.alternate_screen());
}

#[test]
fn adapter_applies_corvid_query_prompt_and_clipboard_policy() {
    let mut terminal = TerminalScreen::with_ghostty(Viewport::new(20, 6).unwrap()).unwrap();
    terminal.configure_clipboard(true, false);
    let replies = terminal
        .process(b"\x1b[3;4H\x1b[c\x1b[5n\x1b[6n\x1b[18t\x1b[>q\x1b[14t")
        .unwrap();
    assert_eq!(replies, b"\x1b[?6c\x1b[0n\x1b[3;4R\x1b[8;6;20t");
    for chunk in b"\x1b]52;c;aGVsbG8=\x07\x1b]633;A\x07\x1b]2;shell\x07".chunks(3) {
        assert!(terminal.process(chunk).unwrap().is_empty());
    }
    assert_eq!(terminal.title(), "shell");
    assert_eq!(terminal.take_clipboard().as_deref(), Some("hello"));
    assert_eq!(terminal.shell_prompt(), Some(true));
    terminal
        .process(b"\x1b[?1002h\x1b[?1006h\x1b[?2004h")
        .unwrap();
    assert_eq!(terminal.wheel_encoding(), WheelEncoding::Sgr);
    assert!(terminal.bracketed_paste());
    assert!(!terminal.allows_shell_history());
}

#[test]
fn adapter_preserves_pending_wrap_across_the_footer_and_next_frame() {
    let inner = Viewport::new(4, 3).unwrap();
    let outer = Viewport::new(4, 4).unwrap();
    let mut terminal = TerminalScreen::with_ghostty(inner).unwrap();
    terminal.process(b"\x1b[42m1234").unwrap();
    let previous = terminal.snapshot().unwrap();
    let mut viewer = vt100::Parser::new(outer.rows, outer.cols, 0);
    viewer.process(
        terminal
            .render(None, outer, Some((4, "test")))
            .unwrap()
            .as_bytes(),
    );
    assert_eq!(viewer.screen().cursor_position(), (0, 4));
    terminal.process(b"5").unwrap();
    viewer.process(
        terminal
            .render(Some(&previous), outer, Some((4, "test")))
            .unwrap()
            .as_bytes(),
    );
    assert_eq!(viewer.screen().contents(), "1234\n5\n\ntest");
    assert_eq!(
        viewer.screen().cell(1, 0).unwrap().bgcolor(),
        vt100::Color::Idx(2)
    );
}
