use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{Context, Result};
use corvid_async::{mpsc, oneshot, watch};
use corvid_client::ControlEndpoint;
use corvid_client::view_state::ViewStore;
use corvid_client::view_state::windows::{WindowRegistry, WindowState};
use corvid_companion::monitor::Scope;
use corvid_presentation::desktop_focus::{FocusRequest, serve_routed};
use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions, px, size,
};
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
    storage: WindowStorage,
    opening: Cell<usize>,
    settings: agent_deck::Settings,
    entries: RefCell<HashMap<Uuid, Entry>>,
    quitting: Cell<bool>,
    applying: Cell<bool>,
    failed_replacement: RefCell<Option<std::path::PathBuf>>,
    ready_to_quit: Cell<bool>,
    closing: RefCell<Vec<watch::Receiver<()>>>,
}

type StorageOperation = Box<dyn FnOnce(&WindowRegistry) + Send>;

#[derive(Clone)]
struct WindowStorage(mpsc::Sender<StorageOperation>);

impl WindowStorage {
    fn start(registry: WindowRegistry) -> Result<Self> {
        let (sender, mut incoming) = mpsc::channel::<StorageOperation>(16);
        std::thread::Builder::new()
            .name("corvid-window-storage".into())
            .spawn(move || {
                while let Some(operation) = incoming.blocking_recv() {
                    operation(&registry);
                }
            })?;
        Ok(Self(sender))
    }

    fn request<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&WindowRegistry) -> Result<T> + Send + 'static,
    ) -> Result<oneshot::Receiver<Result<T>>> {
        let (reply, result) = oneshot::channel();
        self.0
            .try_send(Box::new(move |registry| {
                let _ = reply.send(operation(registry));
            }))
            .map_err(|_| anyhow::anyhow!("Desktop window storage is busy or stopped"))?;
        Ok(result)
    }

    fn close(&self, state: WindowState) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.0
            .send_blocking(Box::new(move |registry| {
                let _ = reply.send(registry.close(state));
            }))
            .map_err(|_| anyhow::anyhow!("Desktop window storage stopped"))?;
        result
            .blocking_recv()
            .context("Desktop window storage stopped")?
    }
}

