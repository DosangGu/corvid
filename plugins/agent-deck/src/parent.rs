use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use corvid_agent_deck::connection::{self, CAPABILITY, Peer};
use corvid_presentation::ipc::Completion;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{AgentDeckOpened, FocusDesktopAgent, PresentedRunKey};
use corvid_protocol::{COMMAND_TIMEOUT, PeerRole, ProtocolFault, accept_hello};
use eframe::egui;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use uuid::Uuid;

#[derive(Clone)]
struct Target {
    generation: Uuid,
    pid: u32,
    requests: mpsc::Sender<(Uuid, PresentedRunKey)>,
}

#[derive(Clone, Default)]
pub(crate) struct DesktopLink(Arc<Mutex<Option<Target>>>);

#[derive(Clone)]
pub(crate) struct Scope {
    pub host: Uuid,
    pub controller: Uuid,
    pub epoch: Uuid,
    pub completed: mpsc::Sender<Completion>,
    pub ctx: egui::Context,
}

struct Exchange {
    scope: Scope,
    generation: Uuid,
    pending: Option<(Uuid, Instant)>,
}

impl DesktopLink {
    pub fn connected(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|target| !target.requests.is_closed())
    }

    pub fn focus(&self, id: Uuid, run: PresentedRunKey) -> bool {
        let target = self.0.lock().unwrap().clone();
        let Some(target) = target else {
            return false;
        };
        #[cfg(windows)]
        if target.pid != 0 {
            // The private pipe supplies the peer PID; labels cannot redirect it.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(target.pid);
            }
        }
        #[cfg(not(windows))]
        let _ = target.pid;
        target.requests.try_send((id, run)).is_ok()
    }

    pub async fn serve(&self, peer: Peer, scope: Scope) -> Result<()> {
        let mut exchange = Exchange {
            scope,
            generation: Uuid::new_v4(),
            pending: None,
        };
        let result = self.exchange(peer, &mut exchange).await;
        let mut target = self.0.lock().unwrap();
        if target
            .as_ref()
            .is_some_and(|target| target.generation == exchange.generation)
        {
            *target = None;
        }
        if let Some((id, _)) = exchange.pending {
            let _ = exchange.scope.completed.try_send(Completion {
                id,
                message: "Desktop disconnected before focus was confirmed.".into(),
                failed: true,
                paired: None,
            });
        }
        exchange.scope.ctx.request_repaint();
        result
    }

    async fn exchange(&self, peer: Peer, exchange: &mut Exchange) -> Result<()> {
        let Scope {
            host,
            controller,
            epoch,
            completed,
            ctx,
        } = &exchange.scope;
        let generation = exchange.generation;
        let pending = &mut exchange.pending;
        let (mut reader, mut writer) =
            timeout(COMMAND_TIMEOUT, connection::frames(peer.stream)).await??;
        let hello = timeout(COMMAND_TIMEOUT, reader.next_envelope())
            .await??
            .context("missing plugin Hello")?;
        let (session, reply) = accept_hello(
            &hello,
            PeerRole::Presentation,
            &host.to_string(),
            &epoch.to_string(),
            &[CAPABILITY],
        )?;
        timeout(COMMAND_TIMEOUT, writer.envelope(&reply)).await??;
        let (requests, mut incoming) = mpsc::channel(1);
        let mut registered = false;
        loop {
            tokio::select! {
                request = reader.next_envelope() => {
                    let Some(request) = request? else {
                        break;
                    };
                    session.validate_scope(&request)?;
                    let id = connection::request_id(&request)?;
                    match request.body {
                        Some(Body::OpenAgentDeck(open)) => {
                            ensure!(
                                open.host_id == host.to_string() && open.controller_id == controller.to_string(),
                                "unexpected Agent Deck identity"
                            );
                            if open.register_desktop {
                                ensure!(!registered, "Desktop already registered");
                                *self.0.lock().unwrap() = Some(Target {
                                    generation,
                                    pid: peer.pid,
                                    requests: requests.clone(),
                                });
                                registered = true;
                            }
                            show(ctx);
                            let reply = session.envelope(
                                Some(id.to_string()),
                                Body::AgentDeckOpened(AgentDeckOpened {}),
                            );
                            timeout(COMMAND_TIMEOUT, writer.envelope(&reply)).await??;
                        }
                        Some(body) if registered && pending.is_some_and(|pending| pending.0 == id) => {
                            let (message, failed) = match body {
                                Body::DesktopAgentFocused(_) => ("Focused in Corvid Desktop.".into(), false),
                                Body::Error(error) => (ProtocolFault::from_wire(&error).to_string(), true),
                                _ => anyhow::bail!("unexpected Desktop focus result"),
                            };
                            *pending = None;
                            let _ = completed.try_send(Completion { id, message, failed, paired: None });
                            ctx.request_repaint();
                        }
                        _ => anyhow::bail!("unsupported plugin message"),
                    }
                }
                request = incoming.recv(), if registered && pending.is_none() => {
                    let Some((id, run)) = request else {
                        break;
                    };
                    let registered = self.0.lock().unwrap().as_ref()
                        .is_some_and(|target| target.generation == generation);
                    ensure!(registered, "Desktop registration superseded");
                    ensure!(run.host_id == host.to_string(), "wrong focus host");
                    *pending = Some((id, Instant::now() + COMMAND_TIMEOUT));
                    let request = session.envelope(
                        Some(id.to_string()),
                        Body::FocusDesktopAgent(FocusDesktopAgent { run: Some(run) }),
                    );
                    timeout(COMMAND_TIMEOUT, writer.envelope(&request)).await??;
                }
                _ = tokio::time::sleep_until(pending.map_or_else(|| Instant::now() + COMMAND_TIMEOUT, |pending| pending.1)) => {
                    if pending.is_some() || !registered {
                        break;
                    }
                    if self.0.lock().unwrap().as_ref().is_none_or(|target| target.generation != generation) {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn show(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_agent_deck::connection::{Reader, Writer};
    use corvid_protocol::wire::{DesktopAgentFocused, OpenAgentDeck};
    use corvid_protocol::{Session, check_hello_result, hello};

    async fn register(
        link: &DesktopLink,
        scope: &Scope,
        desktop: bool,
    ) -> (Reader, Writer, Session, tokio::task::JoinHandle<Result<()>>) {
        let (client, server) = tokio::io::duplex(8192);
        let server_link = link.clone();
        let server_scope = scope.clone();
        let task = tokio::spawn(async move {
            server_link
                .serve(
                    Peer {
                        stream: Box::new(server),
                        pid: 0,
                    },
                    server_scope,
                )
                .await
        });
        let (mut reader, mut writer) = connection::frames(Box::new(client)).await.unwrap();
        let greeting = hello(Uuid::new_v4().to_string(), None, vec![CAPABILITY.into()]);
        writer.envelope(&greeting).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let session = check_hello_result(&greeting, &reply, PeerRole::Presentation).unwrap();
        writer
            .envelope(&session.envelope(
                Some(Uuid::new_v4().to_string()),
                Body::OpenAgentDeck(OpenAgentDeck {
                    host_id: scope.host.to_string(),
                    controller_id: scope.controller.to_string(),
                    register_desktop: desktop,
                }),
            ))
            .await
            .unwrap();
        assert!(matches!(
            reader.next_envelope().await.unwrap().unwrap().body,
            Some(Body::AgentDeckOpened(_))
        ));
        (reader, writer, session, task)
    }

    fn scope() -> (Scope, mpsc::Receiver<Completion>) {
        let (completed, result) = mpsc::channel(8);
        (
            Scope {
                host: Uuid::new_v4(),
                controller: Uuid::new_v4(),
                epoch: Uuid::new_v4(),
                completed,
                ctx: egui::Context::default(),
            },
            result,
        )
    }

    #[tokio::test]
    async fn activation_keeps_the_registered_desktop_and_focus_keeps_exact_run() {
        timeout(COMMAND_TIMEOUT, async {
            let link = DesktopLink::default();
            let (scope, mut completed) = scope();
            let (mut reader, mut writer, session, registration) =
                register(&link, &scope, true).await;
            let (activation_reader, activation_writer, _, activation) =
                register(&link, &scope, false).await;
            drop((activation_reader, activation_writer));
            activation.await.unwrap().unwrap();
            assert!(link.connected());
            let id = Uuid::new_v4();
            let run = PresentedRunKey {
                host_id: scope.host.to_string(),
                remote_epoch: Uuid::new_v4().to_string(),
                agent_run_id: Uuid::new_v4().to_string(),
            };
            assert!(link.focus(id, run.clone()));
            let request = reader.next_envelope().await.unwrap().unwrap();
            assert_eq!(request.request_id, Some(id.to_string()));
            assert_eq!(
                request.body,
                Some(Body::FocusDesktopAgent(FocusDesktopAgent {
                    run: Some(run)
                }))
            );
            writer
                .envelope(&session.envelope(
                    Some(id.to_string()),
                    Body::DesktopAgentFocused(DesktopAgentFocused {}),
                ))
                .await
                .unwrap();
            assert!(!completed.recv().await.unwrap().failed);
            drop((reader, writer));
            registration.await.unwrap().unwrap();
            assert!(!link.connected());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn old_disconnect_cannot_unregister_the_reopened_desktop() {
        timeout(COMMAND_TIMEOUT, async {
            let link = DesktopLink::default();
            let (scope, mut completed) = scope();
            let (old_reader, old_writer, _, old) = register(&link, &scope, true).await;
            let (mut reader, writer, _, current) = register(&link, &scope, true).await;
            drop((old_reader, old_writer));
            old.await.unwrap().unwrap();
            assert!(link.connected());
            let id = Uuid::new_v4();
            assert!(link.focus(
                id,
                PresentedRunKey {
                    host_id: scope.host.to_string(),
                    remote_epoch: Uuid::new_v4().to_string(),
                    agent_run_id: Uuid::new_v4().to_string()
                }
            ));
            reader.next_envelope().await.unwrap().unwrap();
            drop((reader, writer));
            current.await.unwrap().unwrap();
            let failure = completed.recv().await.unwrap();
            assert_eq!(failure.id, id);
            assert!(failure.failed);
            assert!(!link.connected());
        })
        .await
        .unwrap();
    }
}
