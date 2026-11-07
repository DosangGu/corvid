use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use corvid_companion::{PresentationAddress, PresentationClient};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetPresentationSlots, LocalActivation};
use eframe::egui;
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};
use uuid::Uuid;

use crate::model::Snapshot;

const REFRESH: Duration = Duration::from_millis(700);
const RETRY: Duration = Duration::from_secs(1);
const QUERY_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Link {
    #[default]
    Connecting,
    Online,
    Unavailable,
    Incompatible,
}

#[derive(Clone, Default)]
pub(crate) struct Display {
    pub(crate) link: Link,
    pub(crate) snapshot: Option<Arc<Snapshot>>,
    pub(crate) last_ready: Option<Instant>,
    pub(crate) completion: Option<Completion>,
}

impl Display {
    pub(crate) fn accepts(&self, command: &Command) -> bool {
        self.link == Link::Online
            && self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.link == command.link
                    && snapshot.page == command.page
                    && snapshot.generation() == Some(command.generation)
            })
    }
}

#[derive(Clone)]
pub(crate) struct Completion {
    pub(crate) id: Uuid,
    pub(crate) message: String,
    pub(crate) failed: bool,
}

pub(crate) struct Command {
    pub(crate) id: Uuid,
    pub(crate) link: Uuid,
    pub(crate) page: u32,
    pub(crate) generation: Uuid,
    pub(crate) body: Body,
}

pub(crate) struct DesktopClient {
    pub(crate) display: watch::Receiver<Display>,
    pub(crate) commands: mpsc::Sender<Command>,
    pub(crate) page: watch::Sender<u32>,
    _shutdown: oneshot::Sender<()>,
}

impl DesktopClient {
    pub(crate) fn start(
        address: <PresentationAddress as ToOwned>::Owned,
        ctx: &egui::Context,
    ) -> Result<Self> {
        let (display, view) = watch::channel(Display::default());
        let (commands, receive) = mpsc::channel(1);
        let (page, pages) = watch::channel(0);
        let (shutdown, stop) = oneshot::channel();
        let runtime = Builder::new_current_thread().enable_all().build()?;
        let mut worker = Worker {
            display,
            commands: receive,
            page: pages,
            ctx: ctx.clone(),
        };

        thread::Builder::new()
            .name("corvid-presentation".to_owned())
            .spawn(move || {
                runtime.block_on(async {
                    tokio::select! {
                        _ = worker.run(&address) => {}
                        _ = stop => {}
                    }
                });
            })?;
        Ok(Self {
            display: view,
            commands,
            page,
            _shutdown: shutdown,
        })
    }
}

struct Worker {
    display: watch::Sender<Display>,
    commands: mpsc::Receiver<Command>,
    page: watch::Receiver<u32>,
    ctx: egui::Context,
}

impl Worker {
    async fn run(&mut self, address: &PresentationAddress) {
        loop {
            let error = match PresentationClient::connect(address).await {
                Ok(client) => match self.follow(client).await {
                    Ok(()) => return,
                    Err(error) => error,
                },
                Err(error) => error,
            };
            let link = match error
                .downcast_ref::<ProtocolFault>()
                .map(|fault| fault.code.as_str())
            {
                Some("ProtocolMismatch" | "UnsupportedCapability") => Link::Incompatible,
                _ => Link::Unavailable,
            };
            self.publish(|display| display.link = link);
            sleep(RETRY).await;
        }
    }

    async fn follow(&mut self, mut client: PresentationClient) -> Result<()> {
        let link = Uuid::new_v4();
        let mut ticker = interval(REFRESH);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        self.refresh(&mut client, link).await?;
        ticker.tick().await;
        loop {
            let completion = tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { return Ok(()) };
                    if !self.display.borrow().accepts(&command) || *self.page.borrow() != command.page {
                        Some(Completion {
                            id: command.id,
                            message: "Display changed. Review the updated slots and try again.".to_owned(),
                            failed: true,
                        })
                    } else {
                        Some(self.execute(&mut client, command).await?)
                    }
                }
                change = self.page.changed() => {
                    if change.is_err() {
                        return Ok(());
                    }
                    None
                }
                _ = ticker.tick() => None,
            };
            let refreshed = self.refresh(&mut client, link).await;
            if let Some(completion) = completion {
                self.publish(|display| display.completion = Some(completion));
            }
            refreshed?;
        }
    }

    async fn refresh(&mut self, client: &mut PresentationClient, link: Uuid) -> Result<()> {
        let page = *self.page.borrow_and_update();
        let query = Body::GetPresentationSlots(GetPresentationSlots {
            count: None,
            overflow_page: page,
        });
        let reply = timeout(QUERY_TIMEOUT, client.request(query))
            .await
            .context("slot refresh timed out")??;
        let Body::PresentationSlots(slots) = reply else {
            bail!("unexpected slot response");
        };
        let snapshot = Arc::new(Snapshot::new(link, client.session(), page, slots)?);
        self.publish(|display| {
            display.link = Link::Online;
            if snapshot.generation().is_some() {
                display.last_ready = Some(Instant::now());
            } else if display.snapshot.as_ref().map(|previous| previous.host) != Some(snapshot.host)
            {
                display.last_ready = None;
            }
            display.snapshot = Some(snapshot);
        });
        Ok(())
    }

    async fn execute(
        &self,
        client: &mut PresentationClient,
        command: Command,
    ) -> Result<Completion> {
        let expected = Expected::for_request(&command.body)?;
        let result = match client.request(command.body).await {
            Ok(reply) => expected.summarize(reply).map(str::to_owned),
            Err(error) if error.is::<ProtocolFault>() => {
                let fault = error.downcast::<ProtocolFault>()?;
                return Ok(Completion {
                    id: command.id,
                    message: fault_message(&fault),
                    failed: true,
                });
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(message) => Ok(Completion {
                id: command.id,
                message,
                failed: false,
            }),
            Err(error) => {
                self.publish(|display| {
                    display.link = Link::Unavailable;
                    display.completion = Some(Completion {
                        id: command.id,
                        message: "Action result unavailable. Check the refreshed slots before trying again.".to_owned(),
                        failed: true,
                    });
                });
                Err(error)
            }
        }
    }

    fn publish(&self, update: impl FnOnce(&mut Display)) {
        self.display.send_modify(update);
        self.ctx.request_repaint();
    }
}

