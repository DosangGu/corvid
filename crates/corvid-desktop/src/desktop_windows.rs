use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use corvid_client::ControlEndpoint;
use corvid_client::view_state::ViewStore;
use corvid_client::view_state::windows::{WindowRegistry, WindowState};
use corvid_companion::monitor::Scope;
use corvid_presentation::desktop_focus::{FocusRequest, serve_routed};
use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions, px, size,
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::agent_deck;
use crate::terminal_session::Destination;
use crate::workspace_view::{Lifetime, WorkspaceView};

pub struct WindowLaunch {
    pub lifetime: Lifetime,
    pub store: ViewStore,
    pub destination: Option<Destination>,
    pub picker: bool,
}

struct Entry {
    state: Option<WindowState>,
    lifetime: Lifetime,
    handle: WindowHandle<WorkspaceView>,
}

#[derive(Clone)]
pub struct DesktopWindows(Rc<Inner>);

struct Inner {
    registry: WindowRegistry,
    settings: agent_deck::Settings,
    entries: RefCell<HashMap<Uuid, Entry>>,
    quitting: Cell<bool>,
    ready_to_quit: Cell<bool>,
    closing: RefCell<Vec<watch::Receiver<()>>>,
}

impl DesktopWindows {
    pub fn new(settings: agent_deck::Settings) -> Self {
        Self(Rc::new(Inner {
            registry: WindowRegistry::new(settings.state.join("desktop-view")),
            settings,
            entries: RefCell::new(HashMap::new()),
            quitting: Cell::new(false),
            ready_to_quit: Cell::new(false),
            closing: RefCell::new(Vec::new()),
        }))
    }

    pub fn settings(&self) -> agent_deck::Settings {
        self.0.settings.clone()
    }

