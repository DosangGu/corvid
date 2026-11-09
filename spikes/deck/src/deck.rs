use gpui::{
    Context, Div, FocusHandle, FontWeight, IntoElement, KeyDownEvent, Stateful, Window, div,
    prelude::*, px, rgb,
};

use crate::icons::icon;
use crate::{COLUMNS, Deck, Frame, KEY_COUNT, Key, KeyPress, PAGES, Press};

#[path = "live_view.rs"]
pub mod live;
#[path = "setup_view.rs"]
pub mod setup;

const BACKGROUND: u32 = 0x111318;
const PANEL: u32 = 0x191c23;
const RECESS: u32 = 0x101216;
const BORDER: u32 = 0x30343f;
const TEXT: u32 = 0xf3f4f6;
const MUTED: u32 = 0xa4adbc;
const DIM: u32 = 0x818b9c;
const AMBER: u32 = 0xf7b968;
const GREEN: u32 = 0x83dec0;
const SHORTCUTS: [&str; KEY_COUNT] = [
    "1", "2", "3", "4", "5", "q", "w", "e", "r", "t", "a", "s", "d", "f", "g",
];

pub struct DeckView {
    focus: FocusHandle,
    deck: Deck,
}

impl DeckView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        Self {
            focus,
            deck: Deck::default(),
        }
    }

    fn press(&mut self, capture: KeyPress, cx: &mut Context<Self>) {
        if self.deck.press(capture).is_some() {
            cx.notify();
        }
    }

    fn select_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if self.deck.select_page(page) {
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.is_held || event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            direction @ ("left" | "right") => {
                if self.deck.shift_page(direction == "right") {
                    cx.notify();
                }
            }
            shortcut => {
                if let Some(capture) = SHORTCUTS
                    .iter()
                    .position(|key| *key == shortcut)
                    .and_then(|index| self.deck.frame().capture(index))
                {
                    self.press(capture, cx);
                }
            }
        }
    }

    fn page_tabs(&self, current: usize, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(kicker("PAGES"))
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .p(px(4.))
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(RECESS))
                            .children(PAGES.iter().enumerate().map(|(index, page)| {
                                let selected = current == index;
                                let ink = if selected { AMBER } else { MUTED };
                                div()
                                    .id(("page", index))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .px(px(13.))
                                    .h(px(34.))
                                    .rounded_md()
                                    .bg(rgb(if selected { 0x392b23 } else { RECESS }))
                                    .hover(|style| {
                                        style.bg(rgb(if selected { 0x423227 } else { PANEL }))
                                    })
                                    .text_color(rgb(ink))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(icon(page.glyph, 15., ink))
                                    .child(page.name)
                                    .on_click(cx.listener(move |deck, _, _, cx| {
                                        deck.select_page(index, cx);
                                    }))
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(format!("{:02} / {:02}", current + 1, PAGES.len())),
            )
    }

    fn key_grid(&self, frame: &Frame, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(16.))
            .p(px(18.))
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(kicker("CONTROL SURFACE"))
                    .child(kicker("5 × 3 KEYS")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(12.))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x252830))
                    .bg(rgb(RECESS))
                    .id(("layout", frame.layout_revision))
                    .children((0..KEY_COUNT).step_by(COLUMNS).map(|start| {
                        div().flex().gap(px(10.)).children(
                            (start..start + COLUMNS).map(|index| Self::key_tile(frame, index, cx)),
                        )
                    })),
            )
    }

    fn key_tile(frame: &Frame, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let visible = frame.keys[index];
        let key = visible.key;
        let (ink, face) = key.tone.colors();
        let capture = frame
            .capture(index)
            .expect("grid index is in the current frame");
        let rim = if visible.highlighted() { ink } else { 0x373c47 };

        div()
            .id(("key", index))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h(px(99.))
            .items_center()
            .justify_center()
            .gap(px(7.))
            .pt(px(8.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(rim))
            .bg(rgb(face))
            .shadow_md()
            .cursor_pointer()
            .hover(|style| style.border_color(rgb(ink)))
            .active(|style| style.bg(rgb(RECESS)).shadow_none())
            .on_click(cx.listener(move |deck, _, _, cx| deck.press(capture, cx)))
            .child(
                div()
                    .absolute()
                    .top(px(8.))
                    .left(px(9.))
                    .text_size(px(10.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(MUTED))
                    .child(SHORTCUTS[index].to_ascii_uppercase()),
            )
            .when_some(visible.enabled, |tile, enabled| {
                tile.child(
                    div()
                        .absolute()
                        .top(px(11.))
                        .right(px(10.))
                        .child(dot(if enabled { ink } else { 0x697281 }, 6.)),
                )
            })
            .child(icon(key.glyph, 28., ink))
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(key.label),
            )
    }

    fn activity(&self) -> Div {
        let history = self.deck.history();
        let latest = history.front().copied();
        let details = latest.map(|press| (press, PAGES[press.page].keys[press.index]));
        let (symbol, ink, face, title, subtitle) = match details {
            Some((press, key)) => {
                let (ink, face) = key.tone.colors();
                (
                    key.glyph,
                    ink,
                    face,
                    key.label,
                    format!("{} · Key {:02}", PAGES[press.page].name, press.index + 1),
                )
            }
            None => (
                "activity",
                MUTED,
                0x242934,
                "Ready for input",
                "Press any key on the deck.".to_owned(),
            ),
        };

        div()
            .w(px(240.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(17.))
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(kicker("ACTIVITY"))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!("{:02} PRESSES", self.deck.press_count())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(7.))
                    .p(px(14.))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(RECESS))
                    .child(icon_badge(symbol, ink, face, 41.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(subtitle),
                    )
                    .when_some(latest, |card, press| {
                        card.child(pill(press.outcome.label(), ink, face))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(9.))
                    .child(kicker("PREVIOUS PRESSES"))
                    .when(history.len() < 2, |list| {
                        list.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(DIM))
                                .child("No earlier presses"),
                        )
                    })
                    .children(history.iter().skip(1).map(|press| history_row(*press))),
            )
    }
}

