use std::env;
use std::future::Future;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use corvid_companion::{HostEndpoint, MAX_SLOTS, PresentationClient};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::edit_slot_layout::Action;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AckPresentedAttention, AgentBadge, AgentCollection, CompanionConnection, EditSlotLayout,
    EvidenceConfidence, FocusSlot, GetPresentationSlots, LayoutRecovery, LocalActivation,
    PairTerminal, PinSlot, PlaceSlot, PresentationSlots, PresentedBinding, PresentedRun,
    PresentedRunKey, PresentedSlotToken, RunAvailability, UnpairTerminal,
};
#[cfg(target_os = "linux")]
use tokio::signal::unix::{SignalKind, signal};
use uuid::Uuid;

use super::read_pairing_code;
#[cfg(target_os = "linux")]
use crate::{linux::state_path, runtime_path};

#[derive(Args)]
pub(super) struct Options {
    /// Override the private local companion socket directory.
    #[cfg(target_os = "linux")]
    #[arg(long, global = true)]
    socket_dir: Option<PathBuf>,
    /// Choose a named-pipe instance under the current Windows user.
    #[cfg(windows)]
    #[arg(long, global = true, default_value = "main")]
    pipe_instance: String,
    /// Override the private companion metadata directory.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure one expected daemon host and allocate a persistent controller ID.
    Init {
        #[arg(long)]
        host_id: Uuid,
        /// Use an OpenSSH alias for the execution host.
        #[arg(long, required = cfg!(windows))]
        ssh_alias: Option<String>,
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_SLOTS)))]
        count: u32,
    },
    /// Run the local companion and resynchronize the configured host on disconnect.
    Run,
    /// Show slots, captured slot revisions, and available TUI registrations.
    Slots {
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_SLOTS)))]
        count: Option<u32>,
        #[arg(long, default_value_t = 0)]
        overflow_page: u32,
    },
    /// Read the selected TUI's private single-use pairing code from stdin.
    Pair,
    /// Remove an explicit TUI registration.
    Unpair {
        #[arg(long)]
        binding: String,
    },
    /// Select the run in a captured slot and explicitly chosen paired TUI.
    FocusSlot {
        /// The capture value printed for the slot.
        #[arg(long)]
        slot: String,
        /// The capture value printed for the intended client binding.
        #[arg(long)]
        binding: String,
    },
    /// Acknowledge one exact attention record.
    Ack {
        #[arg(long)]
        control: String,
        #[arg(long)]
        run: String,
        #[arg(long)]
        attention_id: Uuid,
    },
    /// Keep the slot reserved when its run ends or is removed.
    Pin {
        #[arg(long)]
        slot: String,
    },
    /// Allow an empty slot to be assigned automatically.
    Unpin {
        #[arg(long)]
        slot: String,
    },
    /// Remove a run from a captured slot and suppress automatic placement.
    Remove {
        #[arg(long)]
        slot: String,
    },
    /// Remove an exact run from the overflow list.
    RemoveOverflow {
        #[arg(long)]
        run: String,
    },
    /// Allow a suppressed current run to be placed again.
    Readd {
        #[arg(long)]
        run: String,
    },
    /// Place a current overflow run in an explicitly selected empty slot.
    Place {
        #[arg(long)]
        slot: String,
        #[arg(long)]
        run: String,
    },
}

pub(super) async fn execute(options: Options, daemon_runtime: Option<PathBuf>) -> Result<()> {
    if let Command::Init {
        host_id,
        ssh_alias,
        count,
    } = options.command
    {
        let endpoint = host_endpoint(ssh_alias, daemon_runtime)?;
        let controller = corvid_companion::initialize(
            &companion_state(options.state_dir)?,
            host_id,
            endpoint,
            count,
        )?;
        println!("controller_id={controller}");
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    let address = companion_socket(options.socket_dir)?;
    #[cfg(windows)]
    let address = options.pipe_instance;
    if matches!(options.command, Command::Run) {
        return corvid_companion::run(&address, &companion_state(options.state_dir)?, shutdown()?)
            .await;
    }

    let mut client = PresentationClient::connect(&address).await?;
    let request = options.command.request(&client.session().server_epoch)?;
    let reply = client.request(request).await?;
    show(
        reply,
        &client.session().host_id,
        &client.session().server_epoch,
    )
}

fn host_endpoint(alias: Option<String>, _daemon_runtime: Option<PathBuf>) -> Result<HostEndpoint> {
    if let Some(alias) = alias {
        return Ok(HostEndpoint::Ssh { alias });
    }
    #[cfg(target_os = "linux")]
    return Ok(HostEndpoint::Local {
        runtime_dir: runtime_path(_daemon_runtime)?,
    });
    #[cfg(windows)]
    bail!("Windows companion init requires --ssh-alias");
}

fn companion_state(path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    #[cfg(target_os = "linux")]
    return Ok(state_path(None)?.join("companion"));
    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .context("set LOCALAPPDATA or companion --state-dir")?;
        Ok(PathBuf::from(base).join(r"Corvid\companion"))
    }
}

