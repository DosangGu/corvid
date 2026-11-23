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

    let dimensions = current.snapshot().size();
    let mut state = RenderState::new().unwrap();
    let snapshot = state.update(&terminal).unwrap();
    assert_eq!(snapshot.rows().unwrap(), dimensions.0.get());
    assert_eq!(snapshot.cols().unwrap(), dimensions.1.get());
    assert_eq!(
        snapshot.cursor_visible().unwrap(),
        !current.snapshot().hide_cursor()
    );
    let cursor = snapshot.cursor_viewport().unwrap().unwrap();
    assert_eq!((cursor.y, cursor.x), current.snapshot().cursor_position());

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
