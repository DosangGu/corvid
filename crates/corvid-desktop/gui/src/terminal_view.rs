use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::thread::JoinHandle;

use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, EventEmitter,
    FocusHandle, IntoElement, KeyDownEvent, Keystroke, MouseButton, Pixels, Point,
    ScrollWheelEvent, Task, UTF16Selection, Window, canvas, div, prelude::*, px, rgb,
};
use uuid::Uuid;

use crate::session_layout::MAX_LOCAL_PANES;
use crate::terminal_paint::{self as paint, Frame, Metrics};
use crate::terminal_session::{self, Grid, Job, MAX_INPUT, Session, State};

#[derive(Clone, Default)]
pub struct Lifetime(Rc<RefCell<Jobs>>);

#[derive(Default)]
struct Jobs {
    active: HashMap<Uuid, Job>,
    closing: Vec<JoinHandle<()>>,
}

impl Jobs {
    fn reap(&mut self) {
        self.closing.retain(|job| !job.is_finished());
    }
}

impl Lifetime {
    pub fn has_capacity(&self) -> bool {
        let mut jobs = self.0.borrow_mut();
        jobs.reap();
        jobs.active.len() + jobs.closing.len() < MAX_LOCAL_PANES
    }

    pub fn close(&self, pane: Uuid) {
        let mut jobs = self.0.borrow_mut();
        jobs.reap();
        if let Some(job) = jobs.active.remove(&pane) {
            jobs.closing.push(job.request_shutdown());
        }
    }

    pub fn shutdown(&self) {
        let Jobs {
            active,
            mut closing,
        } = self.0.take();
        closing.extend(active.into_values().map(Job::request_shutdown));
        for job in closing {
            let _ = job.join();
        }
    }
}

pub struct PaneFocused;

pub struct TerminalView {
    pane: Uuid,
    focus: FocusHandle,
    lifetime: Lifetime,
    session: Option<Session>,
    updates: Option<Task<()>>,
    error: Option<String>,
    marked: String,
}

impl EventEmitter<PaneFocused> for TerminalView {}

impl TerminalView {
    pub fn new(
        pane: Uuid,
        lifetime: Lifetime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        cx.on_focus(&focus, window, |_, _, cx| cx.emit(PaneFocused))
            .detach();
        window.focus(&focus);
        let mut view = Self {
            pane,
            focus,
            lifetime,
            session: None,
            updates: None,
            error: None,
            marked: String::new(),
        };
        view.start(cx);
        view
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self
            .lifetime
            .0
            .borrow()
            .active
            .get(&self.pane)
            .is_some_and(|job| !job.finished())
        {
            return;
        }
        let grid = self
            .session
            .as_ref()
            .map_or(Grid::new(26, 112), |session| *session.resize.borrow());
        self.lifetime.close(self.pane);
        self.error = None;
        if !self.lifetime.has_capacity() {
            self.error = Some("Terminal limit reached. Wait for closed panes to finish.".into());
            cx.notify();
            return;
        }
        let result =
            terminal_session::powershell().and_then(|command| Session::start(grid, command));
        match result {
            Ok((session, job)) => {
                let mut changes = session.changes();
                self.updates = Some(cx.spawn(async move |view, cx| {
                    while changes.changed().await.is_ok() {
                        if view.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                }));
                self.session = Some(session);
                self.lifetime.0.borrow_mut().active.insert(self.pane, job);
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn send(&self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if let Some(session) = &self.session
            && session.send(bytes)
        {
            cx.notify();
        }
    }

    fn paste(&self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        // Keep the paste bounded and prevent pasted escape sequences from ending a bracketed paste.
        if text.len() > MAX_INPUT.saturating_sub(12) {
            return;
        }
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let text: String = text
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\r' | '\t'))
            .collect();
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .session
            .as_ref()
            .is_some_and(|session| session.buffer().screen().bracketed_paste());
        let bytes = if bracketed {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        self.send(bytes, cx);
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        if modifiers.control && !modifiers.alt && key.key == "v" {
            self.paste(cx);
        } else if modifiers.control && modifiers.shift && !modifiers.alt && key.key == "c" {
            cx.write_to_clipboard(ClipboardItem::new_string(
                session.buffer().screen().contents(),
            ));
        } else if modifiers.shift && matches!(key.key.as_str(), "pageup" | "pagedown") {
            let rows = isize::try_from(session.resize.borrow().rows.get()).unwrap_or(25);
            session.scroll(if key.key == "pageup" { rows } else { -rows });
            cx.notify();
        } else {
            let application = session.buffer().screen().application_cursor();
            let Some(bytes) = encode_key(key, application) else {
                return;
            };
            self.send(bytes, cx);
        }
        cx.stop_propagation();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let pixels = event.delta.pixel_delta(paint::LINE_HEIGHT).y;
        let lines = (f32::from(pixels) / f32::from(paint::LINE_HEIGHT)).round() as isize;
        if lines != 0 {
            session.scroll(lines);
            cx.notify();
        }
    }

    fn status(&self) -> (String, bool) {
        if let Some(error) = &self.error {
            return (error.clone(), true);
        }
        let Some(session) = &self.session else {
            return ("Starting local PowerShell…".into(), false);
        };
        let buffer = session.buffer();
        match &buffer.state {
            State::Starting => ("Starting local PowerShell…".into(), false),
            State::Running if buffer.screen().scrollback() > 0 => {
                ("Scrollback · Type to return to the prompt".into(), false)
            }
            State::Running => ("Local PowerShell".into(), false),
            State::Exited(code) => (format!("PowerShell exited ({code})"), true),
            State::Failed(error) => (error.clone(), true),
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, can_restart) = self.status();
        let view = cx.entity();
        let focus = self.focus.clone();
        let resize = self.session.as_ref().map(|session| session.resize.clone());
        let surface = canvas(
            move |bounds, window, cx| {
                let metrics = Metrics::measure(window);
                let grid = metrics.grid(bounds);
                if let Some(resize) = &resize {
                    resize.send_if_modified(|previous| {
                        if *previous == grid {
                            return false;
                        }
                        *previous = grid;
                        true
                    });
                }
                let terminal = view.read(cx);
                let frame = terminal
                    .session
                    .as_ref()
                    .map(|session| Frame::new(session.buffer().screen(), grid))
                    .unwrap_or_default();
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
            .id("local-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, _| view.focus(window)),
            )
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(rgb(paint::BACKGROUND))
            .text_color(rgb(0xe2e8f0))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p(px(14.))
                    .overflow_hidden()
                    .child(surface),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(rgb(0x30343f))
                    .text_size(px(12.))
                    .text_color(rgb(0xa4adbc))
                    .child(div().min_w_0().truncate().child(status))
                    .when(can_restart, |bar| {
                        bar.child(
                            div()
                                .id("restart-powershell")
                                .cursor_pointer()
                                .px(px(10.))
                                .py(px(4.))
                                .rounded_md()
                                .bg(rgb(0x20332f))
                                .text_color(rgb(0x83dec0))
                                .child("New shell")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.focus(window);
                                    view.start(cx);
                                })),
                        )
                    }),
            )
    }
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
        if !text.is_empty() {
            self.send(text.as_bytes().to_vec(), cx);
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
        let buffer = self.session.as_ref()?.buffer();
        let (row, col) = buffer.screen().cursor_position();
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
