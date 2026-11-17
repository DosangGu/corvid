use gpui::*;

pub fn build(
    _: PromptLevel,
    title: &str,
    detail: Option<&str>,
    buttons: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let modal = cx.new(|cx| Modal {
        title: title.to_owned().into(),
        detail: detail.map(|text| text.to_owned().into()),
        buttons: buttons.to_vec(),
        selected: 0,
        focus: cx.focus_handle(),
    });
    handle.with_view(modal, window, cx)
}

struct Modal {
    title: SharedString,
    detail: Option<SharedString>,
    buttons: Vec<PromptButton>,
    selected: usize,
    focus: FocusHandle,
}

impl EventEmitter<PromptResponse> for Modal {}

impl Focusable for Modal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Modal {
    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if event.is_held || self.buttons.is_empty() {
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" => cx.emit(PromptResponse(self.selected)),
            "escape" => {
                if let Some(index) = self
                    .buttons
                    .iter()
                    .position(|button| matches!(button, PromptButton::Cancel(_)))
                {
                    cx.emit(PromptResponse(index));
                }
            }
            "tab" => {
                let count = self.buttons.len();
                let step = if event.keystroke.modifiers.shift {
                    count - 1
                } else {
                    1
                };
                self.selected = (self.selected + step) % count;
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Render for Modal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000099))
            .child(
                div()
                    .w_full()
                    .max_w(px(480.))
                    .mx_4()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x424754))
                    .bg(rgb(0x23262e))
                    .text_color(rgb(0xf0f1f4))
                    .child(div().text_lg().child(self.title.clone()))
                    .children(
                        self.detail
                            .clone()
                            .map(|detail| div().text_sm().text_color(rgb(0xc4c8d1)).child(detail)),
                    )
                    .child(div().flex().justify_end().gap_2().children(
                        self.buttons.iter().enumerate().map(|(index, button)| {
                            div()
                                .id(index)
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .text_sm()
                                .bg(rgb(if index == self.selected {
                                    0x3558a8
                                } else {
                                    0x343945
                                }))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(0x4766ae)))
                                .child(button.label().clone())
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(PromptResponse(index));
                                }))
                        }),
                    )),
            )
    }
}
