use super::*;
use anyhow::Result;
use corvid_companion::{WezTermCandidate, discover_wezterm_panes};
use corvid_protocol::wire::PairTerminal;
use std::thread;
use tokio::runtime::Builder;
use tokio::sync::oneshot::{self, error::TryRecvError};

#[derive(Default)]
pub(crate) struct Pairing {
    draft: Option<Draft>,
}
struct Draft {
    discovery: Discovery,
    selected: Option<usize>,
    code: String,
}
enum Discovery {
    Loading(oneshot::Receiver<Result<Vec<WezTermCandidate>>>),
    Ready(Vec<WezTermCandidate>),
    Failed,
}

impl Pairing {
    pub fn open(&mut self, signal: &UiSignal) {
        self.draft = Some(Draft {
            discovery: Discovery::start(signal),
            selected: None,
            code: String::new(),
        });
    }
    pub fn close(&mut self) {
        self.draft = None;
    }
    pub fn is_open(&self) -> bool {
        self.draft.is_some()
    }
    pub fn poll(&mut self) {
        if let Some(draft) = &mut self.draft {
            draft.discovery.poll();
        }
    }
    pub fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Desktop>) {
        let Some(draft) = &mut self.draft else {
            return;
        };
        if event.keystroke.modifiers.control && event.keystroke.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                draft.code = text
                    .trim()
                    .chars()
                    .filter(|c| c.is_ascii_hexdigit() || *c == '-')
                    .take(36)
                    .collect();
            }
        } else if event.keystroke.key == "backspace" {
            draft.code.pop();
        } else if !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.platform
            && let Some(text) = &event.keystroke.key_char
        {
            for ch in text.chars().filter(|c| c.is_ascii_hexdigit() || *c == '-') {
                if draft.code.len() < 36 {
                    draft.code.push(ch);
                }
            }
        }
        cx.notify();
    }
}

impl Desktop {
    pub(super) fn pair_form(&self, cx: &mut Context<Self>) -> Div {
        let Some(draft) = &self.pairing.draft else {
            return column();
        };
        let mut view = column()
            .child("Pair WezTerm terminal")
            .child(muted(
                "In the attached Corvid TUI, press Ctrl-] then r to show its one-time code.",
            ))
            .child(
                button("refresh-panes", "Refresh panes", true).on_click(cx.listener(
                    |view, _, _, cx| {
                        view.pairing.open(&view.signal);
                        cx.notify();
                    },
                )),
            );
        match &draft.discovery {
            Discovery::Loading(_) => {
                view = view.child(muted("Looking for verified WezTerm panes…"))
            }
            Discovery::Failed => {
                view = view.child(muted("Could not list WezTerm panes. Refresh to try again."))
            }
            Discovery::Ready(panes) => {
                if panes.is_empty() {
                    view = view.child(muted("No verified panes found. Open WezTerm and refresh."));
                }
                view = view.children(panes.iter().enumerate().map(|(index, pane)| {
                    button(("pane", index), pane_label(pane), true)
                        .when(draft.selected == Some(index), |button| {
                            button.text_color(rgb(MINT))
                        })
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(draft) = &mut view.pairing.draft {
                                draft.selected = Some(index);
                            }
                            cx.notify();
                        }))
                }));
            }
        }
        let code = Uuid::parse_str(draft.code.trim())
            .ok()
            .filter(|code| !code.is_nil());
        let target = match &draft.discovery {
            Discovery::Ready(panes) => draft
                .selected
                .and_then(|index| panes.get(index))
                .map(|pane| pane.target()),
            _ => None,
        };
        let can_submit = code.is_some()
            && target.is_some()
            && self
                .view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| self.ready(snapshot));
        view.child(
            div()
                .id("pair-code")
                .p(px(12.))
                .border_1()
                .border_color(rgb(MINT))
                .child(if draft.code.is_empty() {
                    "Type or paste the one-time code".to_owned()
                } else {
                    "•".repeat(draft.code.len())
                }),
        )
        .child(
            row()
                .child(button("cancel-pair", "Cancel", true).on_click(cx.listener(
                    |view, _, _, cx| {
                        view.pairing.close();
                        cx.notify();
                    },
                )))
                .child(button("submit-pair", "Pair terminal", can_submit).when(
                    can_submit,
                    |button| {
                        button.on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(snapshot) = view.view.snapshot.clone()
                                && let Some(generation) = snapshot.generation()
                                && view.ready(&snapshot)
                            {
                                view.submit(
                                    &snapshot,
                                    Body::PairTerminal(PairTerminal {
                                        code: code.unwrap().to_string(),
                                        wezterm: target.clone(),
                                        control_generation: Some(generation.to_string()),
                                    }),
                                );
                                view.pairing.close();
                                cx.notify();
                            }
                        }))
                    },
                )),
        )
    }
}

impl Discovery {
    fn start(ctx: &crate::ui_signal::UiSignal) -> Self {
        let (mut send, receive) = oneshot::channel();
        let ctx = ctx.clone();
        let worker = thread::Builder::new()
            .name("corvid-wezterm-discovery".to_owned())
            .spawn(move || {
                match Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime.block_on(async move {
                        tokio::select! {
                            biased;
                            _ = send.closed() => {}
                            result = discover_wezterm_panes() => { let _ = send.send(result); }
                        }
                    }),
                    Err(error) => {
                        let _ = send.send(Err(error.into()));
                    }
                }
                ctx.notify();
            });
        match worker {
            Ok(_) => Self::Loading(receive),
            Err(_) => Self::Failed,
        }
    }

    fn poll(&mut self) {
        let Self::Loading(receive) = self else {
            return;
        };
        *self = match receive.try_recv() {
            Ok(Ok(panes)) => Self::Ready(panes),
            Ok(Err(_)) | Err(TryRecvError::Closed) => Self::Failed,
            Err(TryRecvError::Empty) => return,
        };
    }
}

fn pane_label(pane: &WezTermCandidate) -> String {
    let location = pane.location();
    let label = format!(
        "GUI {} / window {} / tab {} / pane {}",
        location.gui_pid, location.window_id, location.tab_id, location.pane_id
    );
    if pane.title().is_empty() {
        label
    } else {
        format!("{label} — {}", pane.title())
    }
}
