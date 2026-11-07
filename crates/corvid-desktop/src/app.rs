use std::time::Duration;

use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentProvider, CompanionConnection, EvidenceConfidence,
    LayoutRecovery, PresentedBinding, PresentedRun, PresentedRunKey, PresentedSlot,
    RunAvailability,
};
use eframe::egui::{self, Color32, FontId, Frame, Margin, RichText, TextStyle, Ui};
use uuid::Uuid;

use crate::ipc::{Command, Completion, DesktopClient, Display, Link};
use crate::model::Snapshot;

pub(crate) fn configure(ctx: &egui::Context) {
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
    view: Display,
    binding: Option<PresentedBinding>,
    placement: Option<PresentedRunKey>,
    section: Section,
    page: u32,
    pending: Option<Uuid>,
    notice: Option<Completion>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    #[default]
    Slots,
    Overflow,
    History,
}

impl Desktop {
    pub(crate) fn new(client: DesktopClient) -> Self {
        Self {
            client,
            view: Display::default(),
            binding: None,
            placement: None,
            section: Section::Slots,
            page: 0,
            pending: None,
            notice: None,
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
        match (&self.view.snapshot, &next.snapshot) {
            (Some(previous), Some(current))
                if next.link == Link::Online
                    && previous.link == current.link
                    && previous.generation() == current.generation() =>
            {
                self.binding
                    .take_if(|binding| !current.contains_binding(binding));
                self.placement.take_if(|key| !current.can_place(key));
            }
            _ => {
                self.binding = None;
                self.placement = None;
            }
        }
        self.view = next;
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
            });
            return;
        }
        if self.client.commands.try_send(command).is_ok() {
            self.pending = Some(id);
            self.notice = None;
        } else {
            self.notice = Some(Completion {
                id,
                message: "Could not send the action. Wait for the companion and try again."
                    .to_owned(),
                failed: true,
            });
        }
    }

    fn set_page(&mut self, page: u32) {
        self.page = page;
        self.placement = None;
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
            ui.heading("Corvid");
            let text = connection_text(&self.view);
            ui.label(RichText::new(text).strong());
        });
        if let Some(snapshot) = self.view.snapshot.clone() {
            ui.label(format!("Host {}", snapshot.host));
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
            self.binding_picker(ui, &snapshot);
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
            ui.selectable_value(&mut self.section, Section::Slots, "Slots");
            ui.selectable_value(
                &mut self.section,
                Section::Overflow,
                format!("Overflow ({})", snapshot.data.overflow_count),
            );
            ui.selectable_value(&mut self.section, Section::History, "Recent ended");
        });
        ui.separator();
    }

    fn board(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        if let Some(key) = &self.placement {
            let id = short_id(&key.agent_run_id);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Choose an empty slot for {id}"));
                if ui.button("Cancel placement").clicked() {
                    self.placement = None;
                }
            });
        }
        let columns = ((ui.available_width() / 250.0) as usize).clamp(1, 4);
        for row in snapshot.data.slots.chunks(columns) {
            ui.columns(columns, |cells| {
                for (ui, slot) in cells.iter_mut().zip(row) {
                    ui.push_id(slot.slot_id, |ui| self.card(ui, snapshot, slot));
                }
            });
            ui.add_space(10.0);
        }
    }

    fn card(&mut self, ui: &mut Ui, snapshot: &Snapshot, slot: &PresentedSlot) {
        Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height(116.0);
            ui.horizontal_wrapped(|ui| {
                ui.strong(format!("Slot {:02}", slot.slot_id));
                if slot.pinned {
                    ui.weak("Pinned");
                }
            });

            if let Some(run) = &slot.run {
                describe_run(
                    ui,
                    run,
                    self.view.link != Link::Online || snapshot.generation().is_none(),
                );
                ui.horizontal_wrapped(|ui| {
                    let focus = self
                        .binding
                        .as_ref()
                        .and_then(|binding| snapshot.focus(slot, binding));
                    self.action(ui, snapshot, "Focus", focus);
                    if run.latest_unseen_attention_id.is_some() {
                        self.action(ui, snapshot, "Acknowledge alert", snapshot.acknowledge(run));
                    }
                });
            } else {
                ui.weak(if slot.pinned { "Reserved" } else { "Empty" });
                if let Some(body) = self
                    .placement
                    .as_ref()
                    .and_then(|key| snapshot.place(slot, key))
                {
                    self.action(ui, snapshot, "Place chosen run", Some(body));
                }
            }

            ui.horizontal_wrapped(|ui| {
                let pin = if slot.pinned { "Unpin" } else { "Pin" };
                self.action(ui, snapshot, pin, snapshot.pin(slot));
                if slot.run.is_some() {
                    self.action(ui, snapshot, "Remove", snapshot.remove(slot));
                }
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
            ui.weak("No overflow or removed runs on this page.");
        }
        for (index, run) in snapshot.data.overflow.iter().enumerate() {
            ui.push_id(("overflow", index), |ui| {
                ui.separator();
                describe_run(
                    ui,
                    run,
                    self.view.link != Link::Online || snapshot.generation().is_none(),
                );
                ui.horizontal_wrapped(|ui| {
                    if let Some(key) = run.key.as_ref().filter(|key| snapshot.can_place(key))
                        && ui
                            .add_enabled(
                                self.ready(snapshot),
                                egui::Button::new("Choose for placement"),
                            )
                            .clicked()
                    {
                        self.placement = Some(key.clone());
                        self.section = Section::Slots;
                    }
                    if run.latest_unseen_attention_id.is_some() {
                        self.action(ui, snapshot, "Acknowledge alert", snapshot.acknowledge(run));
                    }
                    self.action(ui, snapshot, "Remove", snapshot.remove_overflow(run));
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
}

impl eframe::App for Desktop {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Widgets process a release against the previous frame. Keep the same
        // slot and binding visible until the entire gesture has been processed.
        if !gesture_active(ctx) {
            self.refresh_view();
        }
        ctx.request_repaint_after(Duration::from_secs(1));

        let panel = Frame::side_top_panel(&ctx.style()).inner_margin(Margin::symmetric(16, 10));
        egui::TopBottomPanel::top("connection")
            .frame(panel)
            .show(ctx, |ui| self.header(ui));
        egui::TopBottomPanel::bottom("action-result")
            .frame(panel)
            .show(ctx, |ui| {
                if self.pending.is_some() {
                    ui.label("Waiting for the companion…");
                } else if let Some(notice) = &self.notice {
                    let mut text = RichText::new(&notice.message);
                    if notice.failed {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    ui.label(text);
                } else {
                    ui.weak("Select a paired terminal to focus a slot.");
                }
            });
        let content = Frame::central_panel(&ctx.style()).inner_margin(Margin::symmetric(16, 10));
        egui::CentralPanel::default()
            .frame(content)
            .show(ctx, |ui| {
                let Some(snapshot) = self.view.snapshot.clone() else {
                    ui.label("Start corvid companion run to load slots.");
                    return;
                };
                self.sections(ui, &snapshot);
                egui::ScrollArea::vertical().show(ui, |ui| match self.section {
                    Section::Slots => self.board(ui, &snapshot),
                    Section::Overflow => self.overflow(ui, &snapshot),
                    Section::History => ended(ui, &snapshot),
                });
            });
    }
}

fn gesture_active(ctx: &egui::Context) -> bool {
    ctx.input(|input| {
        input.pointer.primary_down()
            || input.pointer.primary_released()
            || [egui::Key::Enter, egui::Key::Space]
                .into_iter()
                .any(|key| input.key_down(key) || input.key_pressed(key))
            || input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::AccessKitActionRequest(_)))
    })
}

fn ended(ui: &mut Ui, snapshot: &Snapshot) {
    if snapshot.data.recent_ended.is_empty() {
        ui.weak("No recent ended runs.");
    }
    for run in &snapshot.data.recent_ended {
        ui.horizontal_wrapped(|ui| {
            ui.label("Ended");
            ui.monospace(short_id(&run.agent_run_id));
        });
    }
}

fn describe_run(ui: &mut Ui, run: &PresentedRun, cached: bool) {
    let provider = match AgentProvider::try_from(run.provider) {
        Ok(AgentProvider::Claude) => "Claude Code",
        Ok(AgentProvider::Codex) => "Codex CLI",
        _ => "Unknown provider",
    };
    ui.horizontal_wrapped(|ui| {
        ui.strong(provider);
        if let Some(key) = &run.key {
            ui.monospace(short_id(&key.agent_run_id));
        }
    });

    let availability = RunAvailability::try_from(run.availability).ok();
    let (badge, color) = badge_style(run.last_badge, ui.visuals());
    ui.horizontal_wrapped(|ui| {
        if cached || availability != Some(RunAvailability::Available) {
            ui.weak(format!("Last known: {badge}"));
        } else {
            ui.label(RichText::new(badge).strong().color(color));
        }
        if let Some(label) = availability_label(availability, cached) {
            ui.weak(label);
        }
    });
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
                    "Remote ready"
                }
                Ok(CompanionConnection::Connecting) => "Connecting to remote",
                Ok(CompanionConnection::Syncing) => "Synchronizing remote",
                Ok(CompanionConnection::Offline) => "Remote offline",
                Ok(CompanionConnection::Incompatible) => "Incompatible remote",
                Ok(CompanionConnection::Error) => "Remote connection error",
                _ => "Remote state unknown",
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
        for pressed in [true, false] {
            let input = egui::RawInput {
                events: vec![egui::Event::PointerButton {
                    pos: egui::pos2(10.0, 10.0),
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| assert!(gesture_active(ctx)));
        }
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(!gesture_active(ctx))
        });
    }
}
