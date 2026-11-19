use crate::app::Session;
use crate::catalog::Catalog;
use crate::ui_signal::Presentation;
use corvid_agent_deck_core::notifications::{Notice, now_ms};
use corvid_agent_deck_core::{
    actions::{Actions, FocusAction},
    gesture::{Gesture, Input},
};
use corvid_companion::RunKey;
use corvid_presentation::ipc::{Display, Link};
use corvid_presentation::model::Snapshot;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentProvider, CompanionConnection, EvidenceConfidence,
    PresentedBinding, PresentedRun, PresentedRunKey, PresentedSlot, PromptObservation,
    RunAvailability,
};
use gpui::{
    App, Context, Div, ElementId, FocusHandle, FontWeight, KeyDownEvent, KeyUpEvent, MouseButton,
    Render, SharedString, Stateful, Window, div, prelude::*, px, rgba,
};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod chrome;
mod colors;
mod deck;
mod floating;
#[cfg(windows)]
#[path = "pairing.rs"]
mod pairing;
pub(crate) use chrome::DeckAssets;
use colors::Colors;
use deck::{column, row};

pub(crate) struct DeckView {
    session: Rc<Session>,
    focus: FocusHandle,
    colors: Colors,
    held: Gesture,
    selected: usize,
    menu: Option<(std::sync::Arc<Snapshot>, PresentedSlot)>,
    view: Display,
    binding: Option<PresentedBinding>,
    section: Section,
    page: u32,
    actions: Actions,
    notifications_seen: u64,
    selected_notice: Option<Notice>,
    catalog: Option<Result<Catalog, String>>,
    catalog_view: crate::catalog::View,
    deck_order: Vec<RunKey>,
    queue_waiting: usize,
    window: crate::shell::Window,
    window_ready: bool,
    presentation: Option<Presentation>,
    #[cfg(windows)]
    pairing: pairing::Pairing,
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

impl DeckView {
    pub(crate) fn new(
        session: Rc<Session>,
        preferences: crate::shell::Window,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |view, _| {
                if view
                    .session
                    .background
                    .view
                    .borrow()
                    .capabilities
                    .status_icon
                    .ready()
                {
                    view.window.hide(window);
                    false
                } else {
                    true
                }
            })
            .unwrap_or(true)
        });
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.held.cancel();
            }
            view.update(window, cx);
        });
        activation.detach();
        #[cfg(target_os = "macos")]
        cx.observe_window_appearance(window, |_, _, cx| cx.notify())
            .detach();
        let mut result = Self {
            session,
            colors: Colors::for_window(window),
            view: Display::default(),
            binding: None,
            section: Section::Slots,
            page: 0,
            actions: Actions::default(),
            notifications_seen: 0,
            selected_notice: None,
            catalog: None,
            catalog_view: Default::default(),
            deck_order: Vec::new(),
            queue_waiting: 0,
            window: preferences,
            window_ready: !cfg!(target_os = "linux"),
            presentation: None,
            focus,
            held: Gesture::default(),
            selected: 0,
            menu: None,
            #[cfg(windows)]
            pairing: pairing::Pairing::default(),
        };
        result.refresh_view();
        // Let X11 present its initial frame before the window manager changes
        // decorations. Changing them during mapping can suppress that frame.
        #[cfg(target_os = "linux")]
        {
            let view = cx.weak_entity();
            window.on_next_frame(move |window, cx| {
                let _ = view.update(cx, |view, cx| {
                    view.window_ready = true;
                    view.update(window, cx);
                });
            });
        }
        #[cfg(not(target_os = "linux"))]
        cx.defer_in(window, |view, window, cx| view.update(window, cx));
        result
    }

    fn refresh_view(&mut self) {
        let (state, host) = &self.session.catalog;
        if let Ok(catalog) = self.catalog.get_or_insert_with(|| {
            Catalog::start(
                state.clone(),
                *host,
                self.session.signal.clone(),
                self.session.client.display.clone(),
                self.session.background.desktop.clone(),
            )
            .map_err(|error| error.to_string())
        }) {
            self.catalog_view = catalog.views.borrow().clone();
            if let Ok(completion) = catalog.completed.try_recv() {
                self.actions.complete(completion);
            }
        } else {
            self.catalog_view = Default::default();
        }
        let next = self.session.client.display.borrow().clone();
        if let Some(completion) = &next.completion {
            self.actions.complete(completion.clone());
        }
        if let Some(completion) = self.session.background.try_completion() {
            self.actions.complete(completion);
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
        self.refresh_deck_order();
        let background = self.session.background.view.borrow().clone();
        if background.notifications_open != self.notifications_seen {
            self.notifications_seen = background.notifications_open;
            self.selected_notice = background.selected;
            self.section = Section::Notifications;
            self.window.expanded = true;
        }
        self.actions.expire();
        if self.page >= self.queue_pages() {
            self.set_page(self.queue_pages() - 1);
        }
    }

    fn submit(&mut self, snapshot: &Snapshot, body: Body) {
        self.actions
            .submit(&self.session.client, self.page, snapshot, body);
    }

    fn set_page(&mut self, page: u32) {
        self.page = page;
        self.session.client.page.send_replace(page);
    }

    fn ready(&self, snapshot: &Snapshot) -> bool {
        !self.actions.is_pending()
            && self.view.link == Link::Online
            && snapshot.page == self.page
            && snapshot.generation().is_some()
    }

    fn focus_run(&mut self, key: PresentedRunKey) {
        let Some(snapshot) = self.view.snapshot.clone() else {
            return;
        };
        self.actions.focus(key, |id, run| {
            matches!(&self.catalog, Some(Ok(catalog)) if catalog.focus_current(id, snapshot, run))
        });
    }

    pub(crate) fn refresh(
        &mut self,
        presentation: Option<Presentation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(presentation) = presentation {
            self.presentation = Some(presentation);
        }
        if presentation == Some(Presentation::Open) && !cfg!(target_os = "macos") {
            self.window.expanded = false;
            self.section = Section::Slots;
            self.held.cancel();
            self.menu = None;
            self.set_page(0);
        }
        self.update(window, cx);
    }

    fn update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.held.active() && self.menu.is_none() {
            self.refresh_view();
        }
        #[cfg(windows)]
        self.pairing.poll();
        self.refresh_window(window, cx);
        cx.notify();
    }

    fn refresh_window(&mut self, window: &mut Window, cx: &App) {
        if !self.window_ready {
            return;
        }
        let tray = self
            .session
            .background
            .view
            .borrow()
            .capabilities
            .can_start_hidden();
        let compact_footer = self.show_compact_footer();
        self.window
            .update(window, tray, self.presentation.take(), compact_footer, cx);
    }

    fn activate(&mut self, action: FocusAction) {
        if self.actions.is_pending() {
            return;
        }
        match action {
            FocusAction::Desktop(key) => self.focus_run(key),
            FocusAction::HostDesktop { controller, run } => {
                self.actions.focus(run, |id, run| {
                    matches!(&self.catalog, Some(Ok(catalog)) if catalog.focus(id, controller, run))
                });
            }
            FocusAction::Companion(snapshot, body) => {
                self.submit(&snapshot, *body);
            }
        }
    }

    fn expand(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_view(true, section, window, cx);
    }

    fn switch_view(
        &mut self,
        expanded: bool,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.held.cancel();
        self.menu = None;
        self.section = section;
        self.window.expanded = expanded;
        self.set_page(0);
        self.update(window, cx);
    }
}