enum Expected {
    Focus { client: String, run: String },
    Acknowledge { run: String, attention: String },
    Edit,
}

impl Expected {
    fn for_request(body: &Body) -> Result<Self> {
        Ok(match body {
            Body::FocusSlot(focus) => Self::Focus {
                client: focus
                    .binding
                    .as_ref()
                    .context("missing selected binding")?
                    .client_id
                    .clone(),
                run: focus
                    .slot
                    .as_ref()
                    .and_then(|slot| slot.run.as_ref())
                    .context("missing selected run")?
                    .agent_run_id
                    .clone(),
            },
            Body::AckPresentedAttention(ack) => Self::Acknowledge {
                run: ack
                    .run
                    .as_ref()
                    .context("missing acknowledged run")?
                    .agent_run_id
                    .clone(),
                attention: ack.attention_id.clone(),
            },
            Body::EditSlotLayout(_) => Self::Edit,
            _ => bail!("unsupported desktop action"),
        })
    }

    fn summarize(self, reply: Body) -> Result<&'static str> {
        Ok(match (self, reply) {
            (Self::Focus { client, run }, Body::SlotFocused(result)) => {
                let remote = result.remote.context("missing remote selection")?;
                ensure!(
                    remote.client_id == client && remote.agent_run_id == run,
                    "focus result target mismatch"
                );
                match LocalActivation::try_from(result.local_activation) {
                    Ok(LocalActivation::Activated) => {
                        "Selected run and brought the registered window forward."
                    }
                    Ok(LocalActivation::PaneSelectedForegroundUnknown) => {
                        "Selected run and pane. Bring the registered window forward."
                    }
                    Ok(LocalActivation::ManualRequired) => {
                        "Selected run. Switch to the registered terminal."
                    }
                    Ok(LocalActivation::Unavailable) => {
                        "Selected run. Local terminal activation was unavailable."
                    }
                    _ => "Selected run. Local activation could not be confirmed.",
                }
            }
            (Self::Acknowledge { run, attention }, Body::AttentionAcknowledged(result))
                if result.agent_run_id == run && result.attention_id == attention =>
            {
                "Alert acknowledged. Handle any pending request in the terminal."
            }
            (Self::Edit, Body::SlotLayoutEdited(_)) => "Slot layout saved.",
            _ => bail!("unexpected desktop action result"),
        })
    }
}

fn fault_message(fault: &ProtocolFault) -> String {
    match fault.code.as_str() {
        "StaleBinding" | "TargetGone" => {
            "The selected slot or registration changed. Review the refreshed slots.".to_owned()
        }
        "LocalTerminalUnavailable" => {
            "The registered WezTerm pane is unavailable. Pair the intended terminal again."
                .to_owned()
        }
        "NoAttachedClient" | "AmbiguousClient" => "Choose an attached, paired terminal.".to_owned(),
        "ConnectionRequired" | "Offline" | "EpochMismatch" => {
            "Wait for the companion to synchronize.".to_owned()
        }
        "WriterRequired" | "WorkspaceBusy" => {
            "The chosen terminal must already hold the workspace writer lease.".to_owned()
        }
        "InputBusy" => "Wait for the active paste to finish before switching.".to_owned(),
        "OutcomeUnknown" => {
            "Action result unavailable. Check the refreshed slots before trying again.".to_owned()
        }
        _ => format!("Companion rejected the action ({})", fault.code),
    }
}

#[cfg(test)]
mod tests {
    use crate::model::tests::ready_snapshot;

    use super::*;

    #[test]
    fn rejects_queued_actions_after_page_change_or_reconnect() {
        let mut snapshot = ready_snapshot();
        let command = Command {
            id: Uuid::new_v4(),
            link: snapshot.link,
            page: snapshot.page,
            generation: snapshot.generation().unwrap(),
            body: snapshot
                .focus(&snapshot.data.slots[0], &snapshot.data.bindings[0])
                .unwrap(),
        };
        let mut display = Display {
            link: Link::Online,
            ..Default::default()
        };
        snapshot.page = 1;
        display.snapshot = Some(Arc::new(snapshot));
        assert!(!display.accepts(&command));

        let current = Arc::get_mut(display.snapshot.as_mut().unwrap()).unwrap();
        current.page = 0;
        assert!(display.accepts(&command));

        let current = Arc::get_mut(display.snapshot.as_mut().unwrap()).unwrap();
        current.link = Uuid::new_v4();
        assert!(!display.accepts(&command));
    }
}
