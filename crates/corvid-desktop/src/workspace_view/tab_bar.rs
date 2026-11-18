use std::cell::Cell;
use std::rc::Rc;

use gpui::{Bounds, Div, Pixels, ScrollHandle, Stateful, div, point, prelude::*, px, rgb};

use super::{MUTED, button, ui_px};

const SCROLL_EPSILON: Pixels = px(0.01);

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

#[derive(Default)]
pub(super) struct TabBar {
    scroll: ScrollHandle,
    arrows: Rc<Cell<Arrows>>,
    selection: Option<String>,
    reveal_pending: Rc<Cell<bool>>,
}

impl TabBar {
    pub(super) fn select(&mut self, tab: Option<&str>) {
        if self.selection.as_deref() != tab {
            self.selection = tab.map(str::to_owned);
            self.reveal_pending.set(tab.is_some());
        }
    }

    pub(super) fn render<'a>(&self, tabs: impl Iterator<Item = (&'a str, Stateful<Div>)>) -> Div {
        let mut reveal_index = None;
        let selected = self
            .selection
            .as_deref()
            .filter(|_| self.reveal_pending.get());
        let strip = div()
            .id("daemon-tabs")
            .flex()
            .flex_1()
            .min_w_0()
            .gap(ui_px(3.))
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            .children(tabs.enumerate().map(|(index, (id, tab))| {
                if Some(id) == selected {
                    reveal_index = Some(index);
                }
                tab
            }));

        let arrows = self.arrows.clone();
        let shown = arrows.get();
        let scroll = self.scroll.clone();
        let reveal_pending = self.reveal_pending.clone();
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .on_children_prepainted(move |children, window, _| {
                let reserved_width = match children.as_slice() {
                    [left, _, right] => left.size.width + right.size.width,
                    _ => px(0.),
                };
                let max = scroll.max_offset().width;
                let offset = scroll.offset().x;
                let next = Arrows::for_scroll(max, offset, reserved_width);
                let viewport = scroll.bounds();
                // The arrow layout must settle before revealing the selected tab.
                let offset_changed = if (shown == Arrows::Hidden) == (next == Arrows::Hidden)
                    && let Some(index) = reveal_index
                    && viewport.size.width > px(0.)
                    && let Some(tab) = scroll.bounds_for_item(index)
                {
                    reveal_pending.set(false);
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
        button(id, label, false)
            .w(ui_px(26.))
            .px_0()
            .text_center()
            .when(!enabled, |arrow| {
                arrow
                    .opacity(0.4)
                    .cursor_default()
                    .hover(|style| style.text_color(rgb(MUTED)))
            })
            .when(enabled, |arrow| {
                arrow.on_click(move |_, window, _| {
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
                        window.refresh();
                    }
                })
            })
    }
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
    let x = offset.clamp(-scroll.max_offset().width, px(0.));
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
    fn repeated_selection_preserves_manual_scroll_after_reveal() {
        let mut bar = TabBar::default();
        bar.select(Some("tab-a"));
        assert!(bar.reveal_pending.get());
        bar.reveal_pending.set(false);
        bar.select(Some("tab-a"));
        assert!(!bar.reveal_pending.get());
        bar.select(Some("tab-b"));
        assert!(bar.reveal_pending.get());
    }
}
