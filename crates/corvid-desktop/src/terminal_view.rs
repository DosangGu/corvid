use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::{Arc, Weak},
    time::Instant,
};

use corvid_async::oneshot;
use corvid_protocol::ansi_screen::{Cell as ScreenCell, Screen};
use gpui::{
    App, Bounds, ClipboardItem, Context, Div, FocusHandle, HitboxBehavior, InputHandler,
    IntoElement, KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollEvent, Task, UTF16Selection, WeakEntity, Window, canvas,
    div, prelude::*, rgb,
};

use crate::appearance::{Appearance, ui_px};
use crate::shortcuts;
use crate::terminal_fonts::TerminalFonts;
use crate::terminal_paint::{self as paint, Frame, Metrics};
use crate::terminal_session::{
    Grid, MouseAction, MouseButtonCode, MouseEventReport, Session, WheelTarget,
};

mod history;
mod keyboard;
mod links;
mod wheel;
use history::History;
use wheel::WheelGesture;

type CellPosition = (u16, u16);
type CellRange = Range<CellPosition>;
pub(crate) type SurfaceGeometry = (Bounds<Pixels>, Metrics);

pub struct TerminalView {
    focus: FocusHandle,
    session: Session,
    pane: String,
    marked: String,
    leader_active: bool,
    pending_pane_click: Option<(u16, u16)>,
    surface: Cell<Option<SurfaceGeometry>>,
    displayed: RefCell<Option<DisplayedScreen>>,
    link_hover: RefCell<Option<LinkHover>>,
    selection: Option<Selection>,
    mouse_capture: Option<MouseCapture>,
    history: Option<History>,
    history_request: Option<Task<()>>,
    copy_request: Option<Task<()>>,
    notice: Option<String>,
    notice_task: Option<Task<()>>,
    pending_wheel_back: usize,
    pending_wheel_copy: bool,
    history_epoch: Option<u64>,
    gesture: WheelGesture,
    _updates: Task<()>,
    _clipboard: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryOpen {
    Wheel,
    Browse,
    Search,
    PreviousCommand,
}

#[derive(Clone)]
struct DisplayedScreen {
    screen: Arc<Screen>,
    history: bool,
}

struct LinkHover {
    screen: Weak<Screen>,
    grid: Grid,
    remote_host: Option<String>,
    cell: CellPosition,
    link: Option<links::Link>,
}

impl LinkHover {
    fn update(
        cache: &mut Option<Self>,
        displayed: &DisplayedScreen,
        grid: Grid,
        remote_host: Option<&str>,
        cell: CellPosition,
    ) -> bool {
        if cache.as_ref().is_some_and(|hover| {
            std::ptr::eq(hover.screen.as_ptr(), Arc::as_ptr(&displayed.screen))
                && hover.grid == grid
                && hover.remote_host.as_deref() == remote_host
                && (hover.cell == cell
                    || hover.link.as_ref().is_some_and(|link| {
                        cell.1 < displayed.screen.size().1.get()
                            && link.spans.iter().any(|range| range.contains(&cell))
                    }))
        }) {
            return false;
        }
        let link = links::at(
            &displayed.screen,
            grid,
            displayed.history,
            remote_host,
            cell,
        );
        let changed = cache.as_ref().and_then(|hover| hover.link.as_ref()) != link.as_ref();
        *cache = Some(Self {
            screen: Arc::downgrade(&displayed.screen),
            grid,
            remote_host: remote_host.map(str::to_owned),
            cell,
            link,
        });
        changed
    }
}

struct Selection {
    displayed: DisplayedScreen,
    anchor: CellPosition,
    head: CellPosition,
    dragging: bool,
    link: Option<links::LinkTarget>,
}

impl Selection {
    fn cell(&self, position: CellPosition) -> CellPosition {
        let (rows, cols) = self.displayed.screen.size();
        let row = position.0.min(rows.get() - 1);
        let mut col = position.1.min(cols.get() - 1);
        if self
            .displayed
            .screen
            .cell(row, col)
            .is_some_and(ScreenCell::is_wide_continuation)
        {
            col = col.saturating_sub(1);
        }
        (row, col)
    }

    fn range(&self) -> CellRange {
        let anchor = self.cell(self.anchor);
        let head = self.cell(self.head);
        let (row, col) = anchor.max(head);
        let wide = self
            .displayed
            .screen
            .cell(row, col)
            .is_some_and(ScreenCell::is_wide);
        anchor.min(head)..(row, col + 1 + u16::from(wide))
    }

    fn text(&self) -> String {
        let range = self.range();
        self.displayed.screen.contents_between(
            range.start.0,
            range.start.1,
            range.end.0,
            range.end.1,
        )
    }

    fn drag_to(&mut self, head: CellPosition, inside: bool) {
        if head != self.anchor || !inside {
            self.link = None;
        }
        self.head = head;
    }

