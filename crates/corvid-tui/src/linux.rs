pub(crate) mod shell_agent;
mod start;

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::{Subcommand, ValueEnum};
use corvid_client::{ControlEndpoint, ControllerRequest, ControllerScope};
use corvid_protocol::ProtocolFault;
use corvid_protocol::snapshot::{HISTORY_CAPABILITY, SynchronizedSnapshot};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentActivity, AgentBadge, AgentCollection, AgentEndReason, AgentInfo, AgentLifecycle,
    AgentProcessState, AgentProvider, AttentionReason, CollectorStatus, HelloResult, HookEvent,
    InputRequestKind, InputRequestStatus, LayoutRecovery, PersistenceState, ProviderCapabilityKind,
    ProviderCorrelation, ProviderExecutionMode, ProviderQualification, TuiInputMode,
};
use tokio::signal::unix::{SignalKind, signal};
use uuid::Uuid;

use crate::hooks::HookProvider;
use crate::runtime_path;

use crate::{hooks, read_pairing_code};

pub(super) use start::{ensure, start};

#[derive(Subcommand)]
pub(super) enum Command {
    /// Run the daemon in the foreground.
    Daemon {
        /// Override the private directory containing daemon metadata.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        autostarted: bool,
    },
    /// Start the background daemon if needed without attaching a terminal.
    EnsureDaemon,
    /// Replace an idle background daemon with this build.
    Upgrade,
    /// Attach to the live terminal, or start one explicitly.
    Attach {
        /// Start a new terminal with the user's shell.
        #[arg(long)]
        new: bool,
        /// Start the named provider; arguments after -- go to its CLI.
        #[arg(long, value_enum)]
        agent: Option<ManagedAgent>,
        /// Attach to a workspace by ID.
        #[arg(long, conflicts_with = "new_workspace")]
        workspace: Option<String>,
        /// Attach to a tab by ID within the workspace.
        #[arg(long, conflicts_with_all = ["new_workspace", "new_tab"])]
        tab: Option<String>,
        /// Attach to an exact pane within the selected workspace and tab.
        #[arg(long, requires_all = ["workspace", "tab"])]
        pane: Option<String>,
        /// Create a workspace with this label and open a terminal.
        #[arg(long)]
        new_workspace: Option<String>,
        /// Create a tab with this label and open a terminal.
        #[arg(long)]
        new_tab: Option<String>,
        /// Explicitly claim an additional unowned workspace for this TUI.
        #[arg(long, value_name = "WORKSPACE_ID")]
        claim_workspace: Vec<Uuid>,
        /// Move a hidden live tab from the attached workspace.
        #[arg(
            long,
            value_name = "TAB_ID",
            requires_all = ["to_workspace", "claim_workspace"],
            conflicts_with_all = ["new", "agent", "new_workspace", "new_tab", "argv"]
        )]
        move_tab: Option<Uuid>,
        /// Destination for --move-tab. Claim it with --claim-workspace.
        #[arg(long, value_name = "WORKSPACE_ID", requires = "move_tab")]
        to_workspace: Option<Uuid>,
        /// Start the shell in the last recorded directory if it still exists.
        #[arg(long, requires = "new")]
        restore_cwd: bool,
        /// Milliseconds to wait for a fragmented escape sequence.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=2000))]
        escape_delay_ms: u64,
        /// Program and arguments for an explicitly started terminal.
        #[arg(last = true)]
        argv: Vec<OsString>,
    },
    /// Send a read-only command or relay the control protocol over stdio.
    Control {
        #[arg(long, conflicts_with = "request")]
        stdio: bool,
        #[arg(value_enum)]
        request: Option<ControlRequest>,
    },
    /// Relay the interactive writer protocol over a non-PTY SSH connection.
    TerminalRelay,
    /// Manage an explicit controller registration or acknowledge an alert.
    Controller {
        #[arg(long)]
        controller_id: String,
        #[arg(long)]
        host_id: String,
        #[arg(long)]
        server_epoch: String,
        /// Reuse this ID only to obtain the result of the same command.
        #[arg(long)]
        request_id: Option<String>,
        #[command(subcommand)]
        action: ControllerAction,
    },
    /// Print an additive provider hook configuration for explicit installation.
    Hooks {
        #[arg(value_enum)]
        provider: HookProvider,
    },
    /// Submit one provider observation from stdin.
    Observe {
        #[arg(long, value_enum)]
        provider: HookProvider,
        /// Emit a fixed empty object for Codex hooks that expect JSON.
        #[arg(long, hide = true)]
        neutral_json: bool,
        /// Read the optional Claude input-batch hook.
        #[arg(long, hide = true, conflicts_with = "neutral_json")]
        claude_input_batch: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ControlRequest {
    Host,
    /// Show control features and release-qualified provider profiles.
    Capabilities,
    Ping,
    Status,
    /// Show live run coverage and collector traffic for the daemon epoch.
    CollectorStatus,
    Workspaces,
    Agents,
    /// Show recorded runs and requests from earlier daemon epochs.
    History,
    Attention,
    Clients,
    Snapshot,
    Watch,
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ManagedAgent {
    Claude,
    Codex,
}

#[derive(Subcommand)]
pub(super) enum ControllerAction {
    /// Read the attached TUI's single-use pairing code from stdin.
    Pair,
    Unpair {
        #[arg(long)]
        client_id: Uuid,
        #[arg(long)]
        binding_revision: u64,
    },
    Ack {
        #[arg(long)]
        run_id: Uuid,
        #[arg(long)]
        attention_id: Uuid,
    },
    /// Select the exact managed agent in an existing registered client.
    Focus {
        #[arg(long)]
        client_id: Uuid,
        #[arg(long)]
        binding_revision: u64,
        #[arg(long)]
        run_id: Uuid,
    },
}

pub(super) fn execute(
    command: Command,
    runtime_dir: Option<PathBuf>,
    started: Instant,
) -> Result<u32> {
    match command {
        Command::Observe {
            provider,
            neutral_json,
            claude_input_batch,
        } => {
            hooks::observe(
                provider,
                neutral_json,
                claude_input_batch,
                runtime_dir,
                started,
            );
            Ok(0)
        }
        Command::Hooks { provider } => hooks::settings(provider).map(|settings| {
            println!("{settings:#}");
            0
        }),
        command => crate::execute(run(command, runtime_dir)),
    }
}

async fn run(command: Command, runtime_dir: Option<PathBuf>) -> Result<u32> {
    let runtime = runtime_path(runtime_dir)?;
    match command {
        Command::Hooks { .. } | Command::Observe { .. } => {
            unreachable!("handled before daemon runtime")
        }
        Command::Daemon {
            state_dir,
            autostarted,
        } => {
            let state = state_path(state_dir)?;
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            corvid_daemon::run(&runtime, &state, autostarted, async move {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                }
            })
            .await
            .map(|()| 0)
        }
        Command::EnsureDaemon => start::ensure(&runtime).await.map(|()| 0),
        Command::Upgrade => start::upgrade(&runtime).await.map(|()| 0),
        Command::Attach {
            new,
            agent,
            restore_cwd,
            escape_delay_ms,
            argv,
            workspace,
            tab,
            pane,
            new_workspace,
            new_tab,
            claim_workspace,
            move_tab,
            to_workspace,
        } => {
            let launch = (new
                || agent.is_some()
                || !argv.is_empty()
                || new_workspace.is_some()
                || new_tab.is_some())
            .then_some(argv);
            let options = corvid_client::AttachOptions {
                launch,
                launch_if_empty: false,
                agent: agent.map(|agent| match agent {
                    ManagedAgent::Claude => AgentProvider::Claude,
                    ManagedAgent::Codex => AgentProvider::Codex,
                }),
                restore_cwd,
                escape_delay: Duration::from_millis(escape_delay_ms),
                workspace,
                tab,
                pane,
                new_workspace,
                new_tab,
                claim_workspaces: claim_workspace,
                move_tab: move_tab.zip(to_workspace),
            };
            corvid_client::attach(&runtime, options).await
        }
        Command::Controller {
            controller_id,
            host_id,
            server_epoch,
            request_id,
            action,
        } => {
            let scope = ControllerScope {
                controller_id,
                host_id,
                server_epoch,
            };
            controller_action(&runtime, &scope, request_id, action).await
        }
        Command::Control { stdio: true, .. } => corvid_client::relay(&runtime).await.map(|()| 0),
        Command::TerminalRelay => corvid_client::relay_tui(&runtime).await.map(|()| 0),
        Command::Control { request, .. } => match request.unwrap_or(ControlRequest::Host) {
            ControlRequest::Host => {
                let host = corvid_client::host(&runtime).await?;
                println!(
                    "host_id={}\nserver_epoch={}",
                    host.host_id, host.server_epoch
                );
                Ok(0)
            }
            ControlRequest::Ping => {
                corvid_client::ping(&runtime).await?;
                println!("pong");
                Ok(0)
            }
            ControlRequest::Capabilities => {
                let capabilities = ControlEndpoint::Local(runtime).capabilities().await?;
                print_capabilities(capabilities)?;
                Ok(0)
            }
            ControlRequest::Status => {
                let status = corvid_client::status(&runtime).await?;
                let persistence = match PersistenceState::try_from(status.persistence) {
                    Ok(PersistenceState::Saved) => "saved",
                    Ok(PersistenceState::Pending) => "pending",
                    Ok(PersistenceState::Failed) => "failed",
                    _ => bail!("unknown persistence status"),
                };
                let recovery = match LayoutRecovery::try_from(status.layout_recovery) {
                    Ok(LayoutRecovery::Normal) => "normal",
                    Ok(LayoutRecovery::Backup) => "backup",
                    Ok(LayoutRecovery::Fresh) => "fresh",
                    _ => bail!("unknown layout recovery state"),
                };
                println!(
                    "terminal_live={}\npersistence={persistence}\nlayout_recovery={recovery}",
                    status.terminal_live
                );
                Ok(0)
            }
            ControlRequest::CollectorStatus => {
                let status = corvid_client::status(&runtime).await?;
                let collector = status.collector.ok_or_else(|| {
                    ProtocolFault::new(
                        "UnsupportedCapability",
                        "daemon does not report collector status",
                    )
                })?;
                print_collector_status(collector);
                Ok(0)
            }
            ControlRequest::Workspaces => {
                let list = corvid_client::workspaces(&runtime).await?;
                for workspace in list.workspaces {
                    println!("workspace {} {}", workspace.workspace_id, workspace.label);
                    for tab in workspace.tabs {
                        println!("  tab {} {}", tab.tab_id, tab.label);
                        for pane in tab.pane_ids {
                            println!("    pane {pane}");
                        }
                    }
                }
                Ok(0)
            }
            ControlRequest::Agents => {
                let list = corvid_client::agents(&runtime).await?;
                for agent in list.agents {
                    print_agent(agent);
                }
                Ok(0)
            }
            ControlRequest::History => {
                print_history(&corvid_client::snapshot(&runtime).await?)?;
                Ok(0)
            }
            ControlRequest::Attention => {
                let cache = corvid_client::snapshot(&runtime).await?;
                print_attention(&cache);
                Ok(0)
            }
            ControlRequest::Clients => {
                for client in corvid_client::clients(&runtime).await?.clients {
                    print!(
                        "{} generation={}",
                        client.client_id, client.connection_generation
                    );
                    if let Some(terminal) = client.terminal_id {
                        print!(" terminal={terminal}");
                    }
                    if let Some(revision) = client.view_revision {
                        print!(" view_revision={revision}");
                    }
                    if let Some(mode) = client.input_mode {
                        let mode = TuiInputMode::try_from(mode)?;
                        print!(" mode={mode:?}");
                    }
                    if let Some(keymap) = client.keymap {
                        print!(
                            " keymap_tabs={} keymap_pairing={}",
                            keymap.tabs, keymap.pairing
                        );
                    }
                    for lease in client.writer_leases {
                        print!(" writer={}/{}", lease.workspace_id, lease.generation);
                    }
                    for binding in client.bindings {
                        print!(
                            " binding={}/{}",
                            binding.controller_id, binding.binding_revision
                        );
                    }
                    println!();
                }
                Ok(0)
            }
            ControlRequest::Snapshot => {
                print_snapshot(&corvid_client::snapshot(&runtime).await?);
                Ok(0)
            }
            ControlRequest::Watch => {
                let mut watch = corvid_client::watch_snapshot(&runtime).await?;
                print_snapshot(watch.state());
                loop {
                    watch.next_change().await?;
                    print_snapshot(watch.state());
                }
            }
        },
    }
}

fn print_collector_status(collector: CollectorStatus) {
    println!("healthy_runs={}", collector.healthy_runs);
    println!("not_registered_runs={}", collector.not_registered_runs);
    println!("degraded_runs={}", collector.degraded_runs);
    println!("gap_detected_runs={}", collector.gap_detected_runs);
    println!("unsupported_runs={}", collector.unsupported_runs);
    if let Some(traffic) = collector.traffic {
        println!("accepted_submissions={}", traffic.accepted_submissions);
        println!("rejected_submissions={}", traffic.rejected_submissions);
        println!("connection_failures={}", traffic.connection_failures);
        println!("saturated_connections={}", traffic.saturated_connections);
    }
}

fn print_capabilities(description: HelloResult) -> Result<()> {
    let limits = description
        .limits
        .context("capability response lacks limits")?;
    println!("writer_policy={}", description.writer_policy);
    println!("control_frame_bytes={}", limits.control_frame_bytes);
    println!("command_timeout_ms={}", limits.command_timeout_ms);
    for feature in description.capabilities {
        println!("capability={feature}");
    }

    for profile in description.provider_profiles {
        let adapter = profile
            .adapter
            .context("provider profile lacks an adapter")?;
        println!(
            "provider={} qualified_version={} adapter={} adapter_schema={}",
            enum_name::<AgentProvider>(profile.provider),
            profile.version,
            enum_name::<ProviderExecutionMode>(adapter.mode),
            adapter.schema_revision,
        );
        for event in adapter.qualified_events {
            println!("  event={}", enum_name::<HookEvent>(event));
        }
        for field in adapter.correlation {
            println!("  correlation={}", enum_name::<ProviderCorrelation>(field));
        }
        for capability in adapter.capabilities {
            println!(
                "  observation={} qualification={} reason={:?}",
                enum_name::<ProviderCapabilityKind>(capability.kind),
                enum_name::<ProviderQualification>(capability.qualification),
                capability.reason,
            );
        }
    }
    Ok(())
}

fn enum_name<T: TryFrom<i32> + std::fmt::Debug>(value: i32) -> String {
    match T::try_from(value) {
        Ok(value) => format!("{value:?}"),
        Err(_) => format!("Unknown({value})"),
    }
}

fn print_attention(cache: &SynchronizedSnapshot) {
    let requests: BTreeMap<_, _> = cache
        .snapshot()
        .entities()
        .filter_map(|entry| {
            let Some(EntityBody::Request(request)) = &entry.body else {
                return None;
            };
            Some((
                (request.agent_run_id.as_str(), request.request_id.as_str()),
                request,
            ))
        })
        .collect();
    for entry in cache.snapshot().entities() {
        let Some(EntityBody::Attention(alert)) = &entry.body else {
            continue;
        };
        let reason =
            AttentionReason::try_from(alert.reason).unwrap_or(AttentionReason::Unspecified);
        let request = alert
            .request_id
            .as_deref()
            .and_then(|id| requests.get(&(alert.agent_run_id.as_str(), id)));
        let kind = request
            .and_then(|request| InputRequestKind::try_from(request.kind).ok())
            .unwrap_or(InputRequestKind::Unspecified);
        print!(
            "run_id={} attention_id={} reason={reason:?} request_kind={kind:?} acknowledged={}",
            alert.agent_run_id,
            alert.attention_id,
            alert.acknowledged_unix_ms.is_some(),
        );
        if let Some(request) = request {
            print!(" request_id={}", request.request_id);
            if let Ok(status) = InputRequestStatus::try_from(request.status) {
                print!(" request_status={status:?}");
            } else {
                print!(" request_status=Unknown");
            }
        }
        println!();
    }
}

async fn controller_action(
    runtime: &std::path::Path,
    scope: &ControllerScope,
    request_id: Option<String>,
    action: ControllerAction,
) -> Result<u32> {
    let command = match action {
        ControllerAction::Pair => ControllerRequest::Register {
            code: read_pairing_code()?,
        },
        ControllerAction::Unpair {
            client_id,
            binding_revision,
        } => ControllerRequest::Unregister {
            client: client_id,
            revision: binding_revision,
        },
        ControllerAction::Ack {
            run_id,
            attention_id,
        } => ControllerRequest::Acknowledge {
            run: run_id,
            attention: attention_id,
        },
        ControllerAction::Focus {
            client_id,
            binding_revision,
            run_id,
        } => ControllerRequest::Focus {
            client: client_id,
            run: run_id,
            revision: binding_revision,
        },
    };
    let request_id = request_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    match corvid_client::controller(runtime, scope, request_id, command).await? {
        Body::ClientBindingRegistered(binding) => println!(
            "client_id={} binding_revision={} sequence={}",
            binding.client_id, binding.binding_revision, binding.commit_sequence,
        ),
        Body::ClientBindingUnregistered(binding) => {
            println!("sequence={}", binding.commit_sequence)
        }
        Body::AttentionAcknowledged(ack) => println!(
            "attention_id={} acknowledged_unix_ms={} sequence={}",
            ack.attention_id, ack.acknowledged_unix_ms, ack.commit_sequence,
        ),
        Body::FocusApplied(applied) => {
            let selection = applied
                .selection
                .context("focus response is missing a selection")?;
            println!(
                "client_id={} run_id={} terminal_id={} workspace_id={} tab_id={} pane_id={} view_revision={} sequence={}",
                applied.client_id,
                applied.agent_run_id,
                applied.terminal_id,
                selection.workspace_id,
                selection.tab_id,
                selection.pane_id,
                applied.view_revision,
                applied.commit_sequence,
            );
        }
        _ => bail!("unexpected controller response"),
    }
    Ok(0)
}

fn print_snapshot(cache: &SynchronizedSnapshot) {
    let mut workspaces = 0;
    let mut terminals = 0;
    let mut clients = 0;
    let mut agents = 0;
    let mut requests = 0;
    let mut attention = 0;
    for entry in cache.snapshot().entities() {
        match &entry.body {
            Some(EntityBody::Workspace(_)) => workspaces += 1,
            Some(EntityBody::Terminal(_)) => terminals += 1,
            Some(EntityBody::Client(_)) => clients += 1,
            Some(EntityBody::Agent(_)) => agents += 1,
            Some(EntityBody::Request(_)) => requests += 1,
            Some(EntityBody::Attention(_)) => attention += 1,
            None => {}
        }
    }
    println!(
        "host_id={} server_epoch={} sequence={} workspaces={workspaces} terminals={terminals} clients={clients} agents={agents} requests={requests} attention={attention}",
        cache.session().host_id,
        cache.session().server_epoch,
        cache.snapshot().sequence(),
    );
}

fn print_history(cache: &SynchronizedSnapshot) -> Result<()> {
    ensure!(
        cache
            .snapshot()
            .capabilities()
            .iter()
            .any(|item| item == HISTORY_CAPABILITY),
        "daemon does not expose historical runs"
    );
    for run in cache.snapshot().history() {
        let provider = AgentProvider::try_from(run.provider)?;
        let lifecycle = AgentLifecycle::try_from(run.last_lifecycle)?;
        let process = AgentProcessState::try_from(run.last_process)?;
        let activity = AgentActivity::try_from(run.last_activity)?;
        let evidence = run
            .last_evidence
            .as_ref()
            .context("historical evidence is missing")?;
        println!(
            "{} epoch={} run={} terminal={} provider={provider:?} state=Historical process=Unknown last_lifecycle={lifecycle:?} last_process={process:?} last_activity={activity:?} observed_unix_ms={}",
            cache.session().host_id,
            run.server_epoch,
            run.agent_run_id,
            run.terminal_id,
            evidence.observed_unix_ms,
        );
        for request in &run.requests {
            let kind = InputRequestKind::try_from(request.kind)?;
            let status = InputRequestStatus::try_from(request.status)?;
            println!(
                "  request={} kind={kind:?} status={status:?}",
                request.request_id
            );
        }
        for attention in &run.attention {
            let reason = AttentionReason::try_from(attention.reason)?;
            print!("  attention={} reason={reason:?}", attention.attention_id);
            if let Some(time) = attention.acknowledged_unix_ms {
                print!(" acknowledged_unix_ms={time}");
            }
            println!();
        }
    }
    Ok(())
}

fn print_agent(agent: AgentInfo) {
    let provider = AgentProvider::try_from(agent.provider).unwrap_or(AgentProvider::Unspecified);
    let process =
        AgentProcessState::try_from(agent.process).unwrap_or(AgentProcessState::Unspecified);
    let badge = AgentBadge::try_from(agent.badge).unwrap_or(AgentBadge::Unspecified);
    let collection =
        AgentCollection::try_from(agent.collection).unwrap_or(AgentCollection::Unspecified);
    let end = AgentEndReason::try_from(agent.end_reason).unwrap_or(AgentEndReason::Unspecified);
    print!(
        "{} terminal={} provider={provider:?} process={process:?} badge={badge:?} collection={collection:?} end={end:?} pending={} unverified={} resolved={} cancelled={} response={}",
        agent.agent_run_id,
        agent.terminal_id,
        agent.pending_requests,
        agent.unverified_requests,
        agent.resolved_requests,
        agent.cancelled_requests,
        agent.response_observed_unix_ms.is_some(),
    );
    if let Some(process) = agent.process_identity {
        print!(" pid={}", process.pid);
    }
    if let Some(session) = agent.provider_session_id {
        print!(" session={session}");
    }
    if let Some(parent) = agent.parent_run_id {
        print!(" parent_run={parent}");
    }
    if let Some(actor) = agent.actor_id {
        print!(" actor={actor}");
    }
    if agent.child_coverage_gap {
        print!(" child_coverage_gap=true");
    }
    if let Some(version) = agent.qualified_provider_version {
        print!(" version={version}");
    }
    if let Some(adapter) = agent.adapter {
        let mode = ProviderExecutionMode::try_from(adapter.mode)
            .unwrap_or(ProviderExecutionMode::Unspecified);
        print!(
            " adapter={mode:?} adapter_schema={}",
            adapter.schema_revision
        );
    }
    println!();
}

pub(super) fn state_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    if let Some(base) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(base).join("corvid"));
    }
    let home = env::var_os("HOME").context("set XDG_STATE_HOME or --state-dir")?;
    ensure!(!home.is_empty(), "set XDG_STATE_HOME or --state-dir");
    Ok(PathBuf::from(home).join(".local/state/corvid"))
}
