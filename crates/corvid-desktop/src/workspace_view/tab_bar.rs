use std::cell::Cell;
use std::rc::Rc;

use corvid_protocol::wire::reorder_tab::Placement;
use gpui::{
    AnyWindowHandle, Bounds, Context, Div, Pixels, ScrollHandle, Stateful, Window, div, point,
    prelude::*, px, rgb,
};

#[cfg(windows)]
use super::window_controls::window_drag_region;
use super::{MUTED, NAVIGATION_HIGHLIGHT, PANEL, TEXT, WorkspaceView, ui_px};
#[cfg(target_os = "macos")]
use crate::macos::window_drag_region;
use crate::terminal_session::TabReorderView;

const SCROLL_EPSILON: Pixels = px(0.01);
const TABS_IN_TITLEBAR: bool = cfg!(any(windows, target_os = "macos"));

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Arrows {
    #[default]
    Hidden,
    Visible {
        left: bool,
        right: bool,
    },
}

impl Arrows {
    fn for_scroll(max: Pixels, offset: Pixels, reserved_width: Pixels) -> Self {
        if max <= reserved_width + SCROLL_EPSILON {
            return Self::Hidden;
        }
        Self::Visible {
            left: offset < -SCROLL_EPSILON,
            right: offset > -max + SCROLL_EPSILON,
        }
    }

    fn buttons(self) -> Option<(bool, bool)> {
        match self {
            Self::Hidden => None,
            Self::Visible { left, right } => Some((left, right)),
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
}

pub(super) struct TabDrag {
    pub(super) window: AnyWindowHandle,
    pub(super) view: TabReorderView,
    pub(super) workspace: String,
    pub(super) tab: String,
}

struct TabDragPreview(String);

impl Render for TabDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().pt(ui_px(24.)).child(
            div()
                .rounded_md()
                .bg(rgb(PANEL))
                .px(ui_px(8.))
                .py(ui_px(3.))
                .text_size(ui_px(12.))
                .text_color(rgb(TEXT))
                .max_w(ui_px(240.))
                .truncate()
                .child(self.0.clone()),
        )
    }
}

impl TabDrag {
    pub(super) fn title_button(self, button: Stateful<Div>, title: String) -> Stateful<Div> {
        button.on_drag(self, move |_: &Self, _, _, cx| {
            cx.new(|_| TabDragPreview(title.clone()))
        })
    }
}

pub(super) fn drop_zones(workspace: &str, tab: &str, cx: &Context<WorkspaceView>) -> [Div; 2] {
    [Direction::Left, Direction::Right].map(|side| {
        let workspace = workspace.to_owned();
        let target = tab.to_owned();
        let placement = match side {
            Direction::Left => Placement::BeforeTabId(target.clone()),
            Direction::Right => Placement::AfterTabId(target.clone()),
        };
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .w_1_2()
            .border_color(gpui::transparent_black())
            .map(|zone| match side {
                Direction::Left => zone.left_0().border_l_2(),
                Direction::Right => zone.right_0().border_r_2(),
            })
            .can_drop(move |drag, window, _| {
                drag.downcast_ref::<TabDrag>().is_some_and(|drag| {
                    drag.window == window.window_handle()
                        && drag.workspace == workspace
                        && drag.tab != target
                })
            })
            .drag_over::<TabDrag>(|style, _, _, _| style.border_color(rgb(NAVIGATION_HIGHLIGHT)))
            .on_drop(cx.listener(move |view, drag: &TabDrag, _, _| {
                if let Some(session) = &view.session {
                    session.reorder_tab(
                        &drag.view,
                        drag.workspace.clone(),
                        drag.tab.clone(),
                        placement.clone(),
                    );
                }
            }))
    })
}

#[derive(Default)]
pub(super) struct TabBar {
    scroll: ScrollHandle,
    arrows: Rc<Cell<Arrows>>,
    selection: Option<String>,
    follow_selection: Rc<Cell<bool>>,
    drag_scroll_pending: Rc<Cell<bool>>,
}

impl TabBar {
    pub(super) fn select(&mut self, tab: Option<&str>, reveal: bool) {
        if self.selection.as_deref() != tab {
            self.selection = tab.map(str::to_owned);
            self.follow_selection.set(tab.is_some());
        }
        if reveal {
            self.follow_selection.set(true);
        }
    }