    fn finish(&mut self, release: Option<CellPosition>) -> Option<links::LinkTarget> {
        self.dragging = false;
        self.link
            .take()
            .filter(|_| release == Some(self.anchor) && self.head == self.anchor)
    }
}

#[derive(Clone, Copy)]
struct MouseCapture {
    route: (u64, WheelTarget),
    button: MouseButton,
    code: MouseButtonCode,
    cell: CellPosition,
}

fn link_modifier(modifiers: Modifiers) -> bool {
    modifiers == Modifiers::control()
        || (cfg!(target_os = "macos") && modifiers == Modifiers::command())
}

fn mouse_modifiers(modifiers: Modifiers) -> u8 {
    u8::from(modifiers.shift) * 4 + u8::from(modifiers.alt) * 8 + u8::from(modifiers.control) * 16
}

fn mouse_button_code(button: MouseButton) -> Option<MouseButtonCode> {
    match button {
        MouseButton::Left => Some(MouseButtonCode::Left),
        MouseButton::Middle => Some(MouseButtonCode::Middle),
        MouseButton::Right => Some(MouseButtonCode::Right),
        _ => None,
    }
}

fn mouse_forward_target(
    history_open: bool,
    button: MouseButton,
    modifiers: Modifiers,
    target: Option<(u64, WheelTarget)>,
) -> Option<((u64, WheelTarget), MouseButtonCode)> {
    if history_open || modifiers.shift || (button == MouseButton::Left && link_modifier(modifiers))
    {
        return None;
    }
    let code = mouse_button_code(button)?;
    let route @ (_, WheelTarget::Mouse(_)) = target? else {
        return None;
    };
    Some((route, code))
}

fn hit_cell(
    bounds: Bounds<Pixels>,
    metrics: Metrics,
    position: Point<Pixels>,
) -> Option<CellPosition> {
    if !bounds.contains(&position) {
        return None;
    }
    let (row, col) = metrics.position(bounds, position);
    metrics
        .cell(bounds.origin, row, col, 1)
        .contains(&position)
        .then_some((row, col))
}

impl TerminalView {
    pub fn new(session: Session, pane: String, window: &Window, cx: &mut Context<Self>) -> Self {
        let mut clipboard = session.clipboard_changes();
        let clipboard_task = cx.spawn_in(window, async move |view, cx| {
            while clipboard.changed().await.is_ok() {
                let _ = cx.update(|window, cx| {
                    if let Some(view) = view.upgrade() {
                        let view = view.read(cx);
                        let text = view.session.take_clipboard(&view.pane);
                        if window.is_window_active()
                            && view.focus.is_focused(window)
                            && cx.global::<TerminalFonts>().allows_clipboard_write()
                            && let Some(text) = text
                        {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                });
            }
        });
        let mut changes = session.screen_changes();
        let updates = cx.spawn_in(window, async move |view, cx| {
            while changes.changed().await.is_ok() {
                // A busy terminal can keep the watch ready and monopolize the UI thread.
                // Consume the latest update at the next frame before waiting for more output.
                let view = view.clone();
                let (complete, frame) = oneshot::channel();
                cx.on_next_frame(move |_, cx| {
                    changes.borrow_and_update();
                    if view
                        .update(cx, |view, cx| {
                            if view.mouse_capture.is_some_and(|capture| {
                                view.session
                                    .wheel_target(&view.pane)
                                    .is_none_or(|route| route.0 != capture.route.0)
                            }) {
                                view.mouse_capture = None;
                            }
                            if let Some(epoch) = view.history_epoch
                                && view
                                    .session
                                    .wheel_target(&view.pane)
                                    .is_none_or(|route| route.0 != epoch)
                            {
                                view.clear_history();
                                view.selection = None;
                            }
                            if view.history.is_none() && view.selection.is_none() {
                                cx.notify();
                            }
                        })
                        .is_ok()
                    {
                        let _ = complete.send(changes);
                    }
                });
                let Ok(receiver) = frame.await else {
                    break;
                };
                changes = receiver;
            }
        });
        Self {
            focus: cx.focus_handle(),
            session,
            pane,
            marked: String::new(),
            leader_active: false,
            pending_pane_click: None,
            surface: Cell::new(None),
            displayed: RefCell::new(None),
            link_hover: RefCell::new(None),
            selection: None,
            mouse_capture: None,
            history: None,
            history_request: None,
            copy_request: None,
            notice: None,
            notice_task: None,
            pending_wheel_back: 0,
            pending_wheel_copy: false,
            history_epoch: None,
            gesture: WheelGesture::default(),
            _updates: updates,
            _clipboard: clipboard_task,
        }
    }

    pub(crate) fn pending_pane_click(&mut self, cell: Option<(u16, u16)>) {
        self.pending_pane_click = cell;
    }

    pub(crate) fn set_leader_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.leader_active != active {
            self.leader_active = active;
            cx.notify();
        }
    }

    fn accepts_text_input(&self) -> bool {
        accepts_text_input_for(self.leader_active, self.history.as_ref())
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn mouse_down(
        &mut self,
        button: MouseButton,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_link();
        self.cancel_pending_wheel();
        window.focus(&self.focus, cx);
        let Some((bounds, metrics)) = self.surface.get() else {
            return;
        };
        if !bounds.contains(&event.position) {
            return;
        }
        self.marked.clear();
        let anchor = metrics.position(bounds, event.position);
        if let Some((route, code)) = mouse_forward_target(
            self.history.is_some(),
            button,
            event.modifiers,
            self.session.wheel_target(&self.pane),
        ) {
            self.session.send_mouse(
                &self.pane,
                route,
                MouseEventReport {
                    button: code,
                    action: MouseAction::Press,
                    cell: anchor,
                    modifiers: mouse_modifiers(event.modifiers),
                },
            );
            self.selection = None;
            self.mouse_capture = Some(MouseCapture {
                route,
                button,
                code,
                cell: anchor,
            });
            cx.notify();
            return;
        }
        self.mouse_capture = None;
        if button != MouseButton::Left {
            return;
        }
        let displayed = self.displayed.borrow().clone();
        let remote_host = self.session.remote_host_label();
        self.selection = displayed.map(|displayed| {
            let link = if link_modifier(event.modifiers) {
                hit_cell(bounds, metrics, event.position).and_then(|cell| {
                    links::at(
                        &displayed.screen,
                        metrics.grid(bounds),
                        displayed.history,
                        remote_host.as_deref(),
                        cell,
                    )
                    .map(|link| link.target)
                })
            } else {
                None
            };
            Selection {
                displayed,
                anchor,
                head: anchor,
                dragging: true,
                link,
            }
        });
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((bounds, metrics)) = self.surface.get() else {
            return;
        };
        if let Some(capture) = self.mouse_capture {
            if event.pressed_button == Some(capture.button) {
                let cell = metrics.position(bounds, event.position);
                if cell != capture.cell {
                    // TODO: Distinguish mouse tracking modes (?9h/?1000h/?1002h/?1003h) in RenderHeader
                    // Currently drag motion is sent whenever WheelTarget::Mouse is active; propagating the exact mode lets us avoid sending drag motion to click-only apps and support any-motion hover.
                    if let Some(active) = self.mouse_capture.as_mut() {
                        active.cell = cell;
                    }
                    self.session.send_mouse(
                        &self.pane,
                        capture.route,
                        MouseEventReport {
                            button: capture.code,
                            action: MouseAction::Drag,
                            cell,
                            modifiers: mouse_modifiers(event.modifiers),
                        },
                    );
                }
                self.clear_link_hover(cx);
                return;
            }
            self.mouse_capture = None;
        }
        if let Some(selection) = self
            .selection
            .as_mut()
            .filter(|selection| selection.dragging)
        {
            selection.drag_to(
                metrics.position(bounds, event.position),
                bounds.contains(&event.position),
            );
            cx.notify();
        } else if event.pressed_button.is_none()
            && let Some(cell) = hit_cell(bounds, metrics, event.position)
            && let Some(displayed) = self.displayed.borrow().as_ref()
        {
            let remote_host = self.session.remote_host_label();
            if LinkHover::update(
                &mut self.link_hover.borrow_mut(),
                displayed,
                metrics.grid(bounds),
                remote_host.as_deref(),
                cell,
            ) {
                cx.notify();
            }
        } else {
            self.clear_link_hover(cx);
        }
    }

    fn clear_link_hover(&self, cx: &mut Context<Self>) {
        if self
            .link_hover
            .borrow_mut()
            .take()
            .is_some_and(|hover| hover.link.is_some())
        {
            cx.notify();
        }
    }

    fn hovered_link(
        &self,
        bounds: Bounds<Pixels>,
        metrics: Metrics,
        position: Point<Pixels>,
    ) -> Option<Vec<CellRange>> {
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.dragging)
        {
            return None;
        }
        let displayed = self.displayed.borrow();
        let displayed = displayed.as_ref()?;
        let mut hover = self.link_hover.borrow_mut();
        // Output can replace a hovered link. Refresh that location without
        // scanning output when the pointer has not found a link.
        hover.as_ref()?.link.as_ref()?;
        let grid = metrics.grid(bounds);
        let remote_host = self.session.remote_host_label();
        LinkHover::update(
            &mut hover,
            displayed,
            grid,
            remote_host.as_deref(),
            hit_cell(bounds, metrics, position)?,
        );
        Some(hover.as_ref()?.link.as_ref()?.spans.clone())
    }

    fn mouse_up(
        &mut self,
        button: MouseButton,
        event: &MouseUpEvent,
        inside: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .mouse_capture
            .is_some_and(|capture| capture.button == button)
        {
            let capture = self.mouse_capture.take().unwrap();
            let cell = self
                .surface
                .get()
                .map_or(capture.cell, |(bounds, metrics)| {
                    metrics.position(bounds, event.position)
                });
            self.session.send_mouse(
                &self.pane,
                capture.route,
                MouseEventReport {
                    button: capture.code,
                    action: MouseAction::Release,
                    cell,
                    modifiers: mouse_modifiers(event.modifiers),
                },
            );
            return;
        }
        if button == MouseButton::Left {
            let release = if inside {
                self.surface
                    .get()
                    .filter(|_| link_modifier(event.modifiers))
                    .and_then(|(bounds, metrics)| hit_cell(bounds, metrics, event.position))
            } else {
                None
            };
            self.end_selection(release, cx);
        }
    }

    fn end_selection(&mut self, release: Option<CellPosition>, cx: &mut Context<Self>) {
        let Some(selection) = self
            .selection
            .as_mut()
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        let link = selection.finish(release);
        if selection.anchor == selection.head {
            self.selection = None;
        }
        cx.notify();
        if let Some(target) = link {
            match target {
                links::LinkTarget::External(url)
                | links::LinkTarget::LocalFile { uri: url, .. } => {
                    cx.open_url(&url);
                }
                links::LinkTarget::RemoteFile { path, .. } => {
                    cx.write_to_clipboard(ClipboardItem::new_string(path));
                    self.set_notice("Copied remote file path (not opened locally).", cx);
                }
            }
        }
    }

    fn cancel_link(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.link = None;
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.len() > corvid_core::MAX_PASTE_BYTES {
            self.set_notice(corvid_core::PASTE_LIMIT_NOTICE, cx);
            return;
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text: String = text
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        if text.len() > corvid_core::MAX_PASTE_BYTES {
            self.set_notice(corvid_core::PASTE_LIMIT_NOTICE, cx);
            return;
        }
        self.session.send(&self.pane, text.into_bytes(), true);
    }

    pub(crate) fn jump_previous_command(&mut self, cx: &mut Context<Self>) {
        let height = self.visible_rows();
        if let Some(history) = &mut self.history {
            history.jump_command(true, height);
            self.selection = None;
            cx.notify();
            return;
        }
        self.open_history(HistoryOpen::PreviousCommand, 0, cx);
    }

    pub(crate) fn copy_command_output(&mut self, cx: &mut Context<Self>) {
        if let Some(history) = &mut self.history {
            match history.copy_command_output() {
                Ok(output) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(output.text));
                    history.notice = Some(
                        if output.truncated {
                            "Copied output (older rows omitted)"
                        } else {
                            "Copied command output"
                        }
                        .into(),
                    );
                }
                Err(corvid_protocol::history::CommandOutputError::NoCommands) => {
                    history.notice = Some("No command markers in this terminal.".into());
                }
                Err(corvid_protocol::history::CommandOutputError::NoOutput) => {
                    history.notice = Some("Last command produced no output.".into());
                }
            }
            cx.notify();
            return;
        }

        let route = self.session.wheel_target(&self.pane);
        let Some(receive) = self.session.history(&self.pane, false) else {
            self.set_notice("Terminal history is unavailable.", cx);
            return;
        };
        self.copy_request = Some(cx.spawn(async move |view, cx| {
            let result = receive
                .await
                .unwrap_or_else(|_| Err("The terminal connection ended.".into()));
            let _ = view.update(cx, |view, cx| {
                let current = view.session.wheel_target(&view.pane);
                if current.map(|r| r.0) != route.map(|r| r.0) {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        if snapshot.alternate_screen == Some(true) {
                            view.set_notice(
                                "Command history is unavailable on the alternate screen.",
                                cx,
                            );
                            return;
                        }
                        match corvid_protocol::history::last_command_output(&snapshot) {
                            Ok(output) => {
                                cx.write_to_clipboard(ClipboardItem::new_string(output.text));
                                view.set_notice(
                                    if output.truncated {
                                        "Copied last command output (older rows omitted)"
                                    } else {
                                        "Copied last command output"
                                    },
                                    cx,
                                );
                            }
                            Err(corvid_protocol::history::CommandOutputError::NoCommands) => {
                                view.set_notice("No command markers in this terminal.", cx);
                            }
                            Err(corvid_protocol::history::CommandOutputError::NoOutput) => {
                                view.set_notice("Last command produced no output.", cx);
                            }
                        }
                    }
                    Err(message) => view.set_notice(message, cx),
                }
            });
        }));
    }

