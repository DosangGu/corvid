use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentProvider, CompanionConnection, EvidenceConfidence,
    LayoutRecovery, PresentedBinding, PresentedRun, PresentedRunKey, PresentedSlot,
    RunAvailability,
};
use eframe::egui::{self, Color32, FontId, Frame, Margin, RichText, TextStyle, Ui};
use uuid::Uuid;

use corvid_presentation::ipc::{Command, Completion, DesktopClient, Display, Link};
use corvid_presentation::model::Snapshot;

use crate::background::Background;
use crate::catalog::Catalog;
use crate::notifications::{Notice, now_ms};
use crate::parent::DesktopLink;

mod deck;

pub(crate) fn configure(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = deck::PANEL;
    visuals.window_fill = deck::PANEL;
    visuals.extreme_bg_color = deck::BACKGROUND;
    visuals.selection.bg_fill = Color32::from_rgb(38, 72, 65);
    visuals.selection.stroke.color = Color32::from_rgb(119, 216, 183);
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(23.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(14.0));
        let muted = if style.visuals.dark_mode { 155 } else { 85 };
        style.visuals.weak_text_color = Some(Color32::from_gray(muted));
    });
}

pub(crate) struct Desktop {
    client: DesktopClient,
    parent: DesktopLink,
    background: Background,
    view: Display,
    binding: Option<PresentedBinding>,
    section: Section,
    page: u32,
    pending: Option<Uuid>,
    notice: Option<Completion>,
    pending_since: Option<Instant>,
    notifications_seen: u64,
    selected_notice: Option<Notice>,
    quitting: bool,
    catalog_scope: (PathBuf, Uuid),
    catalog: Option<Result<Catalog, String>>,
    #[cfg(windows)]
    pairing: crate::pairing::Pairing,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    #[default]
    Slots,
    Overflow,
    History,
    Notifications,
    Hosts,
}

impl Desktop {
    pub(crate) fn new(
        client: DesktopClient,
        background: Background,
        catalog_scope: (PathBuf, Uuid),
    ) -> Self {
        Self {
            client,
            parent: background.desktop.clone(),
            background,
            view: Display::default(),
            binding: None,
            section: Section::Slots,
            page: 0,
            pending: None,
            notice: None,
            pending_since: None,
            notifications_seen: 0,
            selected_notice: None,
            quitting: false,
            catalog_scope,
            catalog: None,
            #[cfg(windows)]
            pairing: crate::pairing::Pairing::default(),
        }
    }

    fn refresh_view(&mut self) {
        let next = self.client.display.borrow_and_update().clone();
        if let Some(completion) = &next.completion
            && self.pending == Some(completion.id)
        {
            self.pending = None;
            self.notice = Some(completion.clone());
        }
        if let Ok(completion) = self.background.completed.try_recv()
            && self.pending == Some(completion.id)
        {
            self.pending = None;
            self.notice = Some(completion);
        }
        match (&self.view.snapshot, &next.snapshot) {
            (Some(previous), Some(current))
                if next.link == Link::Online
                    && previous.link == current.link
                    && previous.generation() == current.generation() =>
            {
                self.binding
                    .take_if(|binding| !current.contains_binding(binding));
            }
            _ => {
                self.binding = None;
                #[cfg(windows)]
                self.pairing.close();
            }
        }
        self.view = next;
        let background = self.background.view.borrow().clone();
        if background.notifications_open != self.notifications_seen {
            self.notifications_seen = background.notifications_open;
            self.selected_notice = background.selected;
            self.section = Section::Notifications;
        }
        if self.pending.is_some()
            && self
                .pending_since
                .is_some_and(|time| time.elapsed() > Duration::from_secs(8))
        {
            let id = self.pending.take().unwrap();
            self.notice = Some(Completion {
                id,
                message:
                    "The action was not confirmed. Review the current state before trying again."
                        .into(),
                failed: true,
                paired: None,
            });
        }
        if let Some(snapshot) = &self.view.snapshot
            && self.page >= snapshot.page_count()
        {
            self.set_page(snapshot.page_count() - 1);
        }
    }

