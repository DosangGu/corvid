use std::{cell::Cell, ops::Range};

use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, Task, UTF16Selection, Window, canvas, div, prelude::*, px, rgb,
};
use vt100::Screen;

use crate::terminal_paint::{self as paint, Frame, Metrics};
use crate::terminal_session::{MAX_INPUT, Session};

pub struct TerminalView {
    focus: FocusHandle,
    session: Session,
    pane: String,
    marked: String,
    surface: Cell<Option<(Bounds<Pixels>, Metrics)>>,
    selection: Option<Selection>,
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
                if view.update(cx, |_, cx| cx.notify()).is_err() {
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
        self.selection = self
            .session
            .screen(&self.pane, Clone::clone)
            .map(|screen| Selection {
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

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        if modifiers.control && !modifiers.alt && key.key == "v" {
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
                terminal.session.resize(&terminal.pane, grid);
                let frame = if let Some(selection) = &terminal.selection {
                    Frame::selected(&selection.screen, grid, Some(&selection.range()))
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
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_selection(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_selection(cx)),
            )
            .size_full()
            .min_h_0()
            .p(px(10.))
            .overflow_hidden()
            .bg(rgb(paint::BACKGROUND))
            .child(surface)
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
