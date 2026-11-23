use ghostty_vt::render::{CellIterator, RenderState, RowIterator};
use ghostty_vt::screen::{CellWide, Screen};
use ghostty_vt::style::{RgbColor, StyleColor};
use ghostty_vt::terminal::{ClipboardWritePolicy, Event, Mode, Options, Terminal};

use super::*;

const RED_IMAGE: &[u8] = b"\x1b_Ga=T,t=d,f=32,s=1,v=1,i=7,p=3,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\";
const GREEN_IMAGE: &[u8] = b"\x1b_Ga=t,t=d,f=32,s=1,v=1,i=7,q=2;AP8A/w==\x1b\\";

#[test]
fn kitty_storage_and_placement_follow_replace_screen_switch_and_delete() {
    use ghostty_vt::render::{ImageIterator, ImagePlacement, KittyPlacementKind};

    fn placements(terminal: &Terminal) -> Vec<ImagePlacement> {
        let mut state = RenderState::new().unwrap();
        let snapshot = state.update(terminal).unwrap();
        let mut iterator = ImageIterator::new().unwrap();
        let mut images = iterator.update(&snapshot).unwrap();
        let mut result = Vec::new();
        while let Some(image) = images.next() {
            result.push(image.unwrap());
        }
        result
    }

    let mut terminal = ghostty(Viewport::new(16, 6).unwrap());
    terminal.resize(16, 6, 8, 16).unwrap();
    terminal.set_kitty_image_storage_limit(4096).unwrap();
    terminal.set_kitty_image_from_file_allowed(false).unwrap();
    terminal.set_kitty_image_temp_file_dir(None).unwrap();
    terminal
        .set_kitty_image_from_shared_mem_allowed(false)
        .unwrap();
    terminal.vt_write(b"\x1b[2;4H");
    terminal.vt_write(RED_IMAGE);
    let before = placements(&terminal);
    assert_eq!(before.len(), 1);
    let placement = before[0];
    assert_eq!((placement.image_id, placement.placement_id), (7, 3));
    assert_eq!((placement.viewport_row, placement.viewport_col), (1, 3));
    assert_eq!((placement.grid_cols, placement.grid_rows), (1, 1));
    assert_eq!(placement.kind, KittyPlacementKind::Fixed);
    let graphics = terminal.kitty_graphics().unwrap();
    let red = graphics.image(7).unwrap().data().unwrap().to_vec();
    assert_eq!(red, [255, 0, 0, 255]);

    terminal.vt_write(GREEN_IMAGE);
    let graphics = terminal.kitty_graphics().unwrap();
    let green = graphics.image(7).unwrap();
    assert_eq!(green.data().unwrap(), [0, 255, 0, 255]);
    assert_ne!(green.generation().unwrap(), placement.image_generation);
    // Retransmitting an image drops its previous placements.
    assert!(placements(&terminal).is_empty());
    terminal.vt_write(b"\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\");
    assert_eq!(placements(&terminal).len(), 1);
    terminal.vt_write(b"\x1b[?1049h");
    assert!(placements(&terminal).is_empty());
    terminal.vt_write(b"\x1b[?1049l");
    assert_eq!(placements(&terminal).len(), 1);
    terminal.vt_write(b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
    assert!(placements(&terminal).is_empty());
    assert!(terminal.kitty_graphics().unwrap().image(7).is_none());
}

#[test]
fn text_profile_never_relays_kitty_apc() {
    let mut terminal = TerminalScreen::new(Viewport::new(20, 6).unwrap()).unwrap();
    assert!(terminal.process(RED_IMAGE).unwrap().is_empty());
    terminal.process(b"visible").unwrap();
    let frame = terminal.redraw(Viewport::new(20, 6).unwrap()).unwrap();
    assert!(frame.contains("visible"));
    assert!(!frame.contains("\x1b_G"));
}

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
    let mut current = TerminalScreen::new(viewport).unwrap();
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
    let mut current = TerminalScreen::new(viewport).unwrap();
    terminal
        .resize(resized.cols.get(), resized.rows.get(), 0, 0)
        .unwrap();
    current.resize(resized).unwrap();
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
    let mut terminal = TerminalScreen::new(viewport).unwrap();
    let sequence = "\x1b[1;38;2;10;20;30m한e\u{301}\x1b[0m\x1b[?1h\x1b[?25l";
    for byte in sequence.as_bytes() {
        terminal.process(std::slice::from_ref(byte)).unwrap();
    }
    let first = terminal.snapshot().unwrap();
    let mut viewer = vt100::Parser::new(viewport.rows, viewport.cols, 0);
    viewer.process(terminal.redraw(viewport).unwrap().as_bytes());
    let screen = viewer.screen();
    assert!(
        ScreenSnapshot::from_ansi(
            viewport,
            &terminal.redraw(viewport).unwrap().into_bytes(),
            None
        )
        .matches(&first)
    );
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
    let mut terminal = TerminalScreen::new(viewport).unwrap();
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
    terminal.resize(Viewport::new(8, 4).unwrap()).unwrap();
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
fn dirty_projection_clears_erased_cells_and_restores_the_primary_grid() {
    let viewport = Viewport::new(12, 4).unwrap();
    let mut terminal = TerminalScreen::new(viewport).unwrap();
    terminal
        .process(b"\x1b[?1h\x1b[?25lprimary\r\n\x1b[42mabcdef\x1b[0m")
        .unwrap();
    let initial = terminal.snapshot().unwrap();
    terminal
        .process(b"\x1b[2;3H\x1b[K\x1b[?1l\x1b[?25h")
        .unwrap();
    let erased = terminal.snapshot().unwrap();
    assert_eq!(erased.cell(0, 0).unwrap().contents(), "p");
    assert_eq!(erased.cell(1, 1).unwrap().contents(), "b");
    assert!(erased.cell(1, 2).unwrap().contents().is_empty());
    assert!(!erased.hide_cursor() && !erased.application_cursor());
    assert!(initial.hide_cursor() && initial.application_cursor());

    terminal.process(b"\x1b[?1049h\x1b[Halternate").unwrap();
    assert_eq!(
        terminal.snapshot().unwrap().cell(0, 0).unwrap().contents(),
        "a"
    );
    terminal.process(b"\x1b[?1049l").unwrap();
    let restored = terminal.snapshot().unwrap();
    assert_eq!(restored.cell(0, 0).unwrap().contents(), "p");
    assert!(restored.cell(1, 2).unwrap().contents().is_empty());
}

#[test]
fn adapter_applies_corvid_query_prompt_and_clipboard_policy() {
    let mut terminal = TerminalScreen::new(Viewport::new(20, 6).unwrap()).unwrap();
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
    let mut terminal = TerminalScreen::new(inner).unwrap();
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