    fn submit(&mut self, snapshot: &Snapshot, body: Body) {
        let Some(generation) = snapshot.generation() else {
            return;
        };
        let id = Uuid::new_v4();
        let command = Command {
            id,
            link: snapshot.link,
            page: self.page,
            generation,
            body,
        };
        if !self.client.display.borrow().accepts(&command) {
            self.notice = Some(Completion {
                id,
                message: "The display changed during the gesture. Review the slots and try again."
                    .to_owned(),
                failed: true,
                paired: None,
            });
            return;
        }
        if self.client.commands.try_send(command).is_ok() {
            self.pending = Some(id);
            self.pending_since = Some(Instant::now());
            self.notice = None;
        } else {
            self.notice = Some(Completion {
                id,
                message: "Could not send the action. Wait for the companion and try again."
                    .to_owned(),
                failed: true,
                paired: None,
            });
        }
    }

    fn set_page(&mut self, page: u32) {
        self.page = page;
        self.client.page.send_replace(page);
    }

    fn ready(&self, snapshot: &Snapshot) -> bool {
        self.pending.is_none()
            && self.view.link == Link::Online
            && snapshot.page == self.page
            && snapshot.generation().is_some()
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Agent Deck");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut self.section, Section::Notifications, "Notifications");
                ui.selectable_value(&mut self.section, Section::Hosts, "Hosts");
                if matches!(self.section, Section::Hosts | Section::Notifications) {
                    ui.selectable_value(&mut self.section, Section::Slots, "Deck");
                }
            });
        });
        ui.horizontal(|ui| {
            let host = self
                .view
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.host.to_string());
            let label = host.as_deref().map_or_else(
                || "Corvid".into(),
                |host| format!("Corvid · {}", short_id(host)),
            );
            let response = ui.weak(label);
            if let Some(host) = host {
                response.on_hover_text(format!("Host {host}"));
            }
            let online = self.view.link == Link::Online
                && self
                    .view
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.generation())
                    .is_some();
            let color = if online {
                Color32::from_rgb(119, 216, 183)
            } else {
                ui.visuals().warn_fg_color
            };
            ui.label(
                RichText::new(if online {
                    "Connected"
                } else {
                    "Check connection"
                })
                .size(12.0)
                .color(color),
            )
            .on_hover_text(connection_text(&self.view));
        });
        if self.section == Section::Hosts {
            return;
        }
        if let Some(snapshot) = self.view.snapshot.clone() {
            if self.view.link != Link::Online || snapshot.generation().is_none() {
                if let Some(seen) = self.view.last_ready {
                    ui.label(format!(
                        "Last ready snapshot seen {}s ago",
                        seen.elapsed().as_secs()
                    ));
                } else {
                    ui.label("Waiting for the first ready snapshot.");
                }
            }
            if let Some(code) = snapshot
                .data
                .issue_code
                .as_deref()
                .filter(|code| safe_code(code))
            {
                ui.label(format!("Connection issue ({code})"));
            }
            match LayoutRecovery::try_from(snapshot.data.layout_recovery) {
                Ok(LayoutRecovery::Backup) => {
                    ui.label("Slot layout recovered from its backup.");
                }
                Ok(LayoutRecovery::Fresh) => {
                    ui.label("Slot recovery started a fresh layout.");
                }
                _ => {}
            }
            if !self.parent.connected() {
                self.binding_picker(ui, &snapshot);
            }
            #[cfg(windows)]
            if !self.parent.connected()
                && snapshot.can_pair_wezterm
                && ui
                    .add_enabled(self.ready(&snapshot), egui::Button::new("Pair WezTerm…"))
                    .clicked()
            {
                self.pairing.open(ui.ctx());
            }
        }
    }

    fn binding_picker(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Target terminal");
            let selected = self
                .binding
                .as_ref()
                .map_or_else(|| "Choose a paired terminal".to_owned(), binding_label);
            ui.add_enabled_ui(self.ready(snapshot), |ui| {
                egui::ComboBox::from_id_salt("terminal-choice")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for binding in &snapshot.data.bindings {
                            if snapshot.contains_binding(binding) {
                                ui.selectable_value(
                                    &mut self.binding,
                                    Some(binding.clone()),
                                    binding_label(binding),
                                );
                            }
                        }
                    });
            });
            if snapshot.data.bindings.is_empty() && snapshot.generation().is_some() {
                ui.weak("Pair an attached TUI with corvid companion pair.");
            }
        });
    }

    fn sections(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.section, Section::Slots, "Deck");
            ui.selectable_value(
                &mut self.section,
                Section::Overflow,
                format!("Queue ({})", snapshot.data.overflow_count),
            );
            ui.selectable_value(&mut self.section, Section::History, "Closed panes");
        });
        ui.add_space(12.0);
    }

    fn board(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        let (columns, edge) = deck::grid(ui.available_width());
        let width = columns as f32 * edge + (columns - 1) as f32 * deck::GAP;
        let inset = (ui.available_width() - width).max(0.0) / 2.0;
        for row in snapshot.data.slots.chunks(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = deck::GAP;
                ui.add_space(inset);
                for slot in row {
                    ui.push_id(slot.slot_id, |ui| self.card(ui, snapshot, slot, edge));
                }
            });
            ui.add_space(deck::GAP - ui.spacing().item_spacing.y);
        }
    }

    fn card(&mut self, ui: &mut Ui, snapshot: &Snapshot, slot: &PresentedSlot, edge: f32) {
        let desktop = self.parent.connected();
        let key = slot
            .run
            .as_ref()
            .and_then(|run| snapshot.live_key(run))
            .cloned();
        let focus = self
            .binding
            .as_ref()
            .and_then(|binding| snapshot.focus(slot, binding));
        let can_focus = self.ready(snapshot)
            && if desktop {
                key.is_some()
            } else {
                focus.is_some()
            };
        let cached = self.view.link != Link::Online || snapshot.generation().is_none();
        let response = ui.add(deck::Key {
            slot,
            edge,
            cached,
            can_focus,
        });
        if response.clicked() && can_focus {
            if desktop {
                self.focus_run(key.expect("focusable agent"));
            } else {
                self.submit(snapshot, focus.expect("focusable terminal binding"));
            }
        }
        let Some(run) = &slot.run else {
            return;
        };
        response.context_menu(|ui| {
            ui.strong(format!("Key {:02}", slot.slot_id));
            ui.separator();
            self.action(
                ui,
                snapshot,
                if slot.pinned { "Unpin" } else { "Pin" },
                snapshot.pin(slot),
            );
            if run.latest_unseen_attention_id.is_some() {
                self.action(ui, snapshot, "Acknowledge alert", snapshot.acknowledge(run));
            }
            if self.pending.is_some() {
                ui.close();
            }
        });
        response.on_hover_ui(|ui| {
            describe_run(ui, run, cached, None);
            ui.separator();
            ui.weak(if can_focus {
                "Click to open terminal · Right-click for actions"
            } else {
                "Focus unavailable · Right-click for actions"
            });
        });
    }

    fn overflow(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.page > 0 && self.pending.is_none(),
                    egui::Button::new("Previous"),
                )
                .clicked()
            {
                self.set_page(self.page - 1);
            }
            ui.label(format!(
                "Page {} of {}",
                self.page + 1,
                snapshot.page_count()
            ));
            if ui
                .add_enabled(
                    self.page + 1 < snapshot.page_count() && self.pending.is_none(),
                    egui::Button::new("Next"),
                )
                .clicked()
            {
                self.set_page(self.page + 1);
            }
        });
        if snapshot.page != self.page {
            ui.label("Loading page…");
            return;
        }
        if snapshot.data.overflow.is_empty() && snapshot.data.suppressed.is_empty() {
            ui.weak("No queued agents on this page.");
        }
        for (index, run) in snapshot.data.overflow.iter().enumerate() {
            ui.push_id(("overflow", index), |ui| {
                ui.separator();
                self.run(ui, snapshot, run);
                ui.horizontal_wrapped(|ui| {
                    if run.latest_unseen_attention_id.is_some() {
                        self.action(ui, snapshot, "Acknowledge alert", snapshot.acknowledge(run));
                    }
                });
            });
        }
        if !snapshot.data.suppressed.is_empty() {
            ui.add_space(12.0);
            ui.heading("Removed runs");
            for (index, run) in snapshot.data.suppressed.iter().enumerate() {
                ui.push_id(("removed", index), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(short_id(&run.agent_run_id));
                        self.action(ui, snapshot, "Re-add", snapshot.readd(run));
                    });
                });
            }
        }
    }

    fn action(&mut self, ui: &mut Ui, snapshot: &Snapshot, caption: &str, body: Option<Body>) {
        let enabled = body.is_some() && self.ready(snapshot);
        if ui
            .add_enabled(enabled, egui::Button::new(caption))
            .clicked()
            && let Some(body) = body
        {
            self.submit(snapshot, body);
        }
    }

    fn run(&mut self, ui: &mut Ui, snapshot: &Snapshot, run: &PresentedRun) {
        let key = snapshot.live_key(run);
        let focus = self
            .parent
            .connected()
            .then(|| key.is_some() && self.ready(snapshot));
        let cached = self.view.link != Link::Online || snapshot.generation().is_none();
        if describe_run(ui, run, cached, focus)
            && let Some(key) = key
        {
            self.focus_run(key.clone());
        }
    }

    fn hosts(&mut self, ui: &mut Ui) {
        let (state, current) = &self.catalog_scope;
        if ui.button("Refresh hosts").clicked() {
            self.catalog = None;
        }
        let catalog = self.catalog.get_or_insert_with(|| {
            Catalog::start(state.clone(), *current, ui.ctx().clone())
                .map_err(|error| error.to_string())
        });
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(error) => {
                ui.label(RichText::new(error.as_str()).color(ui.visuals().error_fg_color));
                return;
            }
        };
        let view = catalog.views.borrow().clone();
        if let Some(message) = view.message {
            ui.label(message);
        }
        for (id, host) in view.hosts {
            ui.push_id(id, |ui| {
                ui.group(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(&host.label);
                        ui.monospace(short_id(&id.to_string()));
                        ui.label(if host.online {
                            "Live"
                        } else if host.runs.is_empty() {
                            "Offline"
                        } else {
                            "Cached"
                        });
                        if id == *current {
                            if ui.button("This Deck's slots").clicked() {
                                self.section = Section::Slots;
                            }
                        } else if ui.button("Show host Deck").clicked() {
                            catalog.open(id);
                        }
                    });
                    if host.online && host.runs.is_empty() {
                        ui.weak("No observed agents.");
                    }
                    for run in &host.runs {
                        ui.horizontal_wrapped(|ui| {
                            let provider = match AgentProvider::try_from(run.provider) {
                                Ok(AgentProvider::Claude) => "Claude",
                                Ok(AgentProvider::Codex) => "Codex",
                                _ => "Agent",
                            };
                            ui.label(provider);
                            ui.monospace(short_id(&run.agent_run_id));
                            let (badge, color) = badge_style(run.badge, ui.visuals());
                            ui.label(RichText::new(badge).color(color));
                            if run.pending_requests > 0 {
                                ui.label(format!("{} pending", run.pending_requests));
                            }
                        });
                    }
                    if host.omitted > 0 {
                        ui.weak(format!("{} older runs omitted", host.omitted));
                    }
                });
            });
        }
    }

    fn focus_run(&mut self, key: PresentedRunKey) {
        let id = Uuid::new_v4();
        if self.parent.focus(id, key) {
            self.pending = Some(id);
            self.pending_since = Some(Instant::now());
            self.notice = None;
        } else {
            self.notice = Some(Completion {
                id,
                message: "Could not send the action to Desktop.".into(),
                failed: true,
                paired: None,
            });
        }
    }

    fn notifications(&mut self, ui: &mut Ui) {
        let background = self.background.view.borrow().clone();
        let mut settings = background.settings;
        let mut changed = ui
            .checkbox(&mut settings.enabled, "Show OS notifications")
            .changed();
        ui.horizontal_wrapped(|ui| {
            changed |= ui
                .checkbox(&mut settings.input, "Questions and approvals")
                .changed();
            changed |= ui
                .checkbox(&mut settings.responses, "New responses")
                .changed();
            changed |= ui.checkbox(&mut settings.failures, "Errors").changed();
            changed |= ui.checkbox(&mut settings.sound, "Sound").changed();
        });
        if background.background {
            changed |= ui
                .checkbox(&mut settings.start_at_login, "Start Agent Deck at login")
                .changed();
            ui.weak("Closing this window keeps Agent Deck in the system tray.");
        } else {
            ui.weak("Keep this window open to receive notifications. A system tray backend is unavailable.");
        }
        let paused = settings.paused_until_ms > now_ms();
        if ui
            .button(if paused {
                "Resume notifications"
            } else {
                "Pause for 1 hour"
            })
            .clicked()
        {
            settings.paused_until_ms = if paused {
                0
            } else {
                now_ms().saturating_add(60 * 60 * 1000)
            };
            changed = true;
        }
        if changed {
            self.background.configure(settings);
        }
        if !background.status.is_empty() {
            ui.label(RichText::new(background.status).color(ui.visuals().warn_fg_color));
        }
        if ui.button("Send test notification").clicked() {
            self.background.test_notification();
        }
        if ui.button("Quit Corvid Agent Deck").clicked() {
            self.quitting = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ui.separator();
        if let Some(notice) = self.selected_notice.clone() {
            ui.heading(&notice.title);
            ui.label(&notice.body);
            let current = self.view.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.host == notice.key.host
                    && snapshot.data.remote_epoch.as_deref()
                        == Some(notice.key.epoch.to_string().as_str())
                    && snapshot.generation().is_some()
            });
            if ui
                .add_enabled(
                    current && self.parent.connected() && self.pending.is_none(),
                    egui::Button::new("Open existing terminal in Desktop"),
                )
                .clicked()
            {
                self.focus_run(notice.key.run());
            }
            ui.weak("The current Desktop and daemon revalidate the exact agent. Questions stay pending until answered in the terminal.");
            ui.separator();
        }
        ui.heading("Recent notifications");
        if background.history.is_empty() {
            ui.weak("New agent notifications will appear here.");
        }
        for notice in background.history {
            ui.push_id(notice.key.attention, |ui| {
                if ui
                    .selectable_label(
                        self.selected_notice
                            .as_ref()
                            .is_some_and(|selected| selected.key == notice.key),
                        &notice.title,
                    )
                    .clicked()
                {
                    self.selected_notice = Some(notice.clone());
                }
                ui.weak(&notice.body);
            });
        }
    }
}

