use std::{
    cell::{Cell, RefCell},
    ops::Range,
    sync::Arc,
};

use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    IntoElement, KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollWheelEvent, Task, UTF16Selection, Window, canvas, div,
    prelude::*, px, rgb,
};
use tokio::sync::oneshot;
use vt100::Screen;

use crate::appearance::{Appearance, ui_px};
use crate::terminal_fonts::TerminalFonts;
use crate::terminal_paint::{self as paint, Frame, Metrics};
use crate::terminal_session::{Grid, MAX_INPUT, Session};

mod history;
mod keyboard;
mod links;
use history::History;

type CellPosition = (u16, u16);
type CellRange = Range<CellPosition>;

pub struct TerminalView {
    focus: FocusHandle,
    session: Session,
    pane: String,
    marked: String,
    surface: Cell<Option<(Bounds<Pixels>, Metrics)>>,
    displayed: RefCell<Option<DisplayedScreen>>,
    selection: Option<Selection>,
    history: Option<History>,
    history_request: Option<Task<()>>,
    pending_wheel_back: usize,
    _updates: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryOpen {
    Wheel,
    Browse,
    Search,
}

#[derive(Clone)]
struct DisplayedScreen {
    screen: Arc<Screen>,
    history: bool,
}

struct Selection {
    displayed: DisplayedScreen,
    anchor: CellPosition,
    head: CellPosition,
    dragging: bool,
    link: Option<String>,
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
            .is_some_and(vt100::Cell::is_wide_continuation)
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
            .is_some_and(vt100::Cell::is_wide);
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