#[cfg(target_os = "linux")]
fn companion_socket(path: Option<PathBuf>) -> Result<PathBuf> {
    path.map_or_else(
        || {
            let base = env::var_os("XDG_RUNTIME_DIR")
                .filter(|path| !path.is_empty())
                .context("set XDG_RUNTIME_DIR or companion --socket-dir")?;
            Ok(PathBuf::from(base).join("corvid-companion"))
        },
        Ok,
    )
}

#[cfg(target_os = "linux")]
fn shutdown() -> Result<impl Future<Output = ()>> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
}

#[cfg(windows)]
fn shutdown() -> Result<impl Future<Output = ()>> {
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    let mut break_signal = tokio::signal::windows::ctrl_break()?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = break_signal.recv() => {}
        }
    })
}

impl Command {
    fn request(self, epoch: &str) -> Result<Body> {
        let body = match self {
            Self::Init { .. } | Self::Run => unreachable!("handled by companion execute"),
            Self::Slots {
                count,
                overflow_page,
            } => Body::GetPresentationSlots(GetPresentationSlots {
                count,
                overflow_page,
            }),
            Self::Pair => Body::PairTerminal(PairTerminal {
                code: read_pairing_code()?.to_string(),
            }),
            Self::Unpair { binding } => {
                let (generation, binding) = parse_binding(&binding, epoch)?;
                Body::UnpairTerminal(UnpairTerminal {
                    control_generation: generation,
                    binding: Some(binding),
                })
            }
            Self::FocusSlot { slot, binding } => {
                let (generation, binding) = parse_binding(&binding, epoch)?;
                Body::FocusSlot(FocusSlot {
                    control_generation: generation,
                    binding: Some(binding),
                    slot: Some(parse_slot(&slot, epoch)?),
                })
            }
            Self::Ack {
                control,
                run,
                attention_id,
            } => {
                let parts: Vec<_> = control.split('/').collect();
                let [companion_epoch, generation] = parts.as_slice() else {
                    bail!("invalid control capture")
                };
                require_epoch(companion_epoch, epoch)?;
                Body::AckPresentedAttention(AckPresentedAttention {
                    control_generation: uuid(generation)?,
                    run: Some(parse_run(&run)?),
                    attention_id: attention_id.to_string(),
                })
            }
            Self::Pin { ref slot } | Self::Unpin { ref slot } => {
                let pinned = matches!(self, Self::Pin { .. });
                edit(Action::Pin(PinSlot {
                    slot: Some(parse_slot(slot, epoch)?),
                    pinned: Some(pinned),
                }))
            }
            Self::Remove { slot } => edit(Action::Remove(parse_slot(&slot, epoch)?)),
            Self::RemoveOverflow { run } => edit(Action::RemoveOverflow(parse_run(&run)?)),
            Self::Readd { run } => edit(Action::Readd(parse_run(&run)?)),
            Self::Place { slot, run } => edit(Action::Place(PlaceSlot {
                slot: Some(parse_slot(&slot, epoch)?),
                run: Some(parse_run(&run)?),
            })),
        };
        Ok(body)
    }
}

fn edit(action: Action) -> Body {
    Body::EditSlotLayout(EditSlotLayout {
        action: Some(action),
    })
}

fn parse_slot(token: &str, epoch: &str) -> Result<PresentedSlotToken> {
    let parts: Vec<_> = token.splitn(4, '/').collect();
    let [companion_epoch, slot, revision, run] = parts.as_slice() else {
        bail!("invalid slot capture")
    };
    require_epoch(companion_epoch, epoch)?;
    Ok(PresentedSlotToken {
        slot_id: slot.parse().context("invalid slot ID")?,
        revision: Some(revision.parse().context("invalid slot revision")?),
        run: (*run != "-").then(|| parse_run(run)).transpose()?,
    })
}

fn parse_binding(token: &str, epoch: &str) -> Result<(String, PresentedBinding)> {
    let parts: Vec<_> = token.splitn(5, '/').collect();
    let [companion_epoch, generation, client, revision] = parts.as_slice() else {
        bail!("invalid binding capture")
    };
    require_epoch(companion_epoch, epoch)?;
    Ok((
        uuid(generation)?,
        PresentedBinding {
            client_id: uuid(client)?,
            revision: revision.parse().context("invalid binding revision")?,
        },
    ))
}

fn parse_run(token: &str) -> Result<PresentedRunKey> {
    let parts: Vec<_> = token.splitn(4, '/').collect();
    let [host, epoch, run] = parts.as_slice() else {
        bail!("invalid run key")
    };
    Ok(PresentedRunKey {
        host_id: uuid(host)?,
        remote_epoch: uuid(epoch)?,
        agent_run_id: uuid(run)?,
    })
}

