use std::thread;

use anyhow::Result;
use corvid_companion::{WezTermCandidate, discover_wezterm_panes};
use corvid_protocol::wire::{PairTerminal, envelope::Body};
use eframe::egui::{self, Context, Ui};
use tokio::runtime::Builder;
use tokio::sync::oneshot::{self, error::TryRecvError};
use uuid::Uuid;

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
    pub(crate) fn open(&mut self, ctx: &Context) {
        self.draft = Some(Draft {
            discovery: Discovery::start(ctx),
            selected: None,
            code: String::new(),
        });
    }

    pub(crate) fn close(&mut self) {
        self.draft = None;
    }

    pub(crate) fn show(&mut self, ctx: &Context, host: Uuid, generation: Uuid) -> Option<Body> {
        let draft = self.draft.as_mut()?;
        draft.discovery.poll();
        let modal = egui::Modal::new(egui::Id::new("wezterm-pairing"))
            .show(ctx, |ui| draft.form(ui, host, generation));
        if modal.should_close() || modal.inner.is_some() {
            self.close();
        }
        modal.inner
    }
}

impl Draft {
    fn form(&mut self, ui: &mut Ui, host: Uuid, generation: Uuid) -> Option<Body> {
        ui.set_width(430.0);
        ui.heading("Pair WezTerm terminal");
        ui.label(format!("Host {host}"));
        ui.label("In the attached Corvid TUI, press Ctrl-] then r to show its one-time code.");

        let loading = matches!(self.discovery, Discovery::Loading(_));
        if ui
            .add_enabled(!loading, egui::Button::new("Refresh panes"))
            .clicked()
        {
            self.selected = None;
            self.discovery = Discovery::start(ui.ctx());
        }
        let target = match &self.discovery {
            Discovery::Loading(_) => {
                ui.label("Looking for verified WezTerm panes…");
                None
            }
            Discovery::Ready(panes) if panes.is_empty() => {
                ui.label("No verified panes found. Open WezTerm and refresh.");
                None
            }
            Discovery::Ready(panes) => {
                let selected = self.selected.map(|index| pane_label(&panes[index]));
                egui::ComboBox::from_id_salt("wezterm-pane-choice")
                    .selected_text(
                        selected
                            .as_deref()
                            .unwrap_or("Choose the pane showing the code"),
                    )
                    .show_ui(ui, |ui| {
                        for (index, pane) in panes.iter().enumerate() {
                            ui.selectable_value(&mut self.selected, Some(index), pane_label(pane));
                        }
                    });
                self.selected.map(|index| panes[index].target())
            }
            Discovery::Failed => {
                ui.label("Could not list WezTerm panes. Refresh to try again.");
                None
            }
        };

        ui.label("One-time pairing code");
        ui.add(
            egui::TextEdit::singleline(&mut self.code)
                .password(true)
                .char_limit(36),
        );
        let code = Uuid::parse_str(self.code.trim())
            .ok()
            .filter(|code| !code.is_nil());
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                ui.close();
            }
            if ui
                .add_enabled(
                    target.is_some() && code.is_some(),
                    egui::Button::new("Pair terminal"),
                )
                .clicked()
            {
                Some(Body::PairTerminal(PairTerminal {
                    code: code?.to_string(),
                    wezterm: target,
                    control_generation: Some(generation.to_string()),
                }))
            } else {
                None
            }
        })
        .inner
    }
}

impl Discovery {
    fn start(ctx: &Context) -> Self {
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
                ctx.request_repaint();
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
