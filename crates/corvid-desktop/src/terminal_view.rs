use std::{cell::Cell, ops::Range};

use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, ScrollWheelEvent, Task, UTF16Selection, Window, canvas, div, prelude::*, px, rgb,
};
use vt100::Screen;

use crate::terminal_paint::{self as paint, Frame, Metrics};
use crate::terminal_session::{MAX_INPUT, Session};

mod history;
use history::History;

pub struct TerminalView {
    focus: FocusHandle,
    session: Session,
    pane: String,
    marked: String,
    surface: Cell<Option<(Bounds<Pixels>, Metrics)>>,
    selection: Option<Selection>,
    history: Option<History>,
    history_request: Option<Task<()>>,
    _updates: Task<()>,
}

struct Selection {
    screen: Screen,
    anchor: (u16, u16),
    head: (u16, u16),
    dragging: bool,
}

impl Selection {
    fn cell(&self, position: (u16, u16)) -> (u16, u16) {
        let (rows, cols) = self.screen.size();
        let row = position.0.min(rows.get() - 1);
        let mut col = position.1.min(cols.get() - 1);
        if self
            .screen
            .cell(row, col)
            .is_some_and(vt100::Cell::is_wide_continuation)
        {
            col = col.saturating_sub(1);
        }
        (row, col)
    }

    fn range(&self) -> Range<(u16, u16)> {
        let anchor = self.cell(self.anchor);
        let head = self.cell(self.head);
        let (row, col) = anchor.max(head);
        let wide = self.screen.cell(row, col).is_some_and(vt100::Cell::is_wide);
        anchor.min(head)..(row, col + 1 + u16::from(wide))
    }

    fn text(&self) -> String {
        let range = self.range();
        self.screen
            .contents_between(range.start.0, range.start.1, range.end.0, range.end.1)
    }
}

