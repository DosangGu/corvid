use std::thread;

use corvid_companion::{WezTermCandidate, discover_wezterm_panes};
use corvid_presentation::ipc::Command;
use corvid_protocol::wire::{PairTerminal, envelope::Body};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::*;

type Scope = (Uuid, Uuid);

#[derive(Default)]
pub(super) struct Pairing {
    draft: Option<Draft>,
    discovery: Option<Task<()>>,
    pending: Option<(Scope, Uuid)>,
    choose: Option<(Scope, PresentedBinding)>,
    pub(super) notice: Option<String>,
}

struct Draft {
    scope: Scope,
    panes: Option<anyhow::Result<Vec<WezTermCandidate>>>,
    selected: Option<usize>,
    code: String,
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

    fn cancel(&mut self) {
        self.draft = None;
        self.discovery = None;
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
            .is_some_and(|draft| Some(draft.scope) != scope)
        {
            self.pairing.cancel();
            self.pairing.notice =
                Some("Connection changed. Open a new pairing code and try again.".into());
            changed = true;
        }
        if let Some((request_scope, id)) = self.pairing.pending {
            if Some(request_scope) != scope {
                self.pairing.pending = None;
                self.pairing.notice =
                    Some("Pairing outcome unknown. Check the registered terminals.".into());
                changed = true;
            } else if let Some(completion) = completion.filter(|completion| completion.id == id) {
                self.pairing.pending = None;
                self.pairing.notice = Some(if completion.paired.is_some() {
                    "Terminal paired.".into()
                } else {
                    completion.message
                });
                self.pairing.choose = completion.paired.map(|binding| (request_scope, binding));
                changed = true;
            }
        }
        if let Some((request_scope, paired)) = &self.pairing.choose {
            if Some(*request_scope) != scope {
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
                self.deck.select_target(*request_scope, binding);
                self.pairing.choose = None;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn open_pairing(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self
            .deck
            .scope()
            .filter(|_| self.deck.can_pair() && !self.pairing.waiting())
        else {
            return;
        };
        self.held = None;
        self.pairing.notice = None;
        self.pairing.choose = None;
        self.pairing.draft = Some(Draft {
            scope,
            panes: None,
            selected: None,
            code: String::new(),
        });
        self.discover(cx);
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = &mut self.pairing.draft else {
            return;
        };
        draft.panes = None;
        draft.selected = None;
        let (mut send, receive) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("corvid-wezterm-discovery".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(async {
                        tokio::select! {
                            _ = send.closed() => {}
                            result = discover_wezterm_panes() => { let _ = send.send(result); }
                        }
                    }),
                    Err(error) => {
                        let _ = send.send(Err(error.into()));
                    }
                }
            });
        if let Err(error) = worker {
            draft.panes = Some(Err(error.into()));
        } else {
            self.pairing.discovery = Some(cx.spawn(async move |view, cx| {
                let result = receive.await.unwrap_or_else(|error| Err(error.into()));
                let _ = view.update(cx, |view, cx| {
                    view.pairing.discovery = None;
                    if let Some(draft) = &mut view.pairing.draft {
                        draft.selected = result
                            .as_ref()
                            .is_ok_and(|panes| panes.len() == 1)
                            .then_some(0);
                        draft.panes = Some(result);
                        cx.notify();
                    }
                });
            }));
        }
        cx.notify();
    }

    pub(super) fn pair_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.pairing.cancel(),
            "enter" => self.submit_pairing(cx),
            _ => {
                if let Some(draft) = &mut self.pairing.draft {
                    edit_ascii(&mut draft.code, event, cx, 36, code_byte);
                }
            }
        }
        cx.notify();
    }

    fn submit_pairing(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = &self.pairing.draft else {
            return;
        };
        let (link, generation) = draft.scope;
        if self.deck.scope() != Some(draft.scope) {
            return;
        }
        let Some(code) = pairing_code(&draft.code) else {
            return;
        };
        let Some(pane) = draft
            .selected
            .and_then(|index| draft.panes.as_ref()?.as_ref().ok()?.get(index))
        else {
            return;
        };
        let id = Uuid::new_v4();
        let request = Command {
            id,
            link,
            generation,
            page: 0,
            body: Body::PairTerminal(PairTerminal {
                code: code.to_string(),
                wezterm: Some(pane.target()),
                control_generation: Some(generation.to_string()),
            }),
        };
        if self.client.commands.try_send(request).is_ok() {
            self.pairing.pending = Some((draft.scope, id));
            self.pairing.notice = Some("Registering the terminal…".into());
            self.pairing.cancel();
        }
        cx.notify();
    }

    pub(super) fn pair_form(&self, cx: &mut Context<Self>) -> Div {
        let Some(draft) = &self.pairing.draft else {
            return div();
        };
        let panes = match &draft.panes {
            None => div().child("Looking for verified WezTerm panes…"),
            Some(Err(_)) => div().child("Could not list WezTerm panes. Refresh to try again."),
            Some(Ok(panes)) if panes.is_empty() => {
                div().child("Open the working WezTerm pane, then refresh.")
            }
            Some(Ok(panes)) => div().child(
                div()
                    .id("pair-panes")
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .max_h(px(172.))
                    .overflow_y_scroll()
                    .children(panes.iter().enumerate().map(|(index, pane)| {
                        let selected = draft.selected == Some(index);
                        div()
                            .id(("pair-pane", index))
                            .rounded_md()
                            .p(px(8.))
                            .cursor_pointer()
                            .bg(rgb(if selected { 0x20332f } else { RECESS }))
                            .text_color(rgb(if selected { GREEN } else { TEXT }))
                            .child(pane_label(pane))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(draft) = &mut view.pairing.draft {
                                    draft.selected = Some(index);
                                    cx.notify();
                                }
                            }))
                    })),
            ),
        };
        let can_submit = draft.selected.is_some() && pairing_code(&draft.code).is_some();
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(18.))
            .max_w(px(640.))
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(kicker("PAIR WEZTERM"))
            .child(
                "In your Corvid TUI, press Ctrl+] then r. Copy its one-time code, then press Esc in the terminal.",
            )
            .child(panes)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .px(px(12.))
                            .py(px(8.))
                            .rounded_md()
                            .bg(rgb(RECESS))
                            .child(if draft.code.is_empty() {
                                "Type or paste the one-time code".into()
                            } else {
                                "•".repeat(draft.code.len())
                            }),
                    )
                    .child(
                        form_button("paste-code", "Paste code", false).on_click(cx.listener(
                            |view, _, _, cx| {
                                if let Some(draft) = &mut view.pairing.draft
                                    && let Some(code) = cx
                                        .read_from_clipboard()
                                        .and_then(|item| item.text())
                                        .and_then(|text| pairing_code(&text))
                                {
                                    draft.code = code.to_string();
                                    cx.notify();
                                }
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        form_button("refresh-panes", "Refresh panes", false).on_click(cx.listener(
                            |view, _, _, cx| {
                                if view.pairing.discovery.is_none() {
                                    view.discover(cx);
                                }
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(form_button("cancel-pairing", "Cancel", false).on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.pairing.cancel();
                                    cx.notify();
                                }),
                            ))
                            .when(can_submit, |row| {
                                row.child(
                                    form_button("submit-pairing", "Pair terminal", true).on_click(
                                        cx.listener(|view, _, _, cx| view.submit_pairing(cx)),
                                    ),
                                )
                            }),
                    ),
            )
    }
}

fn code_byte(byte: u8) -> bool {
    byte.is_ascii_hexdigit() || byte == b'-'
}

fn pairing_code(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text.trim())
        .ok()
        .filter(|code| !code.is_nil())
}

fn pane_label(pane: &WezTermCandidate) -> String {
    let location = pane.location();
    let title: String = pane
        .title()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(72)
        .collect();
    let label = format!(
        "Window {} · Tab {} · Pane {}",
        location.window_id, location.tab_id, location.pane_id
    );
    if title.is_empty() {
        label
    } else {
        format!("{label} · {title}")
    }
}
