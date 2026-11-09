use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;

use anyhow::{Context as _, Result};
use corvid_companion::launch::{DesktopStartup, prepare_desktop, wait_for_desktop};
use corvid_desktop::ipc::DesktopClient;
use gpui::{Entity, Task};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::*;

type Identity = (Uuid, Uuid);
type StartupReply = oneshot::Sender<Result<Option<Identity>>>;

#[derive(Clone, Default)]
pub struct DesktopLifetime(Rc<RefCell<Option<StartupJob>>>);

struct StartupJob {
    stop: Option<oneshot::Sender<()>>,
    worker: thread::JoinHandle<()>,
}

impl DesktopLifetime {
    fn cancel(&self) {
        if let Some(stop) = self.0.borrow_mut().as_mut().and_then(|job| job.stop.take()) {
            let _ = stop.send(());
        }
    }

    pub fn shutdown(&self) {
        self.cancel();
        if let Some(job) = self.0.take() {
            let _ = job.worker.join();
        }
    }
}

pub struct SetupView {
    instance: String,
    state: PathBuf,
    alias: String,
    message: Option<String>,
    focus: FocusHandle,
    live: Option<Entity<live::LiveView>>,
    startup: Option<Task<()>>,
    lifetime: DesktopLifetime,
}

impl SetupView {
    pub fn focus(&self, window: &mut Window, cx: &gpui::App) {
        if let Some(live) = &self.live {
            live.read(cx).focus(window);
        } else {
            window.focus(&self.focus);
        }
    }

    pub fn new(
        instance: String,
        state: PathBuf,
        alias: Option<String>,
        lifetime: DesktopLifetime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut view = Self {
            instance,
            state,
            alias: alias.clone().unwrap_or_default(),
            message: None,
            focus,
            live: None,
            startup: None,
            lifetime,
        };
        view.launch(alias, window, cx);
        view
    }

    fn launch(&mut self, alias: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.startup.is_some()
            || self
                .lifetime
                .0
                .borrow()
                .as_ref()
                .is_some_and(|job| !job.worker.is_finished())
        {
            return;
        }
        self.message = None;
        let (stop, cancelled) = oneshot::channel();
        let (reply, result) = oneshot::channel();
        let instance = self.instance.clone();
        let state = self.state.clone();
        let worker = thread::Builder::new()
            .name("corvid-desktop-startup".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime
                        .block_on(launch_companion(&instance, &state, alias, cancelled, reply)),
                    Err(error) => {
                        let _ = reply.send(Err(error.into()));
                    }
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                self.message = Some(format!("Could not start Corvid. {error}"));
                cx.notify();
                return;
            }
        };
        self.lifetime.0.replace(Some(StartupJob {
            stop: Some(stop),
            worker,
        }));
        self.startup = Some(cx.spawn_in(window, async move |view, cx| {
            let result = result
                .await
                .context("companion startup ended")
                .and_then(|reply| reply);
            let _ = view.update_in(cx, |view, window, cx| {
                view.startup = None;
                match result.and_then(|identity| {
                    identity
                        .map(|identity| {
                            DesktopClient::start_scoped(
                                view.instance.clone(),
                                Some(identity),
                                || {},
                            )
                        })
                        .transpose()
                }) {
                    Ok(Some(client)) => {
                        view.live = Some(cx.new(|cx| live::LiveView::new(client, window, cx)));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        view.lifetime.cancel();
                        view.message = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.alias.is_empty() {
            self.launch(Some(self.alias.clone()), window, cx);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.startup.is_some() || self.live.is_some() {
            return;
        }
        if event.keystroke.key == "enter" {
            self.submit(window, cx);
        } else {
            edit_ascii(&mut self.alias, event, cx, 253, |byte| {
                byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
            });
            cx.notify();
        }
    }
}

impl Render for SetupView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().id("desktop-startup").size_full();
        if let Some(live) = &self.live {
            return root.child(live.clone());
        }
        let starting = self.startup.is_some();
        root.track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .child(
                div()
                    .w(px(468.))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .p(px(24.))
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .child(header(
                        if starting { "CONNECTING" } else { "SETUP" },
                        AMBER,
                        0x392b23,
                    ))
                    .when(starting, |panel| panel.child("Starting the companion…"))
                    .when(!starting, |panel| {
                        panel
                            .child("Connect to your Corvid host")
                            .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child(
                                "Enter the OpenSSH alias you already use to reach the Linux host.",
                            ))
                            .child(
                                div()
                                    .id("alias-field")
                                    .px(px(12.))
                                    .py(px(10.))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .bg(rgb(RECESS))
                                    .truncate()
                                    .text_color(rgb(if self.alias.is_empty() { DIM } else { TEXT }))
                                    .child(if self.alias.is_empty() {
                                        "e.g. work-host".into()
                                    } else {
                                        self.alias.clone()
                                    })
                                    .on_click(
                                        cx.listener(|view, _, window, _| window.focus(&view.focus)),
                                    ),
                            )
                            .when_some(self.message.clone(), |panel, message| {
                                panel.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(AMBER))
                                        .child(message),
                                )
                            })
                            .child(div().flex().justify_end().child(
                                form_button("connect-host", "Connect", true).on_click(
                                    cx.listener(|view, _, window, cx| view.submit(window, cx)),
                                ),
                            ))
                    }),
            )
    }
}

async fn launch_companion(
    instance: &str,
    state: &Path,
    alias: Option<String>,
    mut stop: oneshot::Receiver<()>,
    reply: StartupReply,
) {
    let prepared = tokio::select! {
        prepared = prepare_desktop(instance, state, alias.as_deref()) => prepared,
        _ = &mut stop => return,
    };
    let config = match prepared {
        Ok(DesktopStartup::NeedsHost) => {
            let _ = reply.send(Ok(None));
            return;
        }
        Ok(DesktopStartup::Reuse(config)) => {
            let _ = reply.send(Ok(Some((config.host_id, config.controller_id))));
            return;
        }
        Ok(DesktopStartup::Start(config)) => config,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
    let (cancel, cancelled) = oneshot::channel();
    let companion = corvid_companion::run(instance, state, async {
        tokio::select! { _ = stop => {}, _ = cancelled => {} }
    });
    tokio::pin!(companion);
    let result = tokio::select! {
        result = &mut companion => {
            let _ = reply.send(result.and_then(|()| anyhow::bail!("companion stopped during startup")));
            return;
        }
        result = wait_for_desktop(instance, &config) => result,
    };
    let started = result.is_ok();
    if reply
        .send(result.map(|()| Some((config.host_id, config.controller_id))))
        .is_err()
        || !started
    {
        let _ = cancel.send(());
    }
    let _ = companion.await;
}