impl DesktopWindows {
    pub fn new(settings: agent_deck::Settings) -> Result<Self> {
        let storage =
            WindowStorage::start(WindowRegistry::new(settings.state.join("desktop-view")))?;
        Ok(Self(Rc::new(Inner {
            storage,
            opening: Cell::new(0),
            settings,
            entries: RefCell::new(HashMap::new()),
            quitting: Cell::new(false),
            applying: Cell::new(false),
            failed_replacement: RefCell::default(),
            ready_to_quit: Cell::new(false),
            closing: RefCell::new(Vec::new()),
        })))
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
                cx.update(|cx| windows.route_deck_focus(endpoint, request, cx));
            }
        })
        .detach();
        Ok(())
    }

    fn route_deck_focus(&self, endpoint: ControlEndpoint, request: FocusRequest, cx: &mut App) {
        if self.0.quitting.get() || request.result.is_closed() {
            return;
        }
        if self.0.entries.borrow().is_empty() {
            if self.0.opening.get() != 0 {
                let _ = request
                    .result
                    .send(Err("Desktop is opening a window. Try again.".into()));
                return;
            }
            let allocation = self.0.storage.request(WindowRegistry::create);
            let windows = self.clone();
            self.complete_open(allocation, cx, move |state, cx| {
                if let Err(error) = state.and_then(|state| windows.open(state, None, true, cx)) {
                    let _ = request
                        .result
                        .send(Err("Could not open a Desktop window.".into()));
                    eprintln!("Could not open a Desktop window: {error:#}");
                    return;
                }
                windows.route_deck_focus(endpoint, request, cx);
            });
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

    pub fn apply_update(
        &self,
        apply: impl FnOnce() -> Result<corvid_update::Installed> + Send + 'static,
        applied: impl FnOnce(Result<corvid_update::Installed>, &mut App) + 'static,
        restart_failed: impl FnOnce(anyhow::Error, &mut App) + 'static,
        cx: &mut App,
    ) {
        if self.0.applying.replace(true) {
            applied(
                Err(anyhow::anyhow!("Another Desktop update is being applied")),
                cx,
            );
            return;
        }

        let windows = self.clone();
        let install = cx.background_executor().spawn(async move { apply() });
        cx.spawn(async move |cx| {
            let result = install.await;
            windows.0.applying.set(false);
            let replacement = result.as_ref().ok().cloned();
            cx.update(move |cx| {
                applied(result, cx);
                if let Some(installed) = replacement {
                    windows.restart(Rc::new(installed), restart_failed, cx);
                }
            });
        })
        .detach();
    }

    pub fn watch_installed_version(&self, cx: &mut App) {
        let Ok(Some(install)) = corvid_update::Installation::detect() else {
            return;
        };
        if install.root.is_none() {
            return;
        }
        if let Err(error) = install.resume_retirement() {
            eprintln!("Could not resume Corvid cleanup: {error:#}");
        }
        let windows = self.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |cx| {
            loop {
                executor.timer(std::time::Duration::from_secs(3)).await;
                let install = install.clone();
                let (replacement, pending) = executor
                    .spawn(async move {
                        (
                            install.selected_replacement(),
                            install.pending_retirement().is_some_and(|p| p.is_file()),
                        )
                    })
                    .await;
                cx.update(|cx| {
                    for entry in windows.0.entries.borrow().values() {
                        let _ = entry
                            .handle
                            .update(cx, |view, _, cx| view.show_pending_cleanup(pending, cx));
                    }
                    if let Ok(Some(next)) = replacement
                        && !windows.0.applying.get()
                        && !windows.0.quitting.get()
                        && windows.0.failed_replacement.borrow().as_ref() != Some(&next.executable)
                    {
                        windows.restart(
                            Rc::new(next),
                            |e, _| eprintln!("Could not switch Desktop versions: {e:#}"),
                            cx,
                        );
                    }
                });
            }
        })
        .detach();
    }

    pub fn restore(
        &self,
        destination: Option<Destination>,
        ready_dir: Option<std::path::PathBuf>,
        window_ids: Vec<Uuid>,
        cx: &mut App,
    ) {
        let requested = destination.clone();
        let restored = self.0.storage.request(move |registry| {
            if !window_ids.is_empty() {
                return window_ids
                    .into_iter()
                    .map(|id| {
                        let state = registry.reclaim(id)?;
                        let picker = state.store.last().is_none();
                        Ok((state, picker))
                    })
                    .collect();
            }
            let mut states = registry.restore()?;
            if let Some(destination) = requested {
                let target = destination.saved_target();
                let existing = states.iter().position(|state| {
                    state.store.for_target(&target).is_some()
                        || (state.first_launch && state.store.last().is_none())
                });
                let state = match existing {
                    Some(index) => states.remove(index),
                    None => registry.create()?,
                };
                return Ok(vec![(state, false)]);
            }
            if states.is_empty() {
                return Ok(vec![(registry.create()?, true)]);
            }
            Ok(states
                .into_iter()
                .map(|state| {
                    let picker = !state.first_launch && state.store.last().is_none();
                    (state, picker)
                })
                .collect())
        });
        let windows = self.clone();
        self.complete_open(restored, cx, move |states, cx| {
            let result = states.and_then(|states| {
                for (state, picker) in states {
                    windows.open(state, destination.clone(), picker, cx)?;
                }
                if let Some(path) = ready_dir {
                    anyhow::ensure!(
                        path.parent() == Some(std::env::temp_dir().as_path())
                            && path.file_name().is_some_and(|n| n
                                .to_string_lossy()
                                .starts_with("corvid-desktop-ready-")),
                        "invalid Desktop startup acknowledgment directory"
                    );
                    std::fs::File::create_new(path.join("ready"))?;
                }
                Ok(())
            });
            if let Err(error) = result {
                eprintln!("Could not restore Desktop windows: {error:#}");
                windows.quit(cx);
            }
        });
    }

    pub fn new_window(&self, cx: &mut App, failed: impl FnOnce(anyhow::Error, &mut App) + 'static) {
        if self.0.quitting.get() {
            return;
        }
        let created = self.0.storage.request(WindowRegistry::create);
        let windows = self.clone();
        self.complete_open(created, cx, move |state, cx| {
            if let Err(error) = state.and_then(|state| windows.open(state, None, true, cx)) {
                failed(error, cx);
            }
        });
    }

    fn complete_open<T: 'static>(
        &self,
        request: Result<oneshot::Receiver<Result<T>>>,
        cx: &mut App,
        complete: impl FnOnce(Result<T>, &mut App) + 'static,
    ) {
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                complete(Err(error), cx);
                return;
            }
        };
        self.0.opening.set(self.0.opening.get() + 1);
        let windows = self.clone();
        cx.spawn(async move |cx| {
            let result = request
                .await
                .context("Desktop window storage stopped")
                .and_then(|result| result);
            cx.update(|cx| {
                windows.0.opening.set(windows.0.opening.get() - 1);
                if !windows.0.quitting.get() {
                    complete(result, cx);
                }
            });
        })
        .detach();
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
                appears_transparent: cfg!(any(windows, target_os = "macos")),
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
                cx.update(|cx| windows.quit(cx));
            })
            .detach();
            return false;
        }
        let Some(mut entry) = self.0.entries.borrow_mut().remove(&id) else {
            return true;
        };
        let storage = self.0.storage.clone();
        let state = entry.state.take();
        let closing = entry.lifetime.start_shutdown(move || {
            if let Some(state) = state
                && let Err(error) = storage.close(state)
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
            cx.update(|cx| windows.quit_after_shutdown(cx));
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
        let installed = (*installed).clone();
        let ids: Vec<_> = self
            .0
            .entries
            .borrow()
            .keys()
            .map(ToString::to_string)
            .collect();
        let args = vec![
            std::ffi::OsString::from("--state-dir"),
            self.0.settings.state.clone().into_os_string(),
        ];
        #[cfg(windows)]
        let args = args.into_iter().chain([
            "--pipe-instance".into(),
            self.0.settings.pipe_instance.clone().into(),
        ]);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |cx| {
            wait_for_shutdown(pending).await;
            cx.update(|_| {
                for entry in windows.0.entries.borrow_mut().values_mut() {
                    entry.state.take();
                }
            });
            let target = installed.executable.clone();
            let started = executor
                .spawn(async move { installed.start_desktop_restoring(args, &ids) })
                .await;
            cx.update(|cx| {
                if let Err(error) = windows.finish_restart(started, cx) {
                    *windows.0.failed_replacement.borrow_mut() = Some(target);
                    failed(error, cx);
                }
            });
        })
        .detach();
    }

    fn finish_restart(&self, started: Result<()>, cx: &mut App) -> Result<()> {
        match started {
            Ok(()) => {
                self.quit_after_shutdown(cx);
                Ok(())
            }
            Err(error) => {
                let ids: Vec<_> = self.0.entries.borrow().keys().copied().collect();
                let reclaimed = self.0.storage.request(move |registry| {
                    Ok(ids
                        .into_iter()
                        .map(|id| (id, registry.reclaim(id)))
                        .collect::<Vec<_>>())
                });
                let windows = self.clone();
                cx.spawn(async move |cx| {
                    let states = match reclaimed {
                        Ok(reclaimed) => reclaimed
                            .await
                            .context("Desktop window storage stopped")
                            .and_then(|result| result),
                        Err(error) => Err(error),
                    };
                    cx.update(|cx| windows.resume_windows(states, cx));
                })
                .detach();
                Err(error)
            }
        }
    }
    fn resume_windows(&self, states: Result<Vec<(Uuid, Result<WindowState>)>>, cx: &mut App) {
        self.0.quitting.set(false);
        let states = match states {
            Ok(states) => states,
            Err(error) => {
                eprintln!("Could not reclaim Desktop windows: {error:#}");
                self.quit(cx);
                return;
            }
        };
        for (id, state) in states {
            let handle = {
                let mut entries = self.0.entries.borrow_mut();
                let Some(entry) = entries.get_mut(&id) else {
                    continue;
                };
                match state {
                    Ok(state) => {
                        entry.state = Some(state);
                        Ok(entry.handle)
                    }
                    Err(error) => {
                        eprintln!("Could not resume Desktop window: {error:#}");
                        let handle = entry.handle;
                        entries.remove(&id);
                        Err(handle)
                    }
                }
            };
            match handle {
                Ok(handle) => {
                    let _ = handle.update(cx, |view, window, cx| view.resume(window, cx));
                }
                Err(handle) => {
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                }
            }
        }
        if self.0.entries.borrow().is_empty() {
            self.quit(cx);
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
        corvid_async::select! {
            _ = requests.closed() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

async fn serve_deck_scope(scope: Scope, requests: mpsc::Sender<(ControlEndpoint, FocusRequest)>) {
    let (focus, mut incoming) = mpsc::channel::<FocusRequest>(1);
    let listener = serve_routed(scope.config.host_id, scope.config.controller_id, focus);
    corvid_async::pin!(listener);
    loop {
        corvid_async::select! {
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn applied_update_restarts_after_the_initiating_view_is_gone() -> Result<()> {
        let package = tempfile::tempdir()?;
        std::fs::write(package.path().join("package.json"), "{}")?;
        let desktop = package.path().join("corvid-desktop");
        std::fs::write(&desktop, "#!/bin/sh\n: > \"${0%/*}/restarted\"\n")?;
        std::fs::set_permissions(&desktop, std::fs::Permissions::from_mode(0o700))?;
        let cli = package.path().join("corvid");
        let windows =
            DesktopWindows::new(agent_deck::Settings::from(package.path().join("state")))?;
        let timed_out = Rc::new(Cell::new(false));
        let expired = timed_out.clone();

        gpui_platform::headless().run(move |cx| {
            let owner = cx.new(|_| ());
            let view = owner.downgrade();
            windows.apply_update(
                move || {
                    Ok(corvid_update::Installed {
                        executable: cli,
                        message: String::new(),
                    })
                },
                move |result, _| {
                    assert!(view.upgrade().is_none());
                    assert!(result.is_ok());
                },
                |error, _| panic!("restart failed: {error:#}"),
                cx,
            );
            drop(owner);

            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                expired.set(true);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });

        assert!(!timed_out.get(), "Desktop did not quit after restarting");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !package.path().join("restarted").exists() {
            assert!(Instant::now() < deadline, "replacement Desktop did not run");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
