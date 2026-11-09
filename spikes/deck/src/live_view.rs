use crate::live::{Capture, LiveDeck};
use corvid_desktop::ipc::{DesktopClient, Display};
use corvid_protocol::wire::PresentedBinding;
use gpui::{KeyUpEvent, MouseButton, Subscription, Task};

use super::*;

#[path = "pair_view.rs"]
mod pair;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    Pointer,
    Keyboard,
}

pub struct LiveView {
    focus: FocusHandle,
    deck: LiveDeck,
    client: DesktopClient,
    held: Option<(Input, Capture)>,
    pairing: pair::Pairing,
    _updates: Task<()>,
    _activation: Subscription,
}

impl LiveView {
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    pub fn new(client: DesktopClient, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut display = client.display.clone();
        let mut deck = LiveDeck::default();
        deck.update(display.borrow_and_update().clone());
        let updates = cx.spawn(async move |view, cx| {
            while display.changed().await.is_ok() {
                let display = display.borrow_and_update().clone();
                if view
                    .update(cx, |view, cx| {
                        if view.apply_display(display) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let activation = cx.observe_window_activation(window, |view, window, _| {
            if !window.is_window_active() {
                view.held = None;
            }
        });
        Self {
            focus,
            deck,
            client,
            held: None,
            pairing: pair::Pairing::default(),
            _updates: updates,
            _activation: activation,
        }
    }

    fn release(&mut self, source: Input, index: usize, cx: &mut Context<Self>) {
        if self
            .held
            .as_ref()
            .is_some_and(|(input, _)| *input == source)
            && let Some((_, capture)) = self.held.take()
            && capture.index == index
        {
            self.deck.release(&self.client, capture);
            cx.notify();
        }
    }

    fn cancel_pointer(&mut self, index: usize) {
        if self
            .held
            .as_ref()
            .is_some_and(|(input, capture)| *input == Input::Pointer && capture.index == index)
        {
            self.held = None;
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.pairing.form_open() {
            self.pair_key_down(event, cx);
            return;
        }
        if self.pairing.waiting() {
            return;
        }
        if event.is_held || event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.held = None,
            direction @ ("left" | "right") => {
                self.held = None;
                self.deck.shift_page(direction == "right");
                cx.notify();
            }
            key => {
                if let Some(index) = SHORTCUTS.iter().position(|shortcut| *shortcut == key) {
                    self.held = self
                        .deck
                        .capture(index)
                        .map(|capture| (Input::Keyboard, capture));
                }
            }
        }
    }

    fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = SHORTCUTS.iter().position(|key| *key == event.keystroke.key) {
            self.release(Input::Keyboard, index, cx);
        }
    }

    fn tile(&self, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let face = self.deck.face(index);
        let (ink, fill) = face.tone.colors();
        let capture = (!self.pairing.waiting())
            .then(|| self.deck.capture(index))
            .flatten();
        let interactive = capture.is_some();
        div()
            .id(("slot", index))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h(px(99.))
            .items_center()
            .justify_center()
            .gap(px(4.))
            .pt(px(5.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(if face.attention > 0 { ink } else { BORDER }))
            .bg(rgb(fill))
            .shadow_md()
            .when(interactive, |tile| {
                tile.cursor_pointer()
                    .hover(|style| style.border_color(rgb(ink)))
                    .active(|style| style.bg(rgb(RECESS)).shadow_none())
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _, _, _| {
                    view.held = capture.clone().map(|capture| (Input::Pointer, capture));
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |view, _, _, cx| {
                    view.release(Input::Pointer, index, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |view, _, _, _| {
                    view.cancel_pointer(index);
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(7.))
                    .left(px(8.))
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child(SHORTCUTS[index].to_ascii_uppercase()),
            )
            .when(face.attention > 0, |tile| {
                tile.child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .right(px(7.))
                        .text_size(px(10.))
                        .text_color(rgb(AMBER))
                        .child(face.attention.to_string()),
                )
            })
            .child(icon(face.glyph, 24., ink))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(face.label),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(ink))
                    .child(face.detail),
            )
    }

    fn pages(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .child(kicker("AGENTS"))
                    .children((0..self.deck.page_count()).map(|page| {
                        let selected = page == self.deck.page();
                        div()
                            .id(("agent-page", page))
                            .px(px(11.))
                            .py(px(6.))
                            .rounded_md()
                            .bg(rgb(if selected { 0x392b23 } else { PANEL }))
                            .text_color(rgb(if selected { AMBER } else { MUTED }))
                            .cursor_pointer()
                            .child(format!("{:02}", page + 1))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.held = None;
                                view.deck.select_page(page);
                                cx.notify();
                            }))
                    })),
            )
            .child(kicker("5 × 3 KEYS"))
    }

    fn targets(&self, cx: &mut Context<Self>) -> Div {
        let scope = self.deck.scope();
        let selected = self.deck.target();
        div()
            .w(px(240.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(11.))
            .p(px(16.))
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(kicker("TARGET TERMINAL"))
            .when(self.deck.can_pair() && !self.pairing.waiting(), |panel| {
                panel.child(
                    form_button("start-pairing", "Pair WezTerm…", true).on_click(cx.listener(
                        |view, _, window, cx| {
                            window.focus(&view.focus);
                            view.open_pairing(cx);
                        },
                    )),
                )
            })
            .when_some(self.pairing.notice.clone(), |panel, notice| {
                panel.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(notice),
                )
            })
            .child(
                div()
                    .id("bindings")
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .max_h(px(164.))
                    .overflow_y_scroll()
                    .children(
                        self.deck
                            .bindings()
                            .iter()
                            .enumerate()
                            .map(|(index, binding)| {
                                let binding = binding.clone();
                                let active = selected == Some(&binding);
                                div()
                                    .id(("binding", index))
                                    .p(px(8.))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(rgb(if active { 0x20332f } else { RECESS }))
                                    .text_color(rgb(if active { GREEN } else { TEXT }))
                                    .text_size(px(12.))
                                    .child(binding_label(&binding))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(scope) = scope
                                            && !view.pairing.waiting()
                                        {
                                            view.held = None;
                                            view.pairing.cancel_autoselect();
                                            view.deck.select_target(scope, binding.clone());
                                            cx.notify();
                                        }
                                    }))
                            }),
                    ),
            )
            .when(self.deck.bindings().is_empty(), |panel| {
                panel.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(self.deck.terminal_hint()),
                )
            })
            .when(
                selected.is_none() && !self.deck.bindings().is_empty(),
                |panel| {
                    panel.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("Choose the paired terminal before using a key."),
                    )
                },
            )
            .child(kicker("FOCUS RESULT"))
            .child(
                div()
                    .p(px(9.))
                    .rounded_md()
                    .bg(rgb(RECESS))
                    .text_size(px(12.))
                    .child(self.deck.result.clone().unwrap_or_else(|| {
                        "A key selects the run in your existing terminal.".into()
                    })),
            )
            .when(self.deck.overflow() > 0, |panel| {
                panel.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(format!(
                            "{} unassigned · manage in the slot board",
                            self.deck.overflow()
                        )),
                )
            })
    }
}