    fn set_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some(text.into());
        self.notice_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(3))
                .await;
            let _ = view.update(cx, |view, cx| {
                view.notice = None;
                view.notice_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn open_history_command(&mut self, search: bool, cx: &mut Context<Self>) {
        let mode = if search {
            HistoryOpen::Search
        } else {
            HistoryOpen::Browse
        };
        self.open_history(
            mode,
            if search {
                0
            } else {
                self.visible_rows() as isize
            },
            cx,
        );
    }

    fn open_history(&mut self, mode: HistoryOpen, back: isize, cx: &mut Context<Self>) {
        let route = self.session.wheel_target(&self.pane);
        let wheel = mode == HistoryOpen::Wheel;
        self.notice = None;
        self.notice_task = None;
        self.pending_wheel_copy = false;
        let shell_only =
            wheel && route.is_some_and(|(_, owner)| owner == WheelTarget::LegacyScrollback);
        self.history_epoch = route.map(|route| route.0);
        let height = self.visible_rows();
        if wheel {
            self.pending_wheel_back = back as usize;
        } else {
            self.pending_wheel_back = 0;
            self.selection = None;
            self.mouse_capture = None;
            self.marked.clear();
            let mut history = History::new(mode == HistoryOpen::Search);
            history.scroll(-back, height);
            self.history = Some(history);
        }
        let Some(receive) = self.session.history(&self.pane, shell_only) else {
            self.pending_wheel_back = 0;
            if let Some(history) = &mut self.history {
                history.loaded(Err("Terminal history is unavailable.".into()), height);
            }
            cx.notify();
            return;
        };
        self.history_request = Some(cx.spawn(async move |view, cx| {
            let result = receive
                .await
                .unwrap_or_else(|_| Err("The terminal connection ended.".into()));
            let _ = view.update(cx, |view, cx| {
                let height = view.visible_rows();
                let current = view.session.wheel_target(&view.pane);
                if current.map(|r| r.0) != route.map(|r| r.0) {
                    view.clear_history();
                } else if wheel {
                    let back = std::mem::take(&mut view.pending_wheel_back);
                    let copy = std::mem::take(&mut view.pending_wheel_copy);
                    if route == current
                        && let Some(history) = History::from_wheel(result, back, height, shell_only)
                    {
                        view.selection = None;
                        view.mouse_capture = None;
                        view.marked.clear();
                        view.history = Some(history);
                    } else {
                        view.history_epoch = None;
                    }
                    if copy && route == current {
                        view.copy_visible(cx);
                    }
                } else if let Some(history) = &mut view.history {
                    if mode == HistoryOpen::PreviousCommand {
                        match &result {
                            Ok(snapshot) if snapshot.alternate_screen == Some(true) => {
                                view.clear_history();
                                view.set_notice(
                                    "Command history is unavailable on the alternate screen.",
                                    cx,
                                );
                                return;
                            }
                            Ok(_) => {
                                history.loaded(result, height);
                                if !history.jump_command(true, height) {
                                    view.clear_history();
                                    view.set_notice("No command markers in this terminal.", cx);
                                }
                            }
                            Err(_) => {
                                history.loaded(result, height);
                            }
                        }
                    } else {
                        history.loaded(result, height);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn clear_history(&mut self) {
        self.history = None;
        self.history_request = None;
        self.pending_wheel_back = 0;
        self.pending_wheel_copy = false;
        self.history_epoch = None;
        self.marked.clear();
    }

    fn wheel_browse(&self) -> bool {
        self.history
            .as_ref()
            .is_some_and(|h| h.wheel_browse && h.search.is_none())
    }

    fn cancel_pending_wheel(&mut self) {
        if self.pending_wheel_back > 0 {
            self.history_epoch = None;
            self.history_request = None;
            self.pending_wheel_back = 0;
            self.pending_wheel_copy = false;
        }
    }

    fn move_in_history(&mut self, rows: isize, height: usize) {
        if let Some(history) = &mut self.history {
            history.scroll(rows, height);
            if rows > 0 && history.at_bottom(height) {
                self.clear_history();
            }
        }
        self.selection = None;
    }

    fn visible_rows(&self) -> usize {
        self.surface.get().map_or(1, |(bounds, metrics)| {
            usize::from(metrics.grid(bounds).rows.get())
        })
    }

    fn scroll_history(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((bounds, metrics)) = self.surface.get() else {
            return;
        };
        if !bounds.contains(&event.position) {
            return;
        }
        let Some((epoch, live)) = self.session.wheel_target(&self.pane) else {
            return;
        };
        let owner = if self.history.is_some() || live == WheelTarget::LegacyScrollback {
            WheelTarget::Scrollback
        } else {
            live
        };
        let delta = -(event.delta.pixel_delta(metrics.row).y / metrics.row);
        let Some(rows) =
            self.gesture
                .rows(Instant::now(), event.touch_phase, (epoch, owner), delta)
        else {
            cx.stop_propagation();
            return;
        };
        self.cancel_link();
        if self.history.is_some() {
            self.move_in_history(rows, self.visible_rows());
            if self.history.is_none() {
                self.gesture.finish_at_bottom();
            }
        } else if matches!(live, WheelTarget::Mouse(_) | WheelTarget::Cursor { .. }) {
            self.cancel_pending_wheel();
            let modifiers = mouse_modifiers(event.modifiers);
            self.session.send_wheel(
                &self.pane,
                (epoch, live),
                metrics.position(bounds, event.position),
                rows,
                modifiers,
            );
            self.selection = None;
        } else if self.pending_wheel_back > 0 {
            let back = self
                .pending_wheel_back
                .saturating_add_signed(-rows)
                .min(isize::MAX as usize);
            if back == 0 {
                self.cancel_pending_wheel();
                self.gesture.finish_at_bottom();
            } else {
                self.pending_wheel_back = back;
            }
        } else if rows < 0 {
            self.open_history(HistoryOpen::Wheel, -rows, cx);
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn copy_visible(&self, cx: &mut Context<Self>) {
        let contents = self.selection.as_ref().map(Selection::text).or_else(|| {
            if let Some(history) = &self.history {
                let (bounds, metrics) = self.surface.get()?;
                history
                    .screen(metrics.grid(bounds))
                    .map(|(screen, _)| screen.contents())
            } else {
                self.session.screen(&self.pane, Screen::contents)
            }
        });
        if let Some(text) = contents {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn history_key(&mut self, key: &Keystroke, cx: &mut Context<Self>) -> bool {
        let height = self.visible_rows();
        if key.key == "escape" {
            if self.selection.take().is_none() {
                self.clear_history();
            }
            cx.notify();
            return true;
        }
        let Some(history) = &mut self.history else {
            return false;
        };
        let command = cfg!(target_os = "macos") && key.modifiers.platform;
        match (
            shortcuts::key(key),
            key.modifiers.control || command,
            key.modifiers.shift || command,
        ) {
            ("c", true, true) => {
                self.copy_visible(cx);
                return true;
            }
            ("v", true, _) if history.search.is_some() => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    history.edit_query(Some(&text));
                }
            }
            ("backspace", false, _) if history.search.is_some() => history.edit_query(None),
            ("enter", false, shift) if history.search.is_some() => history.find(shift),
            ("[", false, _) | ("p", false, _) | ("up", false, true) if history.search.is_none() => {
                history.jump_command(true, height);
            }
            ("]", false, _) | ("n", false, _) | ("down", false, true)
                if history.search.is_none() =>
            {
                history.jump_command(false, height);
            }
            ("c", false, false) if history.search.is_none() => {
                self.copy_command_output(cx);
                return true;
            }
            ("q", false, false) if history.search.is_none() => {
                self.clear_history();
                self.selection = None;
                cx.notify();
                return true;
            }
            ("k", false, false) if history.search.is_none() => history.scroll(-1, height),
            ("j", false, false) if history.search.is_none() => self.move_in_history(1, height),
            ("up", false, _) => history.scroll(-1, height),
            ("down", false, _) => self.move_in_history(1, height),
            ("pageup", _, _) => history.scroll(-(height as isize), height),
            ("pagedown", _, _) => self.move_in_history(height as isize, height),
            ("home", _, _) => history.scroll(isize::MIN, height),
            ("end", _, _) => self.move_in_history(isize::MAX, height),
            _ => return false,
        }
        self.selection = None;
        cx.notify();
        true
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_link();
        if self.notice.take().is_some() {
            self.notice_task = None;
            cx.notify();
        }
        let key = &event.keystroke;
        let name = shortcuts::key(key);
        let modifiers = key.modifiers;
        let edit_shortcut = modifiers == gpui::Modifiers::control_shift()
            || (cfg!(target_os = "macos") && modifiers == gpui::Modifiers::command());
        if self.pending_wheel_back > 0 && self.marked.is_empty() {
            // The history reply can arrive after the next two keyboard events.
            if key.key == "home" && !modifiers.modified() {
                self.pending_wheel_back = isize::MAX as usize;
                cx.stop_propagation();
                return;
            }
            if name == "c" && edit_shortcut {
                self.pending_wheel_copy = true;
                cx.stop_propagation();
                return;
            }
        }
        self.cancel_pending_wheel();
        if !self.marked.is_empty() {
            return;
        }
        if self.history.is_some() {
            if self.history_key(key, cx) {
                cx.stop_propagation();
                return;
            }
            if !self.wheel_browse() {
                return;
            }
            self.clear_history();
            self.selection = None;
            cx.notify();
        }
        if edit_shortcut && name == "v" {
            self.selection = None;
            self.paste(cx);
            cx.notify();
        } else if edit_shortcut && name == "c" {
            self.copy_visible(cx);
        } else if key.key == "escape" && !modifiers.modified() && self.selection.take().is_some() {
            cx.notify();
        } else {
            let (cursor, keypad) = self
                .session
                .screen(&self.pane, |s| (s.application_cursor, s.application_keypad))
                .unwrap_or_default();
            let Some(bytes) = keyboard::encode(key, cursor, keypad) else {
                return;
            };
            self.selection = None;
            cx.notify();
            self.session.send(&self.pane, bytes, false);
        }
        cx.stop_propagation();
    }

    fn display(&self, grid: Grid) -> Option<(DisplayedScreen, Option<CellRange>)> {
        if let Some(selection) = &self.selection {
            return Some((selection.displayed.clone(), Some(selection.range())));
        }
        if let Some(history) = &self.history {
            let (screen, hit) = history.screen(grid)?;
            let displayed = DisplayedScreen {
                screen: Arc::new(screen),
                history: true,
            };
            return Some((displayed, hit));
        }
        let displayed = DisplayedScreen {
            screen: self.session.screen_snapshot(&self.pane)?,
            history: false,
        };
        Some((displayed, None))
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_pane_click.is_some()
            && self.session.screen_snapshot(&self.pane).is_some()
            && let Some(cell) = self.pending_pane_click.take()
        {
            self.session.send_pane_click(&self.pane, cell);
        }
        let view = cx.entity();
        let focus = self.focus.clone();
        let surface = canvas(
            move |bounds, window, cx| {
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                let family = cx.global::<TerminalFonts>().current().clone();
                let font_size = cx.global::<Appearance>().terminal_size();
                let metrics = Metrics::measure(window, &family, font_size);
                let grid = metrics.grid(bounds);
                let terminal = view.read(cx);
                terminal.surface.set(Some((bounds, metrics)));
                if terminal.history.is_none() {
                    terminal.session.resize(&terminal.pane, grid);
                }
                let frame = if let Some((displayed, highlight)) = terminal.display(grid) {
                    let frame = Frame::selected(&displayed.screen, grid, highlight.as_ref());
                    // Hit testing needs the painted screen even when newer output is available.
                    terminal.displayed.replace(Some(displayed));
                    frame
                } else {
                    terminal.displayed.replace(None);
                    Frame::default()
                };
                (
                    view,
                    metrics,
                    frame,
                    terminal.marked.clone(),
                    family,
                    hitbox,
                )
            },
            move |bounds, (view, metrics, frame, marked, family, hitbox), window, cx| {
                let link = if window.is_window_hovered() && hitbox.is_hovered(window) {
                    view.read(cx)
                        .hovered_link(bounds, metrics, window.mouse_position())
                } else {
                    None
                };
                let active = focus.is_focused(window) && window.is_window_active();
                window.handle_input(
                    &focus,
                    TerminalInput {
                        view: view.downgrade(),
                    },
                    cx,
                );
                frame.paint(
                    bounds,
                    metrics,
                    active.then_some(marked.as_str()),
                    &family,
                    window,
                    cx,
                );
                if let Some(spans) = link {
                    paint::link_underline(bounds, metrics, &spans, window);
                }
            },
        )
        .size_full();
        let link_preview = self
            .selection
            .as_ref()
            .is_none_or(|selection| !selection.dragging)
            .then(|| {
                self.link_hover
                    .borrow()
                    .as_ref()
                    .and_then(|hover| hover.link.as_ref())
                    .map(|link| {
                        let action = if link.target.is_remote_file() {
                            if cfg!(target_os = "macos") {
                                "Cmd+Click to copy path"
                            } else {
                                "Ctrl+Click to copy path"
                            }
                        } else if cfg!(target_os = "macos") {
                            "Cmd+Click"
                        } else {
                            "Ctrl+Click"
                        };
                        format!("{} · {action}", link.target.preview_label())
                    })
            })
            .flatten();

        terminal_surface()
            .id("daemon-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event, window, cx| {
                    view.mouse_down(MouseButton::Left, event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|view, event, window, cx| {
                    view.mouse_down(MouseButton::Middle, event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|view, event, window, cx| {
                    view.mouse_down(MouseButton::Right, event, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                if !*hovered {
                    view.clear_link_hover(cx);
                }
            }))
            .on_scroll(cx.listener(Self::scroll_history))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event, _, cx| view.mouse_up(MouseButton::Left, event, true, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|view, event, _, cx| {
                    view.mouse_up(MouseButton::Middle, event, true, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|view, event, _, cx| {
                    view.mouse_up(MouseButton::Right, event, true, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event, _, cx| {
                    view.mouse_up(MouseButton::Left, event, false, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|view, event, _, cx| {
                    view.mouse_up(MouseButton::Middle, event, false, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|view, event, _, cx| {
                    view.mouse_up(MouseButton::Right, event, false, cx);
                }),
            )
            .when_some(self.history.as_ref(), |this, history| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(rgb(0xa6c8db))
                        .pb(ui_px(6.))
                        .overflow_hidden()
                        .child(history.title(&self.marked)),
                )
            })
            .when(self.history.is_none(), |this| {
                this.when_some(self.notice.as_ref(), |this, notice| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(rgb(0xa6c8db))
                            .pb(ui_px(6.))
                            .overflow_hidden()
                            .child(notice.clone()),
                    )
                })
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(surface)
                    .when_some(link_preview, |this, preview| {
                        this.child(
                            div()
                                .absolute()
                                .left(ui_px(4.))
                                .bottom(ui_px(4.))
                                .max_w_full()
                                .px(ui_px(8.))
                                .py(ui_px(3.))
                                .rounded(ui_px(4.))
                                .bg(rgb(0x16202a))
                                .border_1()
                                .border_color(rgb(0x2d4356))
                                .text_xs()
                                .text_color(rgb(0xa6c8db))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(preview),
                        )
                    }),
            )
    }
}

fn terminal_surface() -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .min_h_0()
        .p(ui_px(10.))
        .overflow_hidden()
        .bg(rgb(paint::BACKGROUND))
}

pub(crate) fn preview(
    session: Session,
    pane: String,
    surface: Option<Rc<Cell<Option<SurfaceGeometry>>>>,
) -> impl IntoElement {
    let canvas = canvas(
        move |bounds, window, cx| {
            let family = cx.global::<TerminalFonts>().current().clone();
            let font_size = cx.global::<Appearance>().terminal_size();
            let metrics = Metrics::measure(window, &family, font_size);
            if let Some(surface) = &surface {
                surface.set(Some((bounds, metrics)));
            }
            let grid = metrics.grid(bounds);
            let frame = session
                .preview(&pane, |screen| Frame::new(screen, grid))
                .unwrap_or_default();
            (metrics, frame, family)
        },
        move |bounds, (metrics, frame, family), window, cx| {
            frame.paint(bounds, metrics, None, &family, window, cx);
        },
    )
    .size_full();
    terminal_surface().child(div().flex_1().min_h_0().child(canvas))
}

// X11 can retain the platform input handler while closing its window asynchronously.
// The terminal view belongs to the workspace and can be released before that cleanup finishes.
struct TerminalInput {
    view: WeakEntity<TerminalView>,
}

impl InputHandler for TerminalInput {
    fn accepts_text_input(&mut self, _: &mut Window, cx: &mut App) -> bool {
        self.view
            .upgrade()
            .is_some_and(|view| view.read(cx).accepts_text_input())
    }

    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        let view = self.view.upgrade()?;
        let end = view.read(cx).marked.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        let view = self.view.upgrade()?;
        let marked = &view.read(cx).marked;
        (!marked.is_empty()).then(|| 0..marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut App) {
        let _ = self.view.update(cx, |view, cx| {
            view.marked.clear();
            cx.notify();
        });
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut App,
    ) {
        let _ = self.view.update(cx, |view, cx| {
            // Allow committing an in-progress marked composition (e.g. when Windows
            // completes the IME on leader/browse entry), but ignore stray text
            // while text input is disabled.
            if !view.accepts_text_input() && view.marked.is_empty() {
                return;
            }
            view.cancel_pending_wheel();
            if view.wheel_browse() {
                view.clear_history();
            }
            view.marked.clear();
            view.selection = None;
            if let Some(history) = &mut view.history {
                history.edit_query(Some(text));
            } else if !text.is_empty() {
                view.session
                    .send(&view.pane, text.as_bytes().to_vec(), text.len() > 8192);
            }
            cx.notify();
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut App,
    ) {
        let _ = self.view.update(cx, |view, cx| {
            if !view.accepts_text_input() {
                return;
            }
            view.cancel_pending_wheel();
            if view.wheel_browse() {
                view.clear_history();
            }
            view.marked = text
                .chars()
                .filter(|ch| !ch.is_control())
                .take(256)
                .collect();
            cx.notify();
        });
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let view = self.view.upgrade()?;
        let view = view.read(cx);
        let (bounds, metrics) = view.surface.get()?;
        let (row, col) = if view.history.is_some() {
            (0, 0)
        } else {
            view.session.screen(&view.pane, Screen::cursor_position)?
        };
        Some(metrics.cell(bounds.origin, row, col, 1))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }
}

fn accepts_text_input_for(leader_active: bool, history: Option<&History>) -> bool {
    !leader_active && history.is_none_or(|history| history.search.is_some() || history.wheel_browse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_session::Grid;

    #[test]
    fn copies_reverse_selection_across_wide_cells_and_soft_wrap() {
        let grid = Grid::new(3, 5);
        let mut parser = corvid_protocol::ansi_screen::Decoder::new(grid.rows, grid.cols);
        parser.process("A한BCde\r\nend".as_bytes());
        let selection = Selection {
            displayed: DisplayedScreen {
                screen: Arc::new(parser.screen().clone()),
                history: false,
            },
            anchor: (2, 0),
            head: (0, 2),
            dragging: false,
            link: None,
        };
        assert_eq!(selection.text(), "한BCde\ne");
    }

    #[test]
    fn releases_a_link_only_once_and_never_after_dragging() {
        let grid = Grid::new(2, 32);
        let mut parser = corvid_protocol::ansi_screen::Decoder::new(grid.rows, grid.cols);
        parser.process(b"https://example.test/");
        let make_selection = || Selection {
            displayed: DisplayedScreen {
                screen: Arc::new(parser.screen().clone()),
                history: false,
            },
            anchor: (0, 0),
            head: (0, 0),
            dragging: true,
            link: Some(links::LinkTarget::External("https://example.test/".into())),
        };

        let mut click = make_selection();
        assert_eq!(
            click.finish(Some((0, 0))),
            Some(links::LinkTarget::External("https://example.test/".into()))
        );
        assert_eq!(click.finish(Some((0, 0))), None);
        assert_eq!(make_selection().finish(None), None);
        assert_eq!(make_selection().finish(Some((0, 1))), None);

        let mut drag = make_selection();
        drag.drag_to((0, 5), true);
        assert_eq!(drag.text(), "https:");
        drag.drag_to((0, 0), true);
        assert_eq!(drag.finish(Some((0, 0))), None);
    }

    #[test]
    fn keeps_the_underline_within_the_hovered_link_and_clears_it_on_new_output() {
        let grid = Grid::new(4, 16);
        let mut parser = corvid_protocol::ansi_screen::Decoder::new(grid.rows, grid.cols);
        parser.process(b"https://example.test/path");
        let displayed = DisplayedScreen {
            screen: Arc::new(parser.screen().clone()),
            history: false,
        };
        let mut hover = None;
        assert!(LinkHover::update(
            &mut hover,
            &displayed,
            grid,
            None,
            (0, 2)
        ));
        assert!(!LinkHover::update(
            &mut hover,
            &displayed,
            grid,
            None,
            (1, 3)
        ));
        assert_eq!(hover.as_ref().unwrap().cell, (0, 2));

        let wider_grid = Grid::new(4, 20);
        assert!(LinkHover::update(
            &mut hover,
            &displayed,
            wider_grid,
            None,
            (0, 17)
        ));
        assert!(hover.as_ref().unwrap().link.is_none());
        assert!(LinkHover::update(
            &mut hover,
            &displayed,
            grid,
            None,
            (0, 2)
        ));

        parser.process(b"\x1b[2J\x1b[Hreplacement");
        let updated = DisplayedScreen {
            screen: Arc::new(parser.screen().clone()),
            history: false,
        };
        assert!(LinkHover::update(&mut hover, &updated, grid, None, (0, 2)));
        assert!(hover.as_ref().unwrap().link.is_none());
    }

    #[test]
    fn mouse_modifiers_encodes_shift_alt_and_control_bits() {
        assert_eq!(mouse_modifiers(Modifiers::none()), 0);
        assert_eq!(mouse_modifiers(Modifiers::shift()), 4);
        assert_eq!(mouse_modifiers(Modifiers::alt()), 8);
        assert_eq!(mouse_modifiers(Modifiers::control()), 16);
        assert_eq!(
            mouse_modifiers(Modifiers {
                control: true,
                alt: true,
                shift: true,
                ..Modifiers::none()
            }),
            28
        );
    }

    #[test]
    fn mouse_forward_target_respects_shift_links_and_history() {
        let mouse_route = Some((
            7,
            WheelTarget::Mouse(corvid_protocol::wire::WheelEncoding::Sgr),
        ));
        assert_eq!(
            mouse_forward_target(false, MouseButton::Left, Modifiers::none(), mouse_route),
            Some((
                (
                    7,
                    WheelTarget::Mouse(corvid_protocol::wire::WheelEncoding::Sgr)
                ),
                MouseButtonCode::Left,
            ))
        );
        assert_eq!(
            mouse_forward_target(false, MouseButton::Middle, Modifiers::alt(), mouse_route),
            Some((
                (
                    7,
                    WheelTarget::Mouse(corvid_protocol::wire::WheelEncoding::Sgr)
                ),
                MouseButtonCode::Middle,
            ))
        );
        assert_eq!(
            mouse_forward_target(false, MouseButton::Right, Modifiers::control(), mouse_route),
            Some((
                (
                    7,
                    WheelTarget::Mouse(corvid_protocol::wire::WheelEncoding::Sgr)
                ),
                MouseButtonCode::Right,
            ))
        );
        assert_eq!(
            mouse_forward_target(false, MouseButton::Left, Modifiers::shift(), mouse_route),
            None
        );
        assert_eq!(
            mouse_forward_target(false, MouseButton::Left, Modifiers::control(), mouse_route),
            None
        );
        if cfg!(target_os = "macos") {
            assert_eq!(
                mouse_forward_target(false, MouseButton::Left, Modifiers::command(), mouse_route),
                None
            );
        }
        assert_eq!(
            mouse_forward_target(true, MouseButton::Left, Modifiers::none(), mouse_route),
            None
        );
        assert_eq!(
            mouse_forward_target(
                false,
                MouseButton::Left,
                Modifiers::none(),
                Some((7, WheelTarget::Scrollback)),
            ),
            None
        );
    }

    #[test]
    fn disables_text_input_during_leader_and_explicit_history_browse() {
        let browse = History::new(false);
        let search = History::new(true);
        let mut wheel = History::new(false);
        wheel.wheel_browse = true;

        assert!(accepts_text_input_for(false, None));
        assert!(!accepts_text_input_for(true, None));
        assert!(!accepts_text_input_for(false, Some(&browse)));
        assert!(!accepts_text_input_for(true, Some(&browse)));
        assert!(accepts_text_input_for(false, Some(&search)));
        assert!(!accepts_text_input_for(true, Some(&search)));
        assert!(accepts_text_input_for(false, Some(&wheel)));
        assert!(!accepts_text_input_for(true, Some(&wheel)));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "opens an isolated native window to exercise the Windows message loop"]
    fn native_terminal_updates_yield_to_window_frames() {
        use std::rc::Rc;
        use std::time::Duration;

        use gpui::{Entity, WindowOptions};

        struct Probe {
            _terminal: Entity<TerminalView>,
        }

        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full()
            }
        }

        const UPDATES: usize = 8;
        let updates = Rc::new(Cell::new(0));
        let observed = updates.clone();
        let at_next_frame = Rc::new(Cell::new(None));
        let sampled = at_next_frame.clone();
        gpui_platform::application().run(move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let (session, changes) = Session::screen_change_fixture();
                let terminal = cx.new(|cx| TerminalView::new(session, "probe".into(), window, cx));
                let handle = window.window_handle();
                let initial = changes.clone();

                cx.observe(&terminal, move |_, cx| {
                    let count = observed.get() + 1;
                    observed.set(count);
                    if count == 1 {
                        let observed = observed.clone();
                        let sampled = sampled.clone();
                        cx.defer(move |cx| {
                            handle
                                .update(cx, |_, window, _| {
                                    window.on_next_frame(move |_, cx| {
                                        sampled.set(Some(observed.get()));
                                        if observed.get() == UPDATES {
                                            cx.quit();
                                        }
                                    });
                                })
                                .unwrap();
                        });
                    }
                    if count < UPDATES {
                        // Keep the foreground watch ready even while it handles this update.
                        changes.send_replace(());
                    } else if sampled.get().is_some() {
                        cx.quit();
                    }
                })
                .detach();

                initial.send_replace(());
                cx.new(|_| Probe {
                    _terminal: terminal,
                })
            })
            .unwrap();

            cx.spawn(async |cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
        assert_eq!(updates.get(), UPDATES);
        assert_eq!(at_next_frame.get(), Some(1));
    }
}