impl eframe::App for Desktop {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let background = self.background.view.borrow().clone();
        if ctx.input(|input| input.viewport().close_requested())
            && background.background
            && !background.quit
            && !self.quitting
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Widgets process a release against the previous frame. Keep the same
        // slot and binding visible until the entire gesture has been processed.
        if !gesture_active(ctx) {
            self.refresh_view();
        }
        ctx.request_repaint_after(Duration::from_secs(1));

        let panel = Frame::side_top_panel(&ctx.style()).inner_margin(Margin::symmetric(20, 12));
        egui::TopBottomPanel::top("connection")
            .frame(panel)
            .show(ctx, |ui| self.header(ui));
        egui::TopBottomPanel::bottom("action-result")
            .frame(panel)
            .show(ctx, |ui| {
                if self.pending.is_some() {
                    ui.label("Waiting for the action…");
                } else if let Some(notice) = &self.notice {
                    let mut text = RichText::new(&notice.message);
                    if notice.failed {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    ui.label(text);
                } else if self.section == Section::Hosts {
                    ui.weak("Each host keeps its own slots and notifications.");
                } else if self.section == Section::Slots
                    && self.view.snapshot.as_ref().is_some_and(|snapshot| {
                        snapshot.data.slots.iter().all(|slot| slot.run.is_none())
                    })
                {
                    ui.weak("Run Codex or Claude in a Corvid terminal to fill the deck.");
                } else if self.parent.connected() {
                    ui.weak("Click a key to open its terminal · Right-click for pin and alerts");
                } else {
                    ui.weak("Select a paired terminal to focus a slot.");
                }
            });
        let content = Frame::central_panel(&ctx.style())
            .fill(deck::BACKGROUND)
            .inner_margin(Margin::symmetric(20, 16));
        egui::CentralPanel::default()
            .frame(content)
            .show(ctx, |ui| {
                if self.section == Section::Hosts {
                    egui::ScrollArea::vertical().show(ui, |ui| self.hosts(ui));
                    return;
                }
                self.catalog = None;
                if self.section == Section::Notifications {
                    egui::ScrollArea::vertical().show(ui, |ui| self.notifications(ui));
                    return;
                }
                let Some(snapshot) = self.view.snapshot.clone() else {
                    ui.label("Start corvid companion run to load slots.");
                    return;
                };
                self.sections(ui, &snapshot);
                egui::ScrollArea::vertical().show(ui, |ui| match self.section {
                    Section::Slots => self.board(ui, &snapshot),
                    Section::Overflow => self.overflow(ui, &snapshot),
                    Section::History => ended(ui, &snapshot),
                    Section::Notifications => self.notifications(ui),
                    Section::Hosts => self.hosts(ui),
                });
            });
        #[cfg(windows)]
        if let Some(snapshot) = self.view.snapshot.clone()
            && let Some(generation) = snapshot.generation()
            && self.ready(&snapshot)
            && let Some(body) = self.pairing.show(ctx, snapshot.host, generation)
        {
            self.submit(&snapshot, body);
        }
    }
}