impl Render for LiveView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (ink, fill) = if self.deck.scope().is_some() {
            (GREEN, 0x1a2b27)
        } else {
            (AMBER, 0x392b23)
        };
        div()
            .id("live-deck")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_size(px(14.))
            .font_family("Segoe UI")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_h_full()
                    .items_center()
                    .justify_center()
                    .p(px(28.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .max_w(px(1024.))
                            .gap(px(19.))
                            .child(header(self.deck.status(), ink, fill))
                            .when(self.pairing.form_open(), |content| {
                                content.child(self.pair_form(cx))
                            })
                            .when(!self.pairing.form_open(), |content| {
                                content
                                    .child(self.pages(cx))
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(17.))
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .gap(px(10.))
                                                    .p(px(12.))
                                                    .rounded_xl()
                                                    .border_1()
                                                    .border_color(rgb(BORDER))
                                                    .bg(rgb(RECESS))
                                                    .children((0..KEY_COUNT).step_by(COLUMNS).map(
                                                        |start| {
                                                            div().flex().gap(px(10.)).children(
                                                                (start..start + COLUMNS).map(
                                                                    |index| self.tile(index, cx),
                                                                ),
                                                            )
                                                        },
                                                    )),
                                            )
                                            .child(self.targets(cx)),
                                    )
                                    .child(footer())
                            }),
                    ),
            )
    }
}

fn binding_label(binding: &PresentedBinding) -> String {
    let short: String = binding.client_id.chars().take(8).collect();
    match &binding.wezterm {
        Some(pane) => format!("WezTerm pane {} · {short}", pane.pane_id),
        None => format!("Paired TUI · {short}"),
    }
}