impl Render for DeckView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.colors = Colors::for_window(window);
        let content = if self.window.expanded {
            self.expanded(window, cx)
        } else {
            self.floating(cx)
        };
        column()
            .id("agent-deck")
            .size_full()
            .relative()
            .bg(self.colors.background)
            .text_color(self.colors.text)
            .text_size(px(13.))
            .line_height(gpui::relative(1.2))
            .when(!cfg!(target_os = "macos"), |view| {
                view.font_family("Segoe UI")
            })
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    view.held.cancel();
                    #[cfg(windows)]
                    if view.pairing.is_open() {
                        view.pairing.close();
                        cx.notify();
                        return;
                    }
                    let menu_closed = view.menu.take().is_some();
                    if !menu_closed && cfg!(target_os = "macos") {
                        view.window.hide(window);
                    }
                    cx.notify();
                    return;
                }
                #[cfg(windows)]
                if view.pairing.is_open() {
                    view.pairing.key(event, cx);
                    return;
                }
                let actions = view.keyboard_actions();
                if actions.is_empty() || view.menu.is_some() {
                    return;
                }
                view.selected = view.selected.min(actions.len() - 1);
                match event.keystroke.key.as_str() {
                    "left" | "up" => {
                        view.held.cancel();
                        view.selected = view.selected.saturating_sub(1);
                    }
                    "right" | "down" | "tab" => {
                        view.held.cancel();
                        view.selected = (view.selected + 1) % actions.len();
                    }
                    "enter" | "space" => {
                        if let Some(Some(action)) = actions.get(view.selected) {
                            view.held
                                .press(Input::Keyboard, view.selected, action.clone());
                        }
                    }
                    _ => {}
                }
                cx.notify();
            }))
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && let Some(action) = view.held.release(Input::Keyboard, view.selected)
                {
                    view.activate(action);
                    view.refresh_window(window, cx);
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.held.cancel();
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.held.cancel();
                    cx.notify();
                }),
            )
            .child(content)
            .children(self.menu_view(cx))
    }
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

fn availability_label(availability: Option<RunAvailability>, cached: bool) -> Option<&'static str> {
    match availability {
        Some(RunAvailability::Available) => cached.then_some("Cached"),
        Some(RunAvailability::Historical) => Some("Historical"),
        Some(RunAvailability::Offline) => Some("Offline"),
        _ => Some("Check needed"),
    }
}

fn prompt_detail(run: &PresentedRun) -> Option<PromptObservation> {
    if run.last_badge == AgentBadge::Unknown as i32
        && run.collection == AgentCollection::Healthy as i32
        && run.pending_requests == 0
        && run.unverified_requests == 0
    {
        PromptObservation::try_from(run.prompt_observation).ok()
    } else {
        None
    }
}

fn evidence_label(run: &PresentedRun) -> &'static str {
    match prompt_detail(run) {
        Some(PromptObservation::Submitted) => return "Prompt submission observed",
        Some(PromptObservation::Responded) => return "Response observed for this prompt",
        _ => {}
    }
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

fn connection_text(display: &Display) -> Option<&'static str> {
    Some(match display.link {
        Link::Connecting => "Connecting to agents…",
        Link::Unavailable => "Agent monitor unavailable",
        Link::Incompatible => "Update Corvid to reconnect",
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
                    return None;
                }
                Ok(CompanionConnection::Connecting) => "Connecting to host…",
                Ok(CompanionConnection::Syncing) => "Synchronizing agents…",
                Ok(CompanionConnection::Offline) => "Host disconnected",
                Ok(CompanionConnection::Incompatible) => "Update Corvid on the host to reconnect",
                Ok(CompanionConnection::Error) => "Could not connect to host",
                _ => "Waiting for agent status…",
            },
            None => "Loading agents…",
        },
    })
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
