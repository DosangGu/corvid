use super::*;
use corvid_presentation::ipc::Command;
use corvid_protocol::wire::{PairTerminal, envelope::Body};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Pairing {
    draft: Option<((Uuid, Uuid), String)>,
    pending: Option<((Uuid, Uuid), Uuid)>,
    choose: Option<((Uuid, Uuid), PresentedBinding)>,
    pub(super) notice: Option<String>,
}

impl Pairing {
    pub(super) fn form_open(&self) -> bool {
        self.draft.is_some()
    }
    pub(super) fn waiting(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn cancel_autoselect(&mut self) {
        self.choose = None;
    }
}

impl LiveView {
    pub(super) fn apply_display(&mut self, display: Display) -> bool {
        let completion = display.completion.clone();
        let mut changed = self.deck.update(display);
        let scope = self.deck.scope();
        if self
            .pairing
            .draft
            .as_ref()
            .is_some_and(|(old, _)| Some(*old) != scope)
        {
            self.pairing.draft = None;
            changed = true;
        }
        if let Some((old, id)) = self.pairing.pending {
            if Some(old) != scope {
                self.pairing.pending = None;
                self.pairing.notice = Some("Connection changed. Pair the terminal again.".into());
                changed = true;
            } else if let Some(reply) = completion.filter(|reply| reply.id == id) {
                self.pairing.pending = None;
                self.pairing.notice = Some(reply.message);
                self.pairing.choose = reply.paired.map(|binding| (old, binding));
                changed = true;
            }
        }
        if let Some((old, paired)) = &self.pairing.choose {
            if Some(*old) != scope {
                self.pairing.choose = None;
            } else if let Some(binding) = self
                .deck
                .bindings()
                .iter()
                .find(|binding| {
                    binding.client_id == paired.client_id && binding.revision == paired.revision
                })
                .cloned()
            {
                self.deck.select_target(*old, binding);
                self.pairing.choose = None;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn open_pairing(&mut self, cx: &mut Context<Self>) {
        if let Some(scope) = self.deck.scope() {
            self.held = None;
            self.pairing.draft = Some((scope, String::new()));
            self.pairing.notice = None;
            cx.notify();
        }
    }

    pub(super) fn pair_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.pairing.draft = None,
            "enter" => self.submit_pairing(),
            _ => {
                if let Some((_, code)) = &mut self.pairing.draft {
                    edit_ascii(code, event, cx, 36, |byte| {
                        byte.is_ascii_hexdigit() || byte == b'-'
                    });
                }
            }
        }
        cx.notify();
    }

    fn submit_pairing(&mut self) {
        let Some((scope, code)) = &self.pairing.draft else {
            return;
        };
        let Ok(code) = Uuid::parse_str(code) else {
            return;
        };
        if code.is_nil() || self.deck.scope() != Some(*scope) {
            return;
        }
        let id = Uuid::new_v4();
        let (link, generation) = *scope;
        let request = Command {
            id,
            link,
            generation,
            page: 0,
            body: Body::PairTerminal(PairTerminal {
                code: code.to_string(),
                wezterm: None,
                control_generation: Some(generation.to_string()),
            }),
        };
        if self.client.commands.try_send(request).is_ok() {
            self.pairing.pending = Some((*scope, id));
            self.pairing.draft = None;
        }
    }

    pub(super) fn pair_form(&self, _: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(20.))
            .child(
                "Open a pairing code in the existing Corvid TUI, paste it here, and press Enter.",
            )
            .child(
                self.pairing
                    .draft
                    .as_ref()
                    .map(|(_, code)| code.clone())
                    .unwrap_or_default(),
            )
            .child("Focus selects that TUI's pane. Activate its terminal window manually.")
    }
}