fn gesture_active(ctx: &egui::Context) -> bool {
    egui::Popup::is_any_open(ctx)
        || ctx.input(|input| {
            input.pointer.any_down()
                || input.pointer.any_released()
                || [egui::Key::Enter, egui::Key::Space].into_iter().any(|key| {
                    input.key_down(key) || input.key_pressed(key) || input.key_released(key)
                })
                || input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::AccessKitActionRequest(_)))
        })
}

fn ended(ui: &mut Ui, snapshot: &Snapshot) {
    if snapshot.data.recent_ended.is_empty() {
        ui.weak("No agents removed by pane closure yet.");
    }
    for run in &snapshot.data.recent_ended {
        ui.horizontal_wrapped(|ui| {
            ui.label("Ended");
            ui.monospace(short_id(&run.agent_run_id));
        });
    }
}

fn describe_run(ui: &mut Ui, run: &PresentedRun, cached: bool, focus: Option<bool>) -> bool {
    let provider = match AgentProvider::try_from(run.provider) {
        Ok(AgentProvider::Claude) => "Claude Code",
        Ok(AgentProvider::Codex) => "Codex CLI",
        _ => "Unknown provider",
    };
    let clicked = ui
        .horizontal_wrapped(|ui| {
            let clicked = if let Some(enabled) = focus {
                ui.add_enabled(enabled, egui::Button::new(RichText::new(provider).strong()))
                    .on_hover_text("Open pane in Corvid Desktop")
                    .clicked()
            } else {
                ui.strong(provider);
                false
            };
            if let Some(key) = &run.key {
                ui.monospace(short_id(&key.agent_run_id));
            }
            clicked
        })
        .inner;

    let availability = RunAvailability::try_from(run.availability).ok();
    let last_known = cached || availability != Some(RunAvailability::Available);
    let (badge, color) = badge_style(run.last_badge, ui.visuals());
    ui.horizontal_wrapped(|ui| {
        if last_known {
            ui.weak(format!("Last known: {badge}"));
        } else {
            ui.label(RichText::new(badge).strong().color(color));
        }
        if let Some(label) = availability_label(availability, cached) {
            ui.weak(label);
        }
    });
    if last_known && let Some(age) = run.last_observed_unix_ms.and_then(observation_age) {
        ui.weak(format!("Observed {age} ago"));
    }
    ui.horizontal_wrapped(|ui| {
        ui.weak(evidence_label(run));
        if run.pending_requests > 0 {
            ui.label(format!("{} pending", run.pending_requests));
        }
        if run.unverified_requests > 0 {
            ui.label(format!("{} unverified", run.unverified_requests));
        }
        if run.unseen_attention > 0 {
            ui.label(format!("{} unseen", run.unseen_attention));
        }
    });
    clicked
}

