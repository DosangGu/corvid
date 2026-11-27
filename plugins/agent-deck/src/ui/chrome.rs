use super::*;
use crate::shell::{BAR_HEIGHT, Orientation};
use gpui::{AssetSource, Transformation, radians, svg};
use std::borrow::Cow;

const PIN: &str = "window-pin.svg";
const EXPAND: &str = "window-expand.svg";
const SHRINK: &str = "window-shrink.svg";
const CLOSE: &str = "window-close.svg";
pub(super) const CLAUDE: &str = "provider-claude.svg";
pub(super) const CODEX: &str = "provider-codex.svg";
pub(super) const ANTIGRAVITY: &str = "provider-antigravity.svg";
pub(super) const AGENT: &str = "provider-agent.svg";

pub(crate) struct DeckAssets;

impl AssetSource for DeckAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let icon = match path {
            PIN => include_bytes!("../../assets/window-pin.svg").as_slice(),
            EXPAND => include_bytes!("../../assets/window-expand.svg").as_slice(),
            SHRINK => include_bytes!("../../assets/window-shrink.svg").as_slice(),
            CLOSE => include_bytes!("../../assets/window-close.svg").as_slice(),
            CLAUDE => include_bytes!("../../assets/provider-claude.svg").as_slice(),
            CODEX => include_bytes!("../../assets/provider-codex.svg").as_slice(),
            ANTIGRAVITY => include_bytes!("../../assets/provider-antigravity.svg").as_slice(),
            AGENT => include_bytes!("../../assets/provider-agent.svg").as_slice(),
            _ => return Ok(None),
        };
        Ok(Some(Cow::Borrowed(icon)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok([
            PIN,
            EXPAND,
            SHRINK,
            CLOSE,
            CLAUDE,
            CODEX,
            ANTIGRAVITY,
            AGENT,
        ]
        .into_iter()
        .filter(|name| name.starts_with(path))
        .map(SharedString::from)
        .collect())
    }
}

impl DeckView {
    pub(super) fn window_header(&self, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let expanded = self.window.expanded;
        let layout = self.window.layout();
        let compact_macos = cfg!(target_os = "macos") && !expanded;
        let portrait = !expanded && layout.orientation == Orientation::Portrait;
        let tray = self
            .session
            .background
            .view
            .borrow()
            .capabilities
            .status_icon
            .ready();
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
                title.child(colors.muted(layout.label()).flex_shrink_0())
            });

        #[cfg(not(target_os = "macos"))]
        let pinned = self.window.pinned();
        #[cfg(not(target_os = "macos"))]
        let title = title.when(!pinned, |title| {
            title.cursor_grab().on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    view.window.start_move(window);
                    cx.notify();
                }),
            )
        });

        #[cfg(not(target_os = "macos"))]
        let pin_tip = if pinned {
            "Unpin window · Allow movement and release always on top"
        } else {
            "Pin window · Lock position and keep on top"
        };
        let size_icon = if expanded { SHRINK } else { EXPAND };
        let size_tip = if expanded { "Mini deck" } else { "Expand Deck" };
        let header = row()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .gap(px(4.))
            .px(px(8.))
            .bg(colors.bar)
            .child(title);
        #[cfg(not(target_os = "macos"))]
        let header = header.child(
            icon_button(colors, PIN, pin_tip, pinned, if pinned { 0. } else { 35. }).on_click(
                cx.listener(|view, _, window, cx| {
                    view.window.toggle_pin();
                    view.update(window, cx);
                }),
            ),
        );
        header
            .when(compact_macos && self.queue_len() > 0, |header| {
                header.child(self.queue_button(true, cx))
            })
            .child(
                icon_button(colors, size_icon, size_tip, false, 0.).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.switch_view(!view.window.expanded, Section::Slots, window, cx);
                    },
                )),
            )
            .child(
                icon_button(
                    colors,
                    CLOSE,
                    if tray && !cfg!(target_os = "macos") {
                        "Hide to tray"
                    } else {
                        "Close"
                    },
                    false,
                    0.,
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    if view
                        .session
                        .background
                        .view
                        .borrow()
                        .capabilities
                        .status_icon
                        .ready()
                    {
                        view.window.hide(window);
                    } else {
                        cx.quit();
                    }
                })),
            )
    }

    pub(super) fn queue_button(&self, compact: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let overflow = self.queue_len();
        let pending = self.queue_waiting;
        let label = if compact || pending == 0 {
            format!("+{overflow}")
        } else {
            format!("+{overflow} · {pending} waiting")
        };
        self.colors
            .button("open-queue", label, true)
            .when(compact, |button| {
                let tooltip = SharedString::from(if pending == 0 {
                    format!("{overflow} queued")
                } else {
                    format!("{overflow} queued · {pending} waiting")
                });
                button
                    .when(pending > 0, |button| {
                        button.text_color(self.colors.attention)
                    })
                    .tooltip(move |_, cx| cx.new(|_| HeaderTooltip(tooltip.clone())).into())
            })
            .on_click(cx.listener(|view, _, window, cx| view.expand(Section::Overflow, window, cx)))
    }
}

fn icon_button(
    colors: Colors,
    icon: &'static str,
    tip: &'static str,
    selected: bool,
    degrees: f32,
) -> Stateful<Div> {
    let color = if selected { colors.accent } else { colors.text };
    div()
        .id(icon)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(30.))
        .rounded_md()
        .cursor_pointer()
        .when(selected, |button| button.bg(colors.selection))
        .hover(|style| {
            style.bg(if selected {
                colors.selection_hover
            } else {
                colors.hover
            })
        })
        .tooltip(move |_, cx| cx.new(|_| HeaderTooltip(tip.into())).into())
        .child(
            svg()
                .path(icon)
                .size(px(16.))
                .text_color(color)
                .with_transformation(Transformation::rotate(radians(degrees.to_radians()))),
        )
}

struct HeaderTooltip(SharedString);

impl Render for HeaderTooltip {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::for_window(window);
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded_md()
            .bg(colors.panel)
            .text_color(colors.text)
            .text_size(px(12.))
            .max_w(px(270.))
            .child(self.0.clone())
    }
}
