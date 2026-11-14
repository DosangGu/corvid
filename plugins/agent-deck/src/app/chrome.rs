use super::*;
use crate::window::{BAR_HEIGHT, Orientation};
use gpui::{AssetSource, Transformation, radians, svg};
use std::borrow::Cow;

const PIN: &str = "window-pin.svg";
const EXPAND: &str = "window-expand.svg";
const SHRINK: &str = "window-shrink.svg";
const CLOSE: &str = "window-close.svg";

pub(crate) struct DeckAssets;

impl AssetSource for DeckAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let icon = match path {
            PIN => include_bytes!("../../assets/window-pin.svg").as_slice(),
            EXPAND => include_bytes!("../../assets/window-expand.svg").as_slice(),
            SHRINK => include_bytes!("../../assets/window-shrink.svg").as_slice(),
            CLOSE => include_bytes!("../../assets/window-close.svg").as_slice(),
            _ => return Ok(None),
        };
        Ok(Some(Cow::Borrowed(icon)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok([PIN, EXPAND, SHRINK, CLOSE]
            .into_iter()
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

impl Desktop {
    pub(super) fn window_header(&self, cx: &mut Context<Self>) -> Div {
        let pinned = self.window.pinned();
        let expanded = self.window.expanded;
        let layout = self.window.layout();
        let portrait = !expanded && layout.orientation == Orientation::Portrait;
        let tray = self.background.view.borrow().background;
        let title = div()
            .id("drag-title")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(6.))
            .overflow_hidden()
            .whitespace_nowrap()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(if portrait {
                "Agents"
            } else {
                "Agent Deck"
            }))
            .when(!expanded, |title| {
                title.child(muted(layout.label()).flex_shrink_0())
            })
            .when(!pinned, |title| {
                let title = title.cursor_grab();
                #[cfg(windows)]
                let title = title.window_control_area(gpui::WindowControlArea::Drag);
                #[cfg(target_os = "linux")]
                let title = title.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, window, _| {
                        if !view.window.pinned() {
                            window.start_window_move();
                        }
                    }),
                );
                title
            });

        let pin_tip = if pinned {
            "Unpin window · Allow movement and release always on top"
        } else {
            "Pin window · Lock position and keep on top"
        };
        let size_icon = if expanded { SHRINK } else { EXPAND };
        let size_tip = if expanded { "Mini deck" } else { "Expand Deck" };
        row()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .gap(px(4.))
            .px(px(8.))
            .bg(rgb(PANEL))
            .child(title)
            .child(
                icon_button(PIN, pin_tip, pinned, if pinned { 0. } else { 35. }).on_click(
                    cx.listener(|view, _, window, cx| {
                        view.window.toggle_pin();
                        view.update(window, cx);
                    }),
                ),
            )
            .child(
                icon_button(size_icon, size_tip, false, 0.).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.switch_view(!view.window.expanded, Section::Slots, window, cx);
                    },
                )),
            )
            .child(
                icon_button(
                    CLOSE,
                    if tray { "Hide to tray" } else { "Close" },
                    false,
                    0.,
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    if view.background.view.borrow().background {
                        view.window.hide(window);
                    } else {
                        cx.quit();
                    }
                })),
            )
    }
}

fn icon_button(
    icon: &'static str,
    tip: &'static str,
    selected: bool,
    degrees: f32,
) -> Stateful<Div> {
    let color = if selected { MINT } else { TEXT };
    div()
        .id(icon)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(30.))
        .rounded_md()
        .cursor_pointer()
        .when(selected, |button| button.bg(rgb(0x234238)))
        .hover(|style| style.bg(rgb(if selected { 0x31574a } else { 0x263541 })))
        .tooltip(move |_, cx| cx.new(|_| HeaderTooltip(tip)).into())
        .child(
            svg()
                .path(icon)
                .size(px(16.))
                .text_color(rgb(color))
                .with_transformation(Transformation::rotate(radians(degrees.to_radians()))),
        )
}

struct HeaderTooltip(&'static str);

impl Render for HeaderTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded_md()
            .bg(rgb(PANEL))
            .text_color(rgb(TEXT))
            .text_size(px(12.))
            .child(self.0)
    }
}