impl Render for DeckView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.deck.frame();
        let content = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(1024.))
            .gap(px(19.))
            .child(header("DEMO", GREEN, 0x1a2b27))
            .child(self.page_tabs(frame.page, cx))
            .child(
                div()
                    .flex()
                    .gap(px(17.))
                    .child(self.key_grid(&frame, cx))
                    .child(self.activity()),
            )
            .child(footer());

        div()
            .id("deck")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
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
                    .items_center()
                    .justify_center()
                    .min_h_full()
                    .p(px(28.))
                    .child(content),
            )
    }
}

fn header(mode: &'static str, ink: u32, fill: u32) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .pb(px(15.))
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(15.))
                .child(icon_badge("brand", AMBER, 0x3b2b22, 53.))
                .child(
                    div().flex().flex_col().child(kicker("CORVID")).child(
                        div()
                            .text_size(px(31.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Control Deck"),
                    ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(11.))
                .py(px(6.))
                .rounded_full()
                .border_1()
                .border_color(rgb(0x355149))
                .bg(rgb(fill))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(ink))
                .child(dot(ink, 6.))
                .child(mode),
        )
}

fn footer() -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .text_size(px(12.))
        .text_color(rgb(MUTED))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(icon("keyboard", 17., MUTED))
                .child("1–5  ·  Q W E R T  ·  A S D F G"),
        )
        .child("← →  Switch pages")
}

fn history_row(press: Press) -> Div {
    let Key {
        label, glyph, tone, ..
    } = PAGES[press.page].keys[press.index];
    let (ink, face) = tone.colors();
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(icon_badge(glyph, ink, face, 26.))
        .child(div().flex_1().text_size(px(12.)).child(label))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(MUTED))
                .child(press.outcome.label()),
        )
}

fn kicker(label: &'static str) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
}

fn pill(label: &'static str, color: u32, fill: u32) -> Div {
    div()
        .px(px(7.))
        .py(px(3.))
        .rounded_md()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color))
        .bg(rgb(fill))
        .child(label)
}

fn icon_badge(path: &'static str, color: u32, fill: u32, size: f32) -> Div {
    div()
        .flex()
        .flex_none()
        .size(px(size))
        .items_center()
        .justify_center()
        .rounded_lg()
        .bg(rgb(fill))
        .child(icon(path, size * 0.56, color))
}

fn dot(color: u32, size: f32) -> Div {
    div().size(px(size)).rounded_full().bg(rgb(color))
}

fn form_button(id: &'static str, label: &'static str, primary: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(12.))
        .py(px(7.))
        .rounded_md()
        .bg(rgb(if primary { 0x20332f } else { RECESS }))
        .text_color(rgb(if primary { GREEN } else { MUTED }))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|style| style.bg(rgb(0x303846)))
        .child(label)
}

fn edit_ascii(
    value: &mut String,
    event: &KeyDownEvent,
    cx: &gpui::App,
    limit: usize,
    allowed: impl Fn(u8) -> bool,
) {
    let key = &event.keystroke;
    if key.modifiers.control && key.key == "v" {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.trim();
            if text.len() <= limit && text.bytes().all(&allowed) {
                *value = text.to_owned();
            }
        }
    } else if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
        if key.key == "backspace" {
            value.pop();
        } else {
            let text = key.key_char.as_deref().unwrap_or(&key.key);
            if text.len() == 1 && value.len() < limit && text.bytes().all(&allowed) {
                value.push_str(text);
            }
        }
    }
}
