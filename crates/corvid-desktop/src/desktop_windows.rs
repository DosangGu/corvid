use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use corvid_client::view_state::ViewStore;
use corvid_client::view_state::windows::{WindowRegistry, WindowState};
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
    registry: WindowRegistry,
    settings: agent_deck::Settings,
    entries: RefCell<HashMap<Uuid, Entry>>,
    quitting: Cell<bool>,
}

impl DesktopWindows {
    pub fn new(settings: agent_deck::Settings) -> Self {
        Self(Rc::new(Inner {
            registry: WindowRegistry::new(settings.state.join("desktop-view")),
            settings,
            entries: RefCell::new(HashMap::new()),
            quitting: Cell::new(false),
        }))
    }

    pub fn settings(&self) -> agent_deck::Settings {
        self.0.settings.clone()
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
                ..Default::default()
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
            return true;
        }
        if self.0.entries.borrow().len() == 1 {
            self.quit(cx);
            return false;
        }
        let Some(mut entry) = self.0.entries.borrow_mut().remove(&id) else {
            return true;
        };
        entry.lifetime.shutdown();
        if let Some(state) = entry.state.take()
            && let Err(error) = self.0.registry.close(state)
        {
            eprintln!("Could not remove closed Desktop window: {error:#}");
        }
        true
    }

    pub fn quit(&self, cx: &mut App) {
        self.shutdown();
        cx.quit();
    }

    pub fn shutdown(&self) {
        self.0.quitting.set(true);
        let entries = self.0.entries.borrow();
        for entry in entries.values() {
            entry.lifetime.stop();
        }
        for entry in entries.values() {
            entry.lifetime.shutdown();
        }
    }

    pub fn restart(&self, installed: &corvid_update::Installed, cx: &mut App) -> Result<()> {
        self.shutdown();
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
                cx.quit();
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