fn observation_age(observed_unix_ms: u64) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let seconds = now
        .checked_sub(Duration::from_millis(observed_unix_ms))?
        .as_secs();
    let (scale, unit) = [(86_400, "d"), (3600, "h"), (60, "m"), (1, "s")]
        .into_iter()
        .find(|&(scale, _)| seconds >= scale)
        .unwrap_or((1, "s"));
    Some(format!("{}{unit}", seconds / scale))
}

fn badge_style(raw: i32, visuals: &egui::Visuals) -> (&'static str, Color32) {
    let normal = visuals.text_color();
    match AgentBadge::try_from(raw) {
        Ok(AgentBadge::Working) => ("Working", visuals.hyperlink_color),
        Ok(AgentBadge::NeedsInput) => ("Needs input", visuals.warn_fg_color),
        Ok(AgentBadge::ResponseObserved) => ("Response observed", normal),
        Ok(AgentBadge::Ready) => ("Ready", normal),
        Ok(AgentBadge::Interrupted) => ("Interrupted", visuals.warn_fg_color),
        Ok(AgentBadge::Error) => ("Error", visuals.error_fg_color),
        Ok(AgentBadge::Ended) => ("Ended", normal),
        _ => ("Unknown", normal),
    }
}

fn availability_label(availability: Option<RunAvailability>, cached: bool) -> Option<&'static str> {
    match availability {
        Some(RunAvailability::Available) => cached.then_some("Cached"),
        Some(RunAvailability::Historical) => Some("Historical"),
        Some(RunAvailability::Offline) => Some("Offline"),
        _ => Some("Check needed"),
    }
}