    pub(super) fn render<'a>(&self, tabs: impl Iterator<Item = (&'a str, Stateful<Div>)>) -> Div {
        let mut reveal_index = None;
        let selected = self
            .selection
            .as_deref()
            .filter(|_| self.follow_selection.get());
        let scroll = self.scroll.clone();
        let follow_selection = self.follow_selection.clone();
        let drag_scroll = scroll.clone();
        let drag_follow = follow_selection.clone();
        let drag_pending = self.drag_scroll_pending.clone();
        let strip = div()
            .id("daemon-tabs")
            .flex()
            .flex_1()
            .min_w_0()
            .when(!TABS_IN_TITLEBAR, |strip| strip.gap(ui_px(3.)))
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            .on_drag_move::<TabDrag>(move |event, window, cx| {
                if event.drag(cx).window == window.window_handle()
                    && drag_scroll.bounds().contains(&event.event.position)
                    && !drag_pending.replace(true)
                {
                    step_drag_scroll(window, &drag_scroll, &drag_follow, &drag_pending);
                }
            })
            .on_scroll(move |_, _, _| {
                if scroll.max_offset().x > SCROLL_EPSILON {
                    follow_selection.set(false);
                }
            })
            .children(tabs.enumerate().map(|(index, (id, tab))| {
                if Some(id) == selected {
                    reveal_index = Some(index);
                }
                tab.when(TABS_IN_TITLEBAR && index > 0, |tab| tab.ml(ui_px(3.)))
            }));

        #[cfg(any(windows, target_os = "macos"))]
        let strip = strip
            .h_full()
            .items_center()
            .child(window_drag_region(div().flex_1().min_w_0().h_full()));

        let arrows = self.arrows.clone();
        let shown = arrows.get();
        let scroll = self.scroll.clone();
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .when(TABS_IN_TITLEBAR, |row| row.h_full())
            .on_children_prepainted(move |children, window, _| {
                let reserved_width = match children.as_slice() {
                    [left, _, right] => left.size.width + right.size.width,
                    _ => px(0.),
                };
                let max = scroll.max_offset().x;
                let offset = scroll.offset().x;
                let next = Arrows::for_scroll(max, offset, reserved_width);
                let viewport = scroll.bounds();
                // The arrow layout must settle before revealing the selected tab.
                // Keep following it as the terminal publishes its title and the label grows.
                let offset_changed = if (shown == Arrows::Hidden) == (next == Arrows::Hidden)
                    && let Some(index) = reveal_index
                    && viewport.size.width > px(0.)
                    && let Some(tab) = scroll.bounds_for_item(index)
                {
                    set_scroll_x(&scroll, reveal_offset(viewport, tab, offset))
                } else {
                    false
                };
                arrows.set(Arrows::for_scroll(max, scroll.offset().x, reserved_width));
                if shown != arrows.get() || offset_changed {
                    window.on_next_frame(|window, _| window.refresh());
                }
            })
            .when_some(shown.buttons(), |row, (left, _)| {
                row.child(self.arrow(Direction::Left, left))
            })
            .child(strip)
            .when_some(shown.buttons(), |row, (_, right)| {
                row.child(self.arrow(Direction::Right, right))
            })
    }

    fn arrow(&self, direction: Direction, enabled: bool) -> Stateful<Div> {
        let (id, label) = match direction {
            Direction::Left => ("scroll-tabs-left", "‹"),
            Direction::Right => ("scroll-tabs-right", "›"),
        };
        let scroll = self.scroll.clone();
        let follow_selection = self.follow_selection.clone();
        div()
            .id(id)
            .flex_none()
            .w(ui_px(26.))
            .py(ui_px(5.))
            .rounded_md()
            .bg(rgb(PANEL))
            .text_color(rgb(MUTED))
            .text_size(ui_px(17.))
            .text_center()
            .child(label)
            .when(!enabled, |arrow| arrow.opacity(0.4))
            .when(enabled, |arrow| {
                arrow
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(TEXT)))
                    .on_click(move |_, window, _| {
                        let viewport = scroll.bounds();
                        let offset = scroll.offset().x;
                        let mut edges = (0..)
                            .map_while(|index| scroll.bounds_for_item(index))
                            .map(|tab| hidden_tab_edge(direction, viewport, tab, offset));
                        let next = match direction {
                            Direction::Left => edges.map_while(|edge| edge).last(),
                            Direction::Right => edges.find_map(|edge| edge),
                        };
                        if let Some(next) = next
                            && set_scroll_x(&scroll, next)
                        {
                            follow_selection.set(false);
                            window.refresh();
                        }
                    })
            })
    }
}