    pub fn listen_for_deck(&self, cx: &mut App) -> Result<()> {
        let (requests, mut incoming) = mpsc::channel(8);
        let state = self.0.settings.state.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("corvid-desktop-focus".into())
            .spawn(move || {
                runtime.block_on(listen_for_deck(state, requests));
            })?;
        let windows = self.clone();
        cx.spawn(async move |cx| {
            while let Some((endpoint, request)) = incoming.recv().await {
                if cx
                    .update(|cx| windows.route_deck_focus(endpoint, request, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Ok(())
    }

    fn route_deck_focus(&self, endpoint: ControlEndpoint, request: FocusRequest, cx: &mut App) {
        if self.0.quitting.get() || request.result.is_closed() {
            return;
        }
        if self.0.entries.borrow().is_empty()
            && let Err(error) = self.new_window(cx)
        {
            let _ = request
                .result
                .send(Err("Could not open a Desktop window.".into()));
            eprintln!("Could not open a Desktop window: {error:#}");
            return;
        }
        let active = cx.active_window().map(|window| window.window_id());
        let handles: Vec<_> = self
            .0
            .entries
            .borrow()
            .iter()
            .map(|(id, entry)| (*id, entry.handle))
            .collect();
        let target = handles
            .into_iter()
            .filter_map(|(id, handle)| {
                let rank = handle
                    .update(cx, |view, _, _| view.deck_focus_rank(&request.run))
                    .ok()?;
                Some(((rank, active == Some(handle.window_id()), id), handle))
            })
            .max_by_key(|(priority, _)| *priority);
        if let Some((_, handle)) = target {
            let _ = handle.update(cx, |view, window, cx| {
                view.route_deck_focus(endpoint, request, window, cx)
            });
        } else {
            let _ = request
                .result
                .send(Err("No Desktop window is available.".into()));
        }
    }

    pub fn restore(&self, destination: Option<Destination>, cx: &mut App) -> Result<()> {
        let mut windows = self.0.registry.restore()?;
        if let Some(destination) = destination {
            let target = destination.saved_target();
            let existing = windows.iter().position(|state| {
                state.store.for_target(&target).is_some()
                    || (state.first_launch && state.store.last().is_none())
            });
            let state = match existing {
                Some(index) => windows.remove(index),
                None => self.0.registry.create()?,
            };
            return self.open(state, Some(destination), false, cx);
        }
        if windows.is_empty() {
            return self.new_window(cx);
        }
        for state in windows {
            let picker = !state.first_launch && state.store.last().is_none();
            self.open(state, None, picker, cx)?;
        }
        Ok(())
    }

    pub fn new_window(&self, cx: &mut App) -> Result<()> {
        if self.0.quitting.get() {
            return Ok(());
        }
        self.open(self.0.registry.create()?, None, true, cx)
    }

    fn open(
        &self,
        state: WindowState,
        destination: Option<Destination>,
        picker: bool,
        cx: &mut App,
    ) -> Result<()> {
        let id = state.id;
        let lifetime = Lifetime::default();
        let launch = WindowLaunch {
            lifetime: lifetime.clone(),
            store: state.store.clone(),
            destination,
            picker,
        };
        // TODO: Restore Desktop window placement
        // Persist bounds separately from workspace navigation and validate them against the current monitor layout before applying them.
        // Issue URL: https://github.com/DosangGu/corvid/issues/49
        let bounds = Bounds::centered(None, size(px(1080.), px(680.)), cx);
        let options = WindowOptions {
            app_id: Some(crate::app_icon::APP_ID.into()),
            titlebar: Some(TitlebarOptions {
                title: Some("Corvid Desktop".into()),
                appears_transparent: cfg!(target_os = "macos"),
                traffic_light_position: cfg!(target_os = "macos")
                    .then(|| gpui::point(px(12.), px(13.))),
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(620.))),
            ..Default::default()
        };
        let manager = self.clone();
        let handle = cx.open_window(options, move |window, cx| {
            #[cfg(target_os = "linux")]
            if let Err(error) = crate::app_icon::set_window_icons() {
                eprintln!("Could not set Corvid Desktop icon: {error}");
            }
            let closing = manager.clone();
            window.on_window_should_close(cx, move |_, cx| closing.close(id, cx));
            cx.new(|cx| WorkspaceView::new(launch, manager, window, cx))
        })?;
        self.0.entries.borrow_mut().insert(
            id,
            Entry {
                state: Some(state),
                lifetime,
                handle,
            },
        );
        Ok(())
    }

    fn close(&self, id: Uuid, cx: &mut App) -> bool {
        if self.0.quitting.get() {
            return self.0.ready_to_quit.get();
        }
        if !cfg!(target_os = "macos") && self.0.entries.borrow().len() == 1 {
            // X11 invokes this callback while borrowing its platform state.
            let windows = self.clone();
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| windows.quit(cx));
            })
            .detach();
            return false;
        }
        let Some(mut entry) = self.0.entries.borrow_mut().remove(&id) else {
            return true;
        };
        let registry = self.0.registry.clone();
        let state = entry.state.take();
        let closing = entry.lifetime.start_shutdown(move || {
            if let Some(state) = state
                && let Err(error) = registry.close(state)
            {
                eprintln!("Could not remove closed Desktop window: {error:#}");
            }
        });
        let mut pending = self.0.closing.borrow_mut();
        pending.retain(|closed| closed.has_changed().is_ok());
        pending.push(closing);
        true
    }

    pub fn quit(&self, cx: &mut App) {
        if self.0.quitting.replace(true) {
            return;
        }
        let pending = self.start_shutdown();
        let windows = self.clone();
        cx.spawn(async move |cx| {
            wait_for_shutdown(pending).await;
            let _ = cx.update(|cx| windows.quit_after_shutdown(cx));
        })
        .detach();
    }

    fn quit_after_shutdown(&self, cx: &App) {
        self.0.ready_to_quit.set(true);
        cx.quit();
    }

    #[cfg(target_os = "macos")]
    pub fn close_active(&self, cx: &mut App) {
        let Some(active) = cx.active_window() else {
            return;
        };
        let id = self
            .0
            .entries
            .borrow()
            .iter()
            .find(|(_, entry)| entry.handle.window_id() == active.window_id())
            .map(|(id, _)| *id);
        if let Some(id) = id
            && self.close(id, cx)
        {
            let _ = active.update(cx, |_, window, _| window.remove_window());
        }
    }

    pub fn start_shutdown(&self) -> Vec<watch::Receiver<()>> {
        self.0.quitting.set(true);
        let mut pending = self.0.closing.borrow().clone();
        pending.extend(
            self.0
                .entries
                .borrow()
                .values()
                .map(|entry| entry.lifetime.start_shutdown(|| {})),
        );
        pending
    }

    pub fn restart(
        &self,
        installed: Rc<corvid_update::Installed>,
        failed: impl FnOnce(anyhow::Error, &mut App) + 'static,
        cx: &mut App,
    ) {
        if self.0.quitting.replace(true) {
            return;
        }
        let pending = self.start_shutdown();
        let windows = self.clone();
        cx.spawn(async move |cx| {
            wait_for_shutdown(pending).await;
            let _ = cx.update(|cx| {
                if let Err(error) = windows.finish_restart(&installed, cx) {
                    failed(error, cx);
                }
            });
        })
        .detach();
    }

    fn finish_restart(&self, installed: &corvid_update::Installed, cx: &mut App) -> Result<()> {
        for entry in self.0.entries.borrow_mut().values_mut() {
            entry.state.take();
        }
        let args = vec![
            std::ffi::OsString::from("--state-dir"),
            self.0.settings.state.clone().into_os_string(),
        ];
        #[cfg(windows)]
        let args = args.into_iter().chain([
            "--pipe-instance".into(),
            self.0.settings.pipe_instance.clone().into(),
        ]);
        match installed.start_desktop_with_args(args) {
            Ok(()) => {
                self.quit_after_shutdown(cx);
                Ok(())
            }
            Err(error) => {
                self.0.quitting.set(false);
                let (handles, displaced) = {
                    let mut entries = self.0.entries.borrow_mut();
                    let mut handles = Vec::new();
                    let mut displaced = Vec::new();
                    entries.retain(|id, entry| match self.0.registry.reclaim(*id) {
                        Ok(state) => {
                            entry.state = Some(state);
                            handles.push(entry.handle);
                            true
                        }
                        Err(error) => {
                            eprintln!("Could not resume Desktop window: {error:#}");
                            displaced.push(entry.handle);
                            false
                        }
                    });
                    (handles, displaced)
                };
                // A deferred update also covers the window currently handling Restart.
                let windows = self.clone();
                cx.defer(move |cx| {
                    for handle in displaced {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                    for handle in handles {
                        let _ = handle.update(cx, |view, window, cx| view.resume(window, cx));
                    }
                    if windows.0.entries.borrow().is_empty() {
                        windows.quit(cx);
                    }
                });
                Err(error)
            }
        }
    }
}

pub async fn wait_for_shutdown(pending: Vec<watch::Receiver<()>>) {
    for mut shutdown in pending {
        let _ = shutdown.changed().await;
    }
}

async fn listen_for_deck(
    state: std::path::PathBuf,
    requests: mpsc::Sender<(ControlEndpoint, FocusRequest)>,
) {
    let mut listeners = HashMap::new();
    loop {
        if let Ok(scopes) = corvid_companion::monitor::discover(&state) {
            let mut current = HashMap::new();
            for scope in scopes {
                let controller = scope.config.controller_id;
                if current.contains_key(&controller) {
                    continue;
                }
                let listener = match listeners.remove(&controller) {
                    Some(listener) if !tokio::task::JoinHandle::<()>::is_finished(&listener) => {
                        listener
                    }
                    _ => tokio::spawn(serve_deck_scope(scope, requests.clone())),
                };
                current.insert(controller, listener);
            }
            for listener in listeners.into_values() {
                listener.abort();
            }
            listeners = current;
        }
        tokio::select! {
            _ = requests.closed() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

async fn serve_deck_scope(scope: Scope, requests: mpsc::Sender<(ControlEndpoint, FocusRequest)>) {
    let (focus, mut incoming) = mpsc::channel::<FocusRequest>(1);
    let listener = serve_routed(scope.config.host_id, scope.config.controller_id, focus);
    tokio::pin!(listener);
    loop {
        tokio::select! {
            _ = &mut listener => break,
            _ = requests.closed() => break,
            request = incoming.recv() => {
                let Some(request) = request else { break };
                match scope.control_endpoint() {
                    Ok(endpoint) => {
                        if let Err(error) = requests.try_send((endpoint, request)) {
                            let (_, request) = error.into_inner();
                            let _ = request.result.send(Err("Desktop is busy. Try again.".into()));
                        }
                    }
                    Err(_) => {
                        let _ = request.result.send(Err("Could not read the agent host connection.".into()));
                    }
                }
            }
        }
    }
}