impl TerminalView {
    pub fn new(session: Session, pane: String, cx: &mut Context<Self>) -> Self {
        let mut changes = session.screen_changes();
        let updates = cx.spawn(async move |view, cx| {
            while changes.changed().await.is_ok() {
                if view
                    .update(cx, |view, cx| {
                        if view.history.is_none() && view.selection.is_none() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            focus: cx.focus_handle(),
            session,
            pane,
            marked: String::new(),
            surface: Cell::new(None),
            selection: None,
            history: None,
            history_request: None,
            _updates: updates,
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    fn start_selection(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus(window);
        let Some((bounds, metrics)) = self.surface.get() else {
            return;
        };
        if !bounds.contains(&event.position) {
            return;
        }
        self.marked.clear();
        let anchor = metrics.position(bounds, event.position);
        let screen = if let Some(history) = &self.history {
            history
                .screen(metrics.grid(bounds))
                .map(|(screen, _)| screen)
        } else {
            self.session.screen(&self.pane, Clone::clone)
        };
        self.selection = screen.map(|screen| Selection {
            screen,
            anchor,
            head: anchor,
            dragging: true,
        });
        cx.notify();
    }

    fn drag_selection(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self
            .selection
            .as_mut()
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        if let Some((bounds, metrics)) = self.surface.get() {
            selection.head = metrics.position(bounds, event.position);
            cx.notify();
        }
    }

    fn end_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(selection) = &mut self.selection {
            if selection.anchor == selection.head {
                self.selection = None;
            } else {
                selection.dragging = false;
            }
            cx.notify();
        }
    }

    fn paste(&self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.len() > MAX_INPUT {
            return;
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text: String = text
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        self.session.send(&self.pane, text.into_bytes(), true);
    }

    fn open_history(&mut self, search: bool, back: isize, cx: &mut Context<Self>) {
        self.selection = None;
        self.marked.clear();
        let mut history = History::new(search);
        if let Some(receive) = self.session.history(&self.pane) {
            self.history_request = Some(cx.spawn(async move |view, cx| {
                let result = receive
                    .await
                    .unwrap_or_else(|_| Err("The terminal connection ended.".into()));
                let _ = view.update(cx, |view, cx| {
                    let height = view.visible_rows();
                    if let Some(history) = &mut view.history {
                        history.loaded(result);
                        if history.search.is_none() {
                            history.scroll(-back, height);
                        }
                        cx.notify();
                    }
                });
            }));
        } else {
            history.loaded(Err("Terminal history is unavailable.".into()));
        }
        self.history = Some(history);
        cx.notify();
    }

    fn visible_rows(&self) -> usize {
        self.surface.get().map_or(1, |(bounds, metrics)| {
            usize::from(metrics.grid(bounds).rows.get())
        })
    }

    fn scroll_history(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = f32::from(event.delta.pixel_delta(paint::LINE_HEIGHT).y);
        let rows = (delta / f32::from(paint::LINE_HEIGHT)).abs().ceil() as isize;
        let height = self.visible_rows();
        if delta > 0. && self.history.is_none() {
            self.open_history(false, rows, cx);
        } else if let Some(history) = &mut self.history {
            history.scroll(if delta > 0. { -rows } else { rows }, height);
            self.selection = None;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn history_key(&mut self, key: &Keystroke, cx: &mut Context<Self>) -> bool {
        let height = self.visible_rows();
        if key.key == "escape" {
            if self.selection.take().is_none() {
                self.history_request = None;
                self.history = None;
                self.marked.clear();
            }
            cx.notify();
            return true;
        }
        let Some(history) = &mut self.history else {
            return false;
        };
        match (key.key.as_str(), key.modifiers.control, key.modifiers.shift) {
            ("c", true, true) => {
                let contents = self.selection.as_ref().map(Selection::text).or_else(|| {
                    let (bounds, metrics) = self.surface.get()?;
                    history
                        .screen(metrics.grid(bounds))
                        .map(|(screen, _)| screen.contents())
                });
                if let Some(text) = contents {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                return true;
            }
            ("f", true, true) => {
                history.search.get_or_insert_with(Default::default);
            }
            ("v", true, _) if history.search.is_some() => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    history.edit_query(Some(&text));
                }
            }
            ("backspace", false, _) => history.edit_query(None),
            ("enter", false, shift) => history.find(shift),
            ("up", false, _) => history.scroll(-1, height),
            ("down", false, _) => history.scroll(1, height),
            ("pageup", _, _) => history.scroll(-(height as isize), height),
            ("pagedown", _, _) => history.scroll(height as isize, height),
            ("home", _, _) => history.scroll(isize::MIN, height),
            ("end", _, _) => history.scroll(isize::MAX, height),
            _ => return false,
        }
        self.selection = None;
        cx.notify();
        true
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        if self.history.is_some() {
            if !self.history_key(key, cx) {
                // Let the platform input handler deliver printable and composed text.
                return;
            }
        } else if modifiers.control && modifiers.shift && !modifiers.alt && key.key == "f" {
            self.open_history(true, 0, cx);
        } else if modifiers.shift && !modifiers.control && !modifiers.alt && key.key == "pageup" {
            self.open_history(false, self.visible_rows() as isize, cx);
        } else if modifiers.control && !modifiers.alt && key.key == "v" {
            self.selection = None;
            self.paste(cx);
            cx.notify();
        } else if modifiers.control && modifiers.shift && !modifiers.alt && key.key == "c" {
            let contents = self
                .selection
                .as_ref()
                .map(Selection::text)
                .or_else(|| self.session.screen(&self.pane, Screen::contents));
            if let Some(contents) = contents {
                cx.write_to_clipboard(ClipboardItem::new_string(contents));
            }
        } else if key.key == "escape" && self.selection.take().is_some() {
            cx.notify();
        } else {
            let application = self
                .session
                .screen(&self.pane, Screen::application_cursor)
                .unwrap_or(false);
            let Some(bytes) = encode_key(key, application) else {
                return;
            };
            self.selection = None;
            cx.notify();
            self.session.send(&self.pane, bytes, false);
        }
        cx.stop_propagation();
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let focus = self.focus.clone();
        let surface = canvas(
            move |bounds, window, cx| {
                let metrics = Metrics::measure(window);
                let grid = metrics.grid(bounds);
                let terminal = view.read(cx);
                terminal.surface.set(Some((bounds, metrics)));
                if terminal.history.is_none() {
                    terminal.session.resize(&terminal.pane, grid);
                }
                let frame = if let Some(selection) = &terminal.selection {
                    Frame::selected(&selection.screen, grid, Some(&selection.range()))
                } else if let Some(history) = &terminal.history {
                    history
                        .screen(grid)
                        .map(|(screen, hit)| Frame::selected(&screen, grid, hit.as_ref()))
                        .unwrap_or_default()
                } else {
                    terminal
                        .session
                        .screen(&terminal.pane, |screen| Frame::new(screen, grid))
                        .unwrap_or_default()
                };
                (view, metrics, frame, terminal.marked.clone())
            },
            move |bounds, (view, metrics, frame, marked), window, cx| {
                let active = focus.is_focused(window) && window.is_window_active();
                window.handle_input(&focus, ElementInputHandler::new(bounds, view), cx);
                frame.paint(bounds, metrics, active, marked, window, cx);
            },
        )
        .size_full();

        div()
            .id("daemon-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::start_selection))
            .on_mouse_move(cx.listener(Self::drag_selection))
            .on_scroll_wheel(cx.listener(Self::scroll_history))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_selection(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_selection(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .p(px(10.))
            .overflow_hidden()
            .bg(rgb(paint::BACKGROUND))
            .when_some(self.history.as_ref(), |this, history| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(rgb(0xa6c8db))
                        .pb(px(6.))
                        .overflow_hidden()
                        .child(history.title(&self.marked)),
                )
            })
            .child(div().flex_1().min_h_0().child(surface))
    }
}

pub fn preview(session: Session, pane: String) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            let metrics = Metrics::measure(window);
            let grid = metrics.grid(bounds);
            let frame = session
                .preview(&pane, |screen| Frame::new(screen, grid))
                .unwrap_or_default();
            (metrics, frame)
        },
        move |bounds, (metrics, frame), window, cx| {
            frame.paint(bounds, metrics, false, String::new(), window, cx);
        },
    )
    .size_full()
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then(|| 0..self.marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        self.selection = None;
        if let Some(history) = &mut self.history {
            history.edit_query(Some(text));
            cx.notify();
            return;
        }
        if !text.is_empty() {
            self.session
                .send(&self.pane, text.as_bytes().to_vec(), text.len() > 8192);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = text
            .chars()
            .filter(|ch| !ch.is_control())
            .take(256)
            .collect();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.history.is_some() {
            return Some(Metrics::measure(window).cell(bounds.origin, 0, 0, 1));
        }
        let (row, col) = self.session.screen(&self.pane, Screen::cursor_position)?;
        Some(Metrics::measure(window).cell(bounds.origin, row, col, 1))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn encode_key(key: &Keystroke, application: bool) -> Option<Vec<u8>> {
    let name = key.key.as_str();
    let modifiers = key.modifiers;
    let arrow = match name {
        "up" => Some(b'A'),
        "down" => Some(b'B'),
        "right" => Some(b'C'),
        "left" => Some(b'D'),
        _ => None,
    };
    if let Some(arrow) = arrow {
        let modifier = 1
            + u8::from(modifiers.shift)
            + 2 * u8::from(modifiers.alt)
            + 4 * u8::from(modifiers.control);
        return if modifier > 1 {
            Some(format!("\x1b[1;{modifier}{}", char::from(arrow)).into_bytes())
        } else {
            Some(vec![0x1b, if application { b'O' } else { b'[' }, arrow])
        };
    }
    let bytes: &[u8] = match name {
        "enter" => b"\r",
        "escape" => b"\x1b",
        "backspace" => b"\x7f",
        "tab" if modifiers.shift => b"\x1b[Z",
        "tab" => b"\t",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "home" => {
            if application {
                b"\x1bOH"
            } else {
                b"\x1b[H"
            }
        }
        "end" => {
            if application {
                b"\x1bOF"
            } else {
                b"\x1b[F"
            }
        }
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        name if modifiers.control && !modifiers.alt => {
            let [byte] = name.as_bytes() else { return None };
            let byte = byte.to_ascii_uppercase();
            return matches!(byte, b'@'..=b'_' | b' ').then(|| vec![byte & 0x1f]);
        }
        _ => return None,
    };
    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_session::Grid;

    #[test]
    fn copies_reverse_selection_across_wide_cells_and_soft_wrap() {
        let grid = Grid::new(3, 5);
        let mut parser = vt100::Parser::new(grid.rows, grid.cols, 0);
        parser.process("A한BCde\r\nend".as_bytes());
        let selection = Selection {
            screen: parser.screen().clone(),
            anchor: (2, 0),
            head: (0, 2),
            dragging: false,
        };
        assert_eq!(selection.text(), "한BCde\ne");
    }
}