fn step_drag_scroll(
    window: &mut Window,
    scroll: &ScrollHandle,
    follow: &Rc<Cell<bool>>,
    pending: &Rc<Cell<bool>>,
) {
    let viewport = scroll.bounds();
    let mouse = window.mouse_position();
    let edge = ui_px(24.)
        .to_pixels(window.rem_size())
        .min(viewport.size.width / 3.);
    let speed = ui_px(9.).to_pixels(window.rem_size());
    let delta = if mouse.x < viewport.left() + edge {
        speed
    } else if mouse.x > viewport.right() - edge {
        -speed
    } else {
        px(0.)
    };
    if !viewport.contains(&mouse) || !set_scroll_x(scroll, scroll.offset().x + delta) {
        pending.set(false);
        return;
    }
    follow.set(false);
    let scroll = scroll.clone();
    let follow = follow.clone();
    let pending = pending.clone();
    window.on_next_frame(move |window, cx| {
        if cx.has_active_drag() {
            step_drag_scroll(window, &scroll, &follow, &pending);
        } else {
            pending.set(false);
        }
    });
    window.refresh();
}

fn hidden_tab_edge(
    direction: Direction,
    viewport: Bounds<Pixels>,
    tab: Bounds<Pixels>,
    offset: Pixels,
) -> Option<Pixels> {
    let next = match direction {
        Direction::Left => viewport.left() - tab.left(),
        Direction::Right => viewport.right() - tab.right(),
    };
    let hidden = match direction {
        Direction::Left => next > offset + SCROLL_EPSILON,
        Direction::Right => next < offset - SCROLL_EPSILON,
    };
    hidden.then_some(next)
}

fn reveal_offset(viewport: Bounds<Pixels>, tab: Bounds<Pixels>, offset: Pixels) -> Pixels {
    if tab.size.width > viewport.size.width {
        return viewport.left() - tab.left();
    }
    hidden_tab_edge(Direction::Left, viewport, tab, offset)
        .or_else(|| hidden_tab_edge(Direction::Right, viewport, tab, offset))
        .unwrap_or(offset)
}

fn set_scroll_x(scroll: &ScrollHandle, offset: Pixels) -> bool {
    let current = scroll.offset();
    let x = offset.clamp(-scroll.max_offset().x, px(0.));
    if (x - current.x).abs() <= SCROLL_EPSILON {
        return false;
    }
    scroll.set_offset(point(x, current.y));
    true
}

#[cfg(test)]
mod tests {
    use gpui::size;

    use super::*;

    fn bounds(left: f32, width: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(left), px(0.)),
            size: size(px(width), px(24.)),
        }
    }

    #[test]
    fn arrows_appear_only_when_tabs_overflow_without_them() {
        assert!(
            Arrows::for_scroll(px(52.), px(-52.), px(52.))
                .buttons()
                .is_none()
        );
        assert_eq!(
            Arrows::for_scroll(px(53.), px(0.), px(52.)).buttons(),
            Some((false, true))
        );
        assert_eq!(
            Arrows::for_scroll(px(53.), px(-53.), px(52.)).buttons(),
            Some((true, false))
        );
    }

    #[test]
    fn builds_the_tab_strip_with_disabled_end_arrows() {
        let bar = TabBar::default();
        for left in [false, true] {
            bar.arrows.set(Arrows::Visible { left, right: !left });
            let _ = bar.render(std::iter::empty());
        }
    }

    #[test]
    fn reveal_moves_only_enough_to_show_the_tab() {
        let viewport = bounds(100., 200.);
        assert_eq!(
            reveal_offset(viewport, bounds(130., 70.), px(-20.)),
            px(-20.)
        );
        assert_eq!(
            reveal_offset(viewport, bounds(105., 70.), px(-20.)),
            px(-5.)
        );
        assert_eq!(
            reveal_offset(viewport, bounds(295., 70.), px(-20.)),
            px(-65.)
        );
        assert_eq!(
            reveal_offset(viewport, bounds(140., 210.), px(0.)),
            px(-40.)
        );
    }

    #[test]
    fn repeated_selection_preserves_manual_scroll() {
        let mut bar = TabBar::default();
        bar.select(Some("tab-a"), false);
        assert!(bar.follow_selection.get());
        bar.follow_selection.set(false);
        bar.select(Some("tab-a"), false);
        assert!(!bar.follow_selection.get());
        bar.select(Some("tab-a"), true);
        assert!(bar.follow_selection.get());
        bar.follow_selection.set(false);
        bar.select(Some("tab-b"), false);
        assert!(bar.follow_selection.get());
    }
}