    fn finish(&mut self, release: Option<CellPosition>) -> Option<String> {
        self.dragging = false;
        self.link
            .take()
            .filter(|_| release == Some(self.anchor) && self.head == self.anchor)
    }
}

fn link_modifier(modifiers: Modifiers) -> bool {
    modifiers == Modifiers::control()
        || (cfg!(target_os = "macos") && modifiers == Modifiers::command())
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
            surface: Cell::new(None),
            displayed: RefCell::new(None),
            selection: None,
            history: None,
            history_request: None,
            pending_wheel_back: 0,
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
        self.cancel_link();
        self.cancel_pending_wheel();
        self.focus(window);
        let Some((bounds, metrics)) = self.surface.get() else {
            return;
        };
        if !bounds.contains(&event.position) {
            return;
        }
        self.marked.clear();
        let anchor = metrics.position(bounds, event.position);
        let displayed = self.displayed.borrow().clone();
        self.selection = displayed.map(|displayed| {
            let link = if link_modifier(event.modifiers) {
                hit_cell(bounds, metrics, event.position).and_then(|cell| {
                    links::at(
                        &displayed.screen,
                        metrics.grid(bounds),
                        displayed.history,
                        cell,
                    )
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

    fn drag_selection(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self
            .selection
            .as_mut()
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        if let Some((bounds, metrics)) = self.surface.get() {
            selection.drag_to(
                metrics.position(bounds, event.position),
                bounds.contains(&event.position),
            );
            cx.notify();
        }
    }

    fn release_selection(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let release = self
            .surface
            .get()
            .filter(|_| link_modifier(event.modifiers))
            .and_then(|(bounds, metrics)| hit_cell(bounds, metrics, event.position));
        self.end_selection(release, cx);
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
        if let Some(url) = link {
            cx.open_url(&url);
        }
    }

    fn cancel_link(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.link = None;
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

    fn open_history(&mut self, mode: HistoryOpen, back: isize, cx: &mut Context<Self>) {
        let shell_only = mode == HistoryOpen::Wheel;
        let height = self.visible_rows();
        if shell_only {
            self.pending_wheel_back = back as usize;
        } else {
            self.pending_wheel_back = 0;
            self.selection = None;
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
                if shell_only {
                    let back = std::mem::take(&mut view.pending_wheel_back);
                    if let Some(history) = History::from_wheel(result, back, height) {
                        view.selection = None;
                        view.marked.clear();
                        view.history = Some(history);
                    }
                } else if let Some(history) = &mut view.history {
                    history.loaded(result, height);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn cancel_pending_wheel(&mut self) {
        if self.pending_wheel_back > 0 {
            self.pending_wheel_back = 0;
            self.history_request = None;
        }
    }

    fn move_in_history(&mut self, rows: isize, height: usize) {
        if let Some(history) = &mut self.history {
            history.scroll(rows, height);
            if rows > 0 && history.at_bottom(height) {
                self.history = None;
                self.history_request = None;
                self.marked.clear();
            }
        }
        self.selection = None;
    }

    fn visible_rows(&self) -> usize {
        self.surface.get().map_or(1, |(bounds, metrics)| {
            usize::from(metrics.grid(bounds).rows.get())
        })
    }

    fn scroll_history(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((bounds, metrics)) = self.surface.get() else {
            return;
        };
        let delta = event.delta.pixel_delta(metrics.row).y;
        if delta == px(0.) {
            return;
        }
        self.cancel_link();
        let count = (delta / metrics.row).abs().ceil() as isize;
        let rows = if delta > px(0.) { -count } else { count };
        let height = self.visible_rows();
        if self.history.is_some() {
            self.move_in_history(rows, height);
            cx.notify();
        } else if bounds.contains(&event.position) {
            let cell = metrics.position(bounds, event.position);
            let modifiers =
                u8::from(event.modifiers.alt) * 8 + u8::from(event.modifiers.control) * 16;
            if !event.modifiers.shift && self.session.send_wheel(&self.pane, cell, rows, modifiers)
            {
                self.cancel_pending_wheel();
                self.selection = None;
                cx.notify();
            } else if self.pending_wheel_back > 0 {
                let back = self
                    .pending_wheel_back
                    .saturating_add_signed(-rows)
                    .min(isize::MAX as usize);
                if back == 0 {
                    self.cancel_pending_wheel();
                } else {
                    self.pending_wheel_back = back;
                }
            } else if rows < 0 {
                self.open_history(HistoryOpen::Wheel, count, cx);
            }
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
        let command = cfg!(target_os = "macos") && key.modifiers.platform;
        match (
            key.key.as_str(),
            key.modifiers.control || command,
            key.modifiers.shift || command,
        ) {
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
        self.cancel_pending_wheel();
        if !self.marked.is_empty() {
            return;
        }
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        let edit_shortcut = modifiers == gpui::Modifiers::control_shift()
            || (cfg!(target_os = "macos") && modifiers == gpui::Modifiers::command());
        if self.history.is_some() {
            if !self.history_key(key, cx) {
                // Let the platform input handler deliver printable and composed text.
                return;
            }
        } else if edit_shortcut && key.key == "f" {
            self.open_history(HistoryOpen::Search, 0, cx);
        } else if modifiers == gpui::Modifiers::shift() && key.key == "pageup" {
            self.open_history(HistoryOpen::Browse, self.visible_rows() as isize, cx);
        } else if edit_shortcut && key.key == "v" {
            self.selection = None;
            self.paste(cx);
            cx.notify();
        } else if edit_shortcut && key.key == "c" {
            let contents = self
                .selection
                .as_ref()
                .map(Selection::text)
                .or_else(|| self.session.screen(&self.pane, Screen::contents));
            if let Some(contents) = contents {
                cx.write_to_clipboard(ClipboardItem::new_string(contents));
            }
        } else if key.key == "escape" && !modifiers.modified() && self.selection.take().is_some() {
            cx.notify();
        } else {
            let (cursor, keypad) = self
                .session
                .screen(&self.pane, |s| {
                    (s.application_cursor(), s.application_keypad())
                })
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
        let view = cx.entity();
        let focus = self.focus.clone();
        let surface = canvas(
            move |bounds, window, cx| {
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
                (view, metrics, frame, terminal.marked.clone(), family)
            },
            move |bounds, (view, metrics, frame, marked, family), window, cx| {
                let active = focus.is_focused(window) && window.is_window_active();
                window.handle_input(&focus, ElementInputHandler::new(bounds, view), cx);
                frame.paint(
                    bounds,
                    metrics,
                    active.then_some(marked.as_str()),
                    &family,
                    window,
                    cx,
                );
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
            .on_mouse_up(MouseButton::Left, cx.listener(Self::release_selection))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_selection(None, cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .p(ui_px(10.))
            .overflow_hidden()
            .bg(rgb(paint::BACKGROUND))
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
            .child(div().flex_1().min_h_0().child(surface))
    }
}

pub fn preview(session: Session, pane: String) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let family = cx.global::<TerminalFonts>().current().clone();
            let font_size = cx.global::<Appearance>().terminal_size();
            let metrics = Metrics::measure(window, &family, font_size);
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
        self.cancel_pending_wheel();
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
        self.cancel_pending_wheel();
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
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (bounds, metrics) = self.surface.get()?;
        let (row, col) = if self.history.is_some() {
            (0, 0)
        } else {
            self.session.screen(&self.pane, Screen::cursor_position)?
        };
        Some(metrics.cell(bounds.origin, row, col, 1))
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
        let mut parser = vt100::Parser::new(grid.rows, grid.cols, 0);
        parser.process(b"https://example.test/");
        let make_selection = || Selection {
            displayed: DisplayedScreen {
                screen: Arc::new(parser.screen().clone()),
                history: false,
            },
            anchor: (0, 0),
            head: (0, 0),
            dragging: true,
            link: Some("https://example.test/".into()),
        };

        let mut click = make_selection();
        assert_eq!(
            click.finish(Some((0, 0))).as_deref(),
            Some("https://example.test/")
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

    #[cfg(windows)]
    #[test]
    #[ignore = "opens an isolated native window to exercise the Windows message loop"]
    fn native_terminal_updates_yield_to_window_frames() {
        use std::rc::Rc;
        use std::time::Duration;

        use gpui::{Application, Entity, WindowOptions};

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
        Application::new().run(move |cx| {
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
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
        assert_eq!(updates.get(), UPDATES);
        assert_eq!(at_next_frame.get(), Some(1));
    }
}