fn evidence_label(run: &PresentedRun) -> &'static str {
    match AgentCollection::try_from(run.collection) {
        Ok(AgentCollection::Healthy) => match EvidenceConfidence::try_from(run.confidence) {
            Ok(EvidenceConfidence::Confirmed) => "Confirmed evidence",
            Ok(EvidenceConfidence::Inferred) => "Inferred evidence",
            _ => "Uncertain evidence",
        },
        Ok(AgentCollection::GapDetected) => "Collection gap",
        Ok(AgentCollection::Degraded) => "Collection degraded",
        Ok(AgentCollection::Unsupported) => "Unsupported collection",
        _ => "Collection unavailable",
    }
}

fn connection_text(display: &Display) -> &'static str {
    match display.link {
        Link::Connecting => "Connecting to companion",
        Link::Unavailable => "Companion offline",
        Link::Incompatible => "Incompatible companion protocol",
        Link::Online => match display
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.data.connection)
        {
            Some(value) => match CompanionConnection::try_from(value) {
                Ok(CompanionConnection::Ready)
                    if display
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.generation())
                        .is_some() =>
                {
                    "Daemon ready"
                }
                Ok(CompanionConnection::Connecting) => "Connecting to daemon",
                Ok(CompanionConnection::Syncing) => "Synchronizing daemon",
                Ok(CompanionConnection::Offline) => "Daemon offline",
                Ok(CompanionConnection::Incompatible) => "Incompatible daemon",
                Ok(CompanionConnection::Error) => "Daemon connection error",
                _ => "Daemon state unknown",
            },
            None => "Waiting for slots",
        },
    }
}

