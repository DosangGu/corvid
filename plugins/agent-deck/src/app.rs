use crate::background::Background;
use crate::catalog::Catalog;
use crate::notifications::{Notice, now_ms};
use crate::parent::DesktopLink;
use crate::ui_signal::UiSignal;
use corvid_presentation::ipc::{Command, Completion, DesktopClient, Display, Link};
use corvid_presentation::model::Snapshot;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentProvider, CompanionConnection, EvidenceConfidence,
    PresentedBinding, PresentedRun, PresentedRunKey, PresentedSlot, RunAvailability,
};
use gpui::{
    Context, Div, ElementId, FocusHandle, FontWeight, KeyDownEvent, KeyUpEvent, MouseButton,
    Render, SharedString, Stateful, Task, Window, div, prelude::*, px, rgb, rgba,
};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod deck;
mod floating;
mod gesture;
use gesture::{Gesture, Input};
#[cfg(windows)]
#[path = "pairing.rs"]
mod pairing;
use deck::{BACKGROUND, MINT, PANEL, TEXT, button, column, muted, row};

#[derive(Clone)]
enum FocusAction {
    Desktop(PresentedRunKey),
    Companion(std::sync::Arc<Snapshot>, Body),
}

pub(crate) struct Desktop {
    client: DesktopClient,
    signal: UiSignal,
    focus: FocusHandle,
    held: Gesture,
    selected: usize,
    menu: Option<(std::sync::Arc<Snapshot>, PresentedSlot)>,
    _tasks: Vec<Task<()>>,
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
    catalog_scope: (PathBuf, Uuid),
    catalog: Option<Result<Catalog, String>>,
    window: crate::window::Window,
    window_ready: bool,
    focus_pending: Option<Uuid>,
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

impl Desktop {
    pub(crate) fn new(
        client: DesktopClient,
        background: Background,
        catalog_scope: (PathBuf, Uuid),
        preferences: crate::window::Window,
        signal: UiSignal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut updates = signal.subscribe();
        let event_task = cx.spawn_in(window, async move |view, cx| {
            while updates.changed().await.is_ok() {
                if view
                    .update_in(cx, |view, window, cx| view.update(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let clock = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if view
                    .update_in(cx, |view, window, cx| view.update(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |view, _| {
                if view.background.view.borrow().background {
                    view.window.hide(window);
                    false
                } else {
                    true
                }
            })
            .unwrap_or(true)
        });
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            view.held.cancel();
            view.update(window, cx);
        });
        activation.detach();
        let mut result = Self {
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
            catalog_scope,
            catalog: None,
            window: preferences,
            window_ready: cfg!(windows),
            signal,
            focus,
            held: Gesture::default(),
            selected: 0,
            menu: None,
            _tasks: vec![event_task, clock],
            focus_pending: None,
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
        #[cfg(windows)]
        cx.defer_in(window, |view, window, cx| view.update(window, cx));
        result
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
            self.window.expanded = true;
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

    fn focus_run(&mut self, key: PresentedRunKey) {
        let id = Uuid::new_v4();
        if self.parent.focus(id, key) {
            self.pending = Some(id);
            self.focus_pending = Some(id);
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

    fn update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.signal.quitting() {
            cx.quit();
            return;
        }
        let opened = self.signal.take_open();
        if opened {
            self.window.expanded = false;
            self.section = Section::Slots;
            self.held.cancel();
            self.menu = None;
            self.set_page(0);
        }
        if !self.held.active() && self.menu.is_none() {
            self.refresh_view();
        }
        let tray = self.background.view.borrow().background;
        if let Some(notice) = &self.notice
            && self.focus_pending == Some(notice.id)
        {
            self.focus_pending = None;
            if !notice.failed && !self.window.always_on_top() && tray {
                self.window.hide(window);
            }
        }
        #[cfg(windows)]
        self.pairing.poll();
        if self.window_ready {
            self.window.update(window, tray, opened);
        }
        cx.notify();
    }

    fn activate(&mut self, action: FocusAction) {
        if self.pending.is_some() {
            return;
        }
        match action {
            FocusAction::Desktop(key) => self.focus_run(key),
            FocusAction::Companion(snapshot, body) => {
                self.submit(&snapshot, body);
                self.focus_pending = self.pending;
            }
        }
    }

    fn expand(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.held.cancel();
        self.menu = None;
        self.section = section;
        self.window.expanded = true;
        self.set_page(0);
        self.update(window, cx);
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.window.expanded {
            self.expanded(window, cx)
        } else {
            self.floating(cx)
        };
        column()
            .id("agent-deck")
            .size_full()
            .relative()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_size(px(13.))
            .line_height(gpui::relative(1.2))
            .font_family("Segoe UI")
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
                    if view.menu.take().is_none() && view.background.view.borrow().background {
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
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && let Some(action) = view.held.release(Input::Keyboard, view.selected)
                {
                    view.activate(action);
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