fn uuid(value: &str) -> Result<String> {
    Ok(Uuid::parse_str(value)
        .context("invalid captured ID")?
        .to_string())
}

fn require_epoch(captured: &str, expected: &str) -> Result<()> {
    if uuid(captured)? != expected {
        return Err(
            ProtocolFault::new("StaleBinding", "companion restarted; refresh slots").into(),
        );
    }
    Ok(())
}

fn show(reply: Body, host: &str, epoch: &str) -> Result<()> {
    match reply {
        Body::PresentationSlots(slots) => show_slots(slots, host, epoch)?,
        Body::ClientBindingRegistered(binding) => println!(
            "client_id={} binding_revision={} commit_sequence={}",
            binding.client_id, binding.binding_revision, binding.commit_sequence
        ),
        Body::ClientBindingUnregistered(unbound) => {
            println!("commit_sequence={}", unbound.commit_sequence)
        }
        Body::AttentionAcknowledged(ack) => println!(
            "run_id={} attention_id={} acknowledged_unix_ms={} commit_sequence={}",
            ack.agent_run_id, ack.attention_id, ack.acknowledged_unix_ms, ack.commit_sequence
        ),
        Body::SlotLayoutEdited(_) => println!("layout_updated=true"),
        Body::SlotFocused(focused) => {
            let applied = focused.remote.context("missing focus result")?;
            let activation = LocalActivation::try_from(focused.local_activation)?;
            ensure!(
                activation != LocalActivation::Unspecified,
                "missing local activation status"
            );
            println!(
                "remote_selection=Applied local_activation={activation:?} client_id={} run_id={} terminal_id={} view_revision={} commit_sequence={}",
                applied.client_id,
                applied.agent_run_id,
                applied.terminal_id,
                applied.view_revision,
                applied.commit_sequence
            );
        }
        _ => bail!("unexpected companion response"),
    }
    Ok(())
}

fn show_slots(slots: PresentationSlots, host: &str, epoch: &str) -> Result<()> {
    let connection = CompanionConnection::try_from(slots.connection)?;
    let recovery = LayoutRecovery::try_from(slots.layout_recovery)?;
    print!(
        "connection={connection:?} host_id={host} controller_id={} companion_epoch={epoch} layout_recovery={recovery:?}",
        slots.controller_id
    );
    if let Some(issue) = slots.issue_code {
        print!(" issue={issue}");
    }
    if let Some(remote_epoch) = slots.remote_epoch {
        print!(" remote_epoch={remote_epoch}");
    }
    if let Some(sequence) = slots.remote_sequence {
        print!(" remote_sequence={sequence}");
    }
    if let Some(generation) = &slots.control_generation {
        print!(" control_capture={epoch}/{generation}");
    }
    println!();
    for slot in slots.slots {
        let key = slot
            .run
            .as_ref()
            .and_then(|run| run.key.as_ref())
            .map_or_else(|| "-".to_owned(), run_key);
        print!(
            "slot={} revision={} pinned={} capture={epoch}/{}/{}/{key}",
            slot.slot_id, slot.revision, slot.pinned, slot.slot_id, slot.revision
        );
        if let Some(run) = slot.run {
            show_run(&run)?;
        } else {
            print!(" empty=true");
        }
        println!();
    }
    for run in slots.overflow {
        print!("overflow");
        show_run(&run)?;
        println!();
    }
    for run in slots.recent_ended {
        println!("ended run={}", run_key(&run));
    }
    for run in slots.suppressed {
        println!("suppressed run={}", run_key(&run));
    }
    if let Some(generation) = &slots.control_generation {
        for binding in slots.bindings {
            println!(
                "binding client_id={} revision={} capture={epoch}/{generation}/{}/{}",
                binding.client_id, binding.revision, binding.client_id, binding.revision
            );
        }
    }
    println!(
        "overflow_count={} suppressed_count={}",
        slots.overflow_count, slots.suppressed_count
    );
    Ok(())
}

fn show_run(run: &PresentedRun) -> Result<()> {
    let key = run.key.as_ref().context("missing presented run identity")?;
    let availability = RunAvailability::try_from(run.availability)?;
    let badge = AgentBadge::try_from(run.last_badge)?;
    let collection = AgentCollection::try_from(run.collection)?;
    let confidence = EvidenceConfidence::try_from(run.confidence)?;
    print!(
        " run={} availability={availability:?} last_badge={badge:?} collection={collection:?} evidence={confidence:?} pending={} unverified={} unseen={}",
        run_key(key),
        run.pending_requests,
        run.unverified_requests,
        run.unseen_attention
    );
    if let Some(alert) = &run.latest_unseen_attention_id {
        print!(" latest_attention_id={alert}");
    }
    Ok(())
}

fn run_key(key: &PresentedRunKey) -> String {
    format!("{}/{}/{}", key.host_id, key.remote_epoch, key.agent_run_id)
}