fn binding_label(binding: &PresentedBinding) -> String {
    let client = Uuid::parse_str(&binding.client_id)
        .map_or_else(|_| "unknown client".to_owned(), |id| id.to_string());
    match &binding.wezterm {
        Some(location) => format!(
            "{} · WezTerm {} / window {} / pane {}",
            client, location.gui_pid, location.window_id, location.pane_id
        ),
        None => format!("{client} · Manual switch"),
    }
}

fn short_id(id: &str) -> String {
    Uuid::parse_str(id)
        .map(|id| id.as_hyphenated().to_string()[..8].to_owned())
        .unwrap_or_else(|_| "unknown ID".to_owned())
}

fn safe_code(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defers_snapshots_on_pointer_press_and_release() {
        let ctx = egui::Context::default();
        for button in [egui::PointerButton::Primary, egui::PointerButton::Secondary] {
            for pressed in [true, false] {
                let input = egui::RawInput {
                    events: vec![egui::Event::PointerButton {
                        pos: egui::pos2(10.0, 10.0),
                        button,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| assert!(gesture_active(ctx)));
            }
        }
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(!gesture_active(ctx))
        });
    }

    #[test]
    fn defers_snapshots_while_a_key_menu_is_open() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::Popup::open_id(ctx, egui::Id::new("key-menu"));
            assert!(gesture_active(ctx));
            egui::Popup::close_all(ctx);
            assert!(!gesture_active(ctx));
        });
    }
}
