use super::*;
use gpui::{
    Bounds, ElementInputHandler, EntityInputHandler, MouseButton, Pixels, Point, UTF16Selection,
    canvas,
};
use std::ops::Range;

pub(super) struct NameDraft {
    text: String,
    marked: String,
    selected: bool,
    revision: u64,
}

impl NameDraft {
    fn append(&mut self, text: &str) {
        if self.selected {
            self.text.clear();
        }
        self.selected = false;
        self.marked.clear();
        self.text = corvid_core::layout::clean_title(&(self.text.clone() + text));
    }
}

impl WorkspaceView {
    pub(super) fn open_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let (ui, revision) = session.ui_with_revision();
        if !ui.can_write() || !ui.naming_supported || ui.changing || !ui.inventory_ready {
            return;
        }
        let Some(selected) = ui.selected.as_ref() else {
            return;
        };
        let Some(tab) = ui
            .inventory
            .as_ref()
            .and_then(|inventory| {
                inventory
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == selected.workspace_id)
            })
            .and_then(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == selected.tab_id)
            })
        else {
            return;
        };
        self.naming = Some(NameDraft {
            text: tab.label.clone(),
            marked: String::new(),
            selected: true,
            revision,
        });
        self.menu_open = false;
        self.switcher_open = false;
        window.focus(&self.setup_focus);
        cx.notify();
    }

    fn save_name(&mut self, automatic: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = self.naming.take()
            && let Some(session) = &self.session
        {
            session.rename_tab(
                if automatic { String::new() } else { draft.text },
                draft.revision,
            );
        }
        self.dismiss_overlay(window, cx);
    }

    pub(super) fn naming_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let Some(draft) = &mut self.naming else {
            return;
        };
        match key.key.as_str() {
            "escape" => self.dismiss_overlay(window, cx),
            "enter" if draft.marked.is_empty() => self.save_name(false, window, cx),
            "backspace" if draft.marked.is_empty() => {
                if draft.selected {
                    draft.text.clear();
                } else {
                    draft.text.pop();
                }
                draft.selected = false;
            }
            "a" if key.modifiers.control => draft.selected = true,
            "v" if key.modifiers.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    draft.append(&text);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn naming_panel(&self, cx: &mut Context<Self>) -> Div {
        let draft = self.naming.as_ref().expect("name editor is open");
        let entity = cx.entity();
        let focus = self.setup_focus.clone();
        let input = canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
            },
        )
        .absolute()
        .inset_0();
        let panel = div()
            .id("rename-view")
            .occlude()
            .relative()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .w(px(440.))
            .p(px(16.))
            .gap(px(12.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .text_color(rgb(TEXT))
            .child(div().text_size(px(18.)).child("Rename tab"))
            .child(
                div()
                    .relative()
                    .h(px(38.))
                    .px(px(8.))
                    .py(px(6.))
                    .overflow_hidden()
                    .bg(rgb(if draft.selected {
                        ACTIVE_FILL
                    } else {
                        BACKGROUND
                    }))
                    .child(format!("{}{}▏", draft.text, draft.marked))
                    .child(input),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("Enter saves · Esc cancels · Empty name restores automatic titles"),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(
                        button("automatic-title", "Use automatic title", false).on_click(
                            cx.listener(|view, _, window, cx| view.save_name(true, window, cx)),
                        ),
                    )
                    .child(button("save-title", "Save name", true).on_click(
                        cx.listener(|view, _, window, cx| view.save_name(false, window, cx)),
                    )),
            );
        self.overlay(panel, false, cx)
    }
}

impl EntityInputHandler for WorkspaceView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let draft = self.naming.as_ref()?;
        let units: Vec<_> = (draft.text.clone() + &draft.marked)
            .encode_utf16()
            .collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *actual = Some(range.clone());
        Some(String::from_utf16_lossy(&units[range]))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let draft = self.naming.as_ref()?;
        let end = draft.text.encode_utf16().count();
        Some(UTF16Selection {
            range: if draft.selected { 0..end } else { end..end },
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let draft = self.naming.as_ref()?;
        let start = draft.text.encode_utf16().count();
        (!draft.marked.is_empty()).then(|| start..start + draft.marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.naming {
            draft.marked.clear();
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = &mut self.naming {
            draft.append(text);
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
        if let Some(draft) = &mut self.naming {
            if draft.selected {
                draft.text.clear();
                draft.selected = false;
            }
            draft.marked = corvid_core::layout::clean_title(text);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
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
