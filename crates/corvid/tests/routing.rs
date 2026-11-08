#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use corvid_client::{ControllerRequest, ControllerScope, controller};
use corvid_companion::{HostEndpoint, PresentationClient, initialize};
use corvid_protocol::controller::{FOCUS_CAPABILITY, PAIR_CAPABILITY};
use corvid_protocol::snapshot::{SNAPSHOT_CAPABILITY, WATCH_CAPABILITY};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentBadge, AgentEndReason, AgentInfo, AgentProcessState, AgentProvider, Attach, AttachRole,
    ClaimWorkspaceWriter, ClientKeymap, ClientState, CompanionConnection, EntityKind, Envelope,
    FocusAgentRequest, FocusApplied, FocusSlot, FrameApplied, GetPresentationSlots, GetSnapshot,
    InputChunk, IssuePairingCode, LaunchCommand, ListClients, MoveTab, PairTerminal, PasteBegin,
    PasteChunk, PasteEnd, Ping, PresentationSlots, PresentedSlotToken, ResizeViewport, RouteToken,
    SelectView, SnapshotWatch, TuiInputMode, ViewSelection, Viewport,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, CONTROL_LIMIT, FRAME_PREAMBLE, Frame, FrameError, FrameReader, FrameRole,
    FrameWriter, PeerRole, ProtocolFault, Session, check_hello_result, decode_envelope, hello,
};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

use common::{WAIT, command, daemon, start_daemon};

const PRESSURE_MOVES: usize = 48;
const PRESSURE_WATCHES: usize = 8;

struct Tui {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    session: Session,
    route: RouteToken,
    selection: ViewSelection,
}

impl Tui {
    async fn attach(runtime: &Path, argv: Option<Vec<Vec<u8>>>) -> Self {
        let launch = argv.map(|argv| LaunchCommand {
            argv,
            ..Default::default()
        });
        Self::attach_command(runtime, launch).await
    }

    async fn attach_command(runtime: &Path, launch: Option<LaunchCommand>) -> Self {
        Self::attach_target(
            runtime,
            Attach {
                launch,
                ..Default::default()
            },
        )
        .await
    }

    async fn attach_target(runtime: &Path, mut attach: Attach) -> Self {
        let directory = RuntimeDir::open(runtime).unwrap();
        let (read, write) = directory
            .connect(SocketRole::Client)
            .await
            .unwrap()
            .into_split();
        let mut writer = FrameWriter::start(write, FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader = FrameReader::start(read, FrameRole::TuiOutput)
            .await
            .unwrap();
        let greeting = hello("hello".into(), None, vec!["attach.v1".into()]);
        writer.envelope(&greeting).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let session = check_hello_result(&greeting, &reply, PeerRole::Tui).unwrap();
        attach.terminal_capabilities.push("legacy_vt.v1".into());
        let attach = Attach {
            viewport: Some(Viewport { cols: 80, rows: 24 }),
            requested_role: AttachRole::Writer as i32,
            ..attach
        };
        writer
            .envelope(&session.envelope(Some("attach".into()), Body::Attach(attach)))
            .await
            .unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let Some(Body::Attached(attached)) = reply.body else {
            panic!("expected Attached")
        };
        Self {
            reader,
            writer,
            session,
            route: attached.route_token.unwrap(),
            selection: attached.selection.unwrap(),
        }
    }

    async fn send(&mut self, body: Body) {
        self.writer
            .envelope(&self.session.envelope(Some("request".into()), body))
            .await
            .unwrap();
    }

    async fn select(&mut self, selection: &ViewSelection) {
        self.send(Body::SelectView(SelectView {
            expected_view_revision: Some(self.route.view_revision),
            workspace_id: selection.workspace_id.clone(),
            tab_id: selection.tab_id.clone(),
            pane_id: selection.pane_id.clone(),
        }))
        .await;
    }

    async fn move_tab(&mut self, tab: &str, destination: &str, output: &mut Vec<u8>) -> Body {
        self.send(Body::MoveTab(MoveTab {
            expected_view_revision: Some(self.route.view_revision),
            tab_id: tab.to_owned(),
            destination_workspace_id: destination.to_owned(),
        }))
        .await;
        self.next_body(output).await
    }

    async fn input(&mut self, route: RouteToken, sequence: u64, bytes: &[u8]) {
        self.send(Body::InputChunk(InputChunk {
            route_token: Some(route),
            input_sequence: sequence,
            data: bytes.to_vec(),
        }))
        .await;
    }

    async fn enter_command_mode(&mut self, sequence: u64, output: &mut Vec<u8>) {
        self.input(self.route.clone(), sequence, b"\x1d").await;
        self.send(Body::Ping(Ping {})).await;
        assert!(matches!(self.next_body(output).await, Body::Pong(_)));
    }

    async fn client_state(&self, runtime: &Path) -> ClientState {
        corvid_client::clients(runtime)
            .await
            .unwrap()
            .clients
            .into_iter()
            .find(|client| client.client_id == self.route.client_id)
            .expect("attached client in inventory")
    }

    async fn next_body(&mut self, output: &mut Vec<u8>) -> Body {
        loop {
            if let Some(body) = self.event(output).await {
                return body;
            }
        }
    }

    async fn moved(&mut self, output: &mut Vec<u8>, destination: &ViewSelection) {
        let Body::ViewChanged(view) = self.next_body(output).await else {
            panic!("expected view change")
        };
        assert_eq!(view.selection.as_ref(), Some(destination));
        self.route = view.route_token.unwrap();
        self.selection = view.selection.unwrap();
    }

    async fn event(&mut self, output: &mut Vec<u8>) -> Option<Body> {
        match self.reader.next_frame().await.unwrap().unwrap() {
            Frame::Render(render) => {
                output.extend(render.ansi);
                assert!(output.len() < 1024 * 1024);
                self.send(Body::FrameApplied(FrameApplied {
                    render_generation: render.header.render_generation,
                    frame_id: render.header.frame_id,
                }))
                .await;
                None
            }
            Frame::Structured(payload) => decode_envelope(&payload).unwrap().body,
        }
    }
}

async fn control(runtime: &Path, request: &str) -> String {
    let result = command(runtime)
        .args(["control", request])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

#[tokio::test]
async fn crash_restores_layout_and_cwd_with_a_new_explicit_terminal() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let workdir = temporary.path().join("recorded-directory");
    std::fs::create_dir(&workdir).unwrap();
    let mut first = daemon(&runtime, &state).await;
    timeout(WAIT, async {
        let before = control(&runtime, "workspaces").await;
        let command = LaunchCommand {
            argv: shell("printf 'OLD-TERMINAL\\n'; sleep 20"),
            cwd: Some(workdir.as_os_str().as_encoded_bytes().to_vec()),
            ..Default::default()
        };
        let mut tui = Tui::attach_command(&runtime, Some(command)).await;
        let mut output = Vec::new();
        wait_for_text(&mut tui, &mut output, "OLD-TERMINAL").await;
        loop {
            let status = control(&runtime, "status").await;
            assert!(status.contains("terminal_live=true"));
            if status.contains("persistence=saved") {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        let stored = std::fs::read_to_string(state.join("state.json")).unwrap();
        assert!(!stored.contains("OLD-TERMINAL"));
        assert!(!stored.contains(&tui.route.terminal_id));
        assert!(state.join("state.json.bak").is_file());
        first.kill().await.unwrap();

        let _second = daemon(&runtime, &state).await;
        assert_eq!(before, control(&runtime, "workspaces").await);
        assert!(
            control(&runtime, "status")
                .await
                .contains("terminal_live=false")
        );
        let argv = shell("printf 'RESTORED=%s\\n' \"$(basename \"$PWD\")\"; sleep 3");
        let mut reopened = Tui::attach(&runtime, Some(argv)).await;
        assert_ne!(reopened.route.terminal_id, tui.route.terminal_id);
        let mut output = Vec::new();
        wait_for_text(&mut reopened, &mut output, "RESTORED=recorded-directory").await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn layout_recovery_preserves_evidence_and_future_schema() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let primary = state.join("state.json");
    let backup = state.join("state.json.bak");
    let mut first = daemon(&runtime, &state).await;
    let original = control(&runtime, "workspaces").await;
    first.kill().await.unwrap();

    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&primary).unwrap()).unwrap();
    let legacy = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "layout": current["layout"],
    }))
    .unwrap();
    std::fs::write(&primary, &legacy).unwrap();
    let mut migrated = daemon(&runtime, &state).await;
    assert_eq!(control(&runtime, "workspaces").await, original);
    let updated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&primary).unwrap()).unwrap();
    assert_eq!(updated["schema_version"], 2);
    assert_eq!(updated["history"], serde_json::json!([]));
    assert_eq!(std::fs::read(&backup).unwrap(), legacy);
    migrated.kill().await.unwrap();

    std::fs::copy(&primary, &backup).unwrap();
    std::fs::write(&primary, b"{damaged").unwrap();

    let mut recovered = daemon(&runtime, &state).await;
    assert_eq!(control(&runtime, "workspaces").await, original);
    assert!(
        control(&runtime, "status")
            .await
            .contains("layout_recovery=backup")
    );
    assert_eq!(
        std::fs::read(state.join("state.json.corrupt")).unwrap(),
        b"{damaged"
    );
    recovered.kill().await.unwrap();

    std::fs::write(&primary, b"{bad").unwrap();
    std::fs::write(&backup, b"{bad-backup").unwrap();
    let mut fresh = daemon(&runtime, &state).await;
    assert_ne!(control(&runtime, "workspaces").await, original);
    assert_eq!(
        control(&runtime, "status").await,
        "terminal_live=false\npersistence=saved\nlayout_recovery=fresh\n"
    );
    fresh.kill().await.unwrap();

    let newer = br#"{"schema_version":99,"layout":{"future":true}}"#;
    std::fs::write(&primary, newer).unwrap();
    let refused = command(&runtime)
        .arg("daemon")
        .arg("--state-dir")
        .arg(&state)
        .output()
        .await
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unsupported layout schema 99"));
    assert_eq!(std::fs::read(&primary).unwrap(), newer);
}

fn shell(script: &str) -> Vec<Vec<u8>> {
    ["/bin/sh", "-c", script]
        .map(|arg| arg.as_bytes().to_vec())
        .to_vec()
}

fn echo_fixture(label: &str) -> LaunchCommand {
    let mut argv = shell(
        "stty -echo; printf '%s-READY\\n' \"$1\"; while IFS= read -r line; do printf '%s=%s\\n' \"$1\" \"$line\"; done",
    );
    argv.extend([b"echo-fixture".to_vec(), label.as_bytes().to_vec()]);
    LaunchCommand {
        argv,
        ..Default::default()
    }
}

#[tokio::test]
async fn managed_pty_runs_keep_distinct_identity_across_exit_and_relaunch() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let fixture = temporary.path().join("claude");
    std::fs::write(
        &fixture,
        "#!/bin/sh\nprintf '%s-READY\\n' \"$1\"\nIFS= read -r code\nexit \"$code\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut daemon_command = command(&runtime);
    daemon_command.env("PATH", temporary.path());
    let _daemon = start_daemon(daemon_command, &runtime, &temporary.path().join("state")).await;
    let launch = |name: &str| LaunchCommand {
        argv: vec![name.as_bytes().to_vec()],
        cwd: Some(temporary.path().as_os_str().as_encoded_bytes().to_vec()),
        agent_provider: Some(AgentProvider::Claude as i32),
    };

    timeout(WAIT, async {
        let mut first = Tui::attach_command(&runtime, Some(launch("ONE"))).await;
        let mut other = Tui::attach_target(
            &runtime,
            Attach {
                create_workspace: Some("Another".into()),
                launch: Some(launch("TWO")),
                ..Default::default()
            },
        )
        .await;
        let (mut first_output, mut other_output) = (Vec::new(), Vec::new());
        wait_for_text(&mut first, &mut first_output, "ONE-READY").await;
        wait_for_text(&mut other, &mut other_output, "TWO-READY").await;
        let one = wait_for_agent(&runtime, &first.route.terminal_id, |agent| {
            agent.process_identity.is_some()
        })
        .await;
        let two = wait_for_agent(&runtime, &other.route.terminal_id, |agent| {
            agent.process_identity.is_some()
        })
        .await;
        assert_ne!(one.agent_run_id, two.agent_run_id);
        assert_eq!(one.location.as_ref(), Some(&first.selection));
        assert_eq!(two.location.as_ref(), Some(&other.selection));
        assert_eq!(one.process, AgentProcessState::Running as i32);
        assert_eq!(one.badge, AgentBadge::Unknown as i32);
        let collector = corvid_client::status(&runtime)
            .await
            .unwrap()
            .collector
            .unwrap();
        assert_eq!(collector.unsupported_runs, 2);
        assert_eq!(collector.healthy_runs, 0);
        let one_process = one.process_identity.unwrap();
        let two_process = two.process_identity.unwrap();
        assert_ne!(one_process.pid, two_process.pid);
        assert_eq!(one_process.boot_id, two_process.boot_id);
        assert!(one_process.start_ticks > 0);
        assert!(one_process.foreground_group.is_some());

        let (_companion, mut companion) =
            start_companion(&runtime, temporary.path(), &first.session.host_id, 2).await;
        let placements = presented(&mut companion, |view| {
            view.connection == CompanionConnection::Ready as i32
        })
        .await;
        assert_eq!(slot_run(&placements, 0), Some(one.agent_run_id.as_str()));
        assert_eq!(slot_run(&placements, 1), Some(two.agent_run_id.as_str()));

        first.input(first.route.clone(), 1, b"7\n").await;
        assert!(matches!(
            first.next_body(&mut first_output).await,
            Body::TerminalExited(_)
        ));
        let exited = wait_for_agent(&runtime, &one.terminal_id, |agent| {
            !agent.terminal_available
        })
        .await;
        assert_eq!(exited.agent_run_id, one.agent_run_id);
        assert_eq!(exited.exit_code, Some(7));
        assert_eq!(exited.badge, AgentBadge::Ended as i32);
        let collector = corvid_client::status(&runtime)
            .await
            .unwrap()
            .collector
            .unwrap();
        assert_eq!(collector.unsupported_runs, 1);
        assert!(first.reader.next_frame().await.unwrap().is_none());
        drop(first);

        let replacement = Tui::attach_command(&runtime, Some(launch("NEW"))).await;
        let fresh = wait_for_agent(&runtime, &replacement.route.terminal_id, |agent| {
            agent.terminal_available
        })
        .await;
        assert_eq!(fresh.location, exited.location);
        assert_ne!(fresh.agent_run_id, exited.agent_run_id);
        assert_ne!(fresh.terminal_id, exited.terminal_id);
        let unchanged = wait_for_agent(&runtime, &two.terminal_id, |_| true).await;
        assert_eq!(unchanged.agent_run_id, two.agent_run_id);
        assert_eq!(unchanged.process, AgentProcessState::Running as i32);
        let changed = presented(&mut companion, |view| {
            slot_run(view, 0) == Some(fresh.agent_run_id.as_str())
        })
        .await;
        assert!(changed.slots[0].revision > placements.slots[0].revision);
        assert_eq!(changed.slots[1], placements.slots[1]);
        assert_eq!(changed.recent_ended[0].agent_run_id, one.agent_run_id);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn silent_foreground_and_exec_changes_cannot_retarget_a_managed_run() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let fixture = temporary.path().join("claude");
    std::fs::write(
        &fixture,
        r#"#!/bin/bash
[ "$1" = --version ] && exit 1
/bin/stty -echo
set -m
printf 'PROBE-READY\n'
IFS= read -r
/bin/cat >/dev/null
IFS= read -r
exec /bin/sleep 30
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut daemon_command = command(&runtime);
    daemon_command.env("PATH", temporary.path());
    let _daemon = start_daemon(daemon_command, &runtime, &temporary.path().join("state")).await;

    timeout(WAIT, async {
        let mut tui = Tui::attach_command(
            &runtime,
            Some(LaunchCommand {
                agent_provider: Some(AgentProvider::Claude as i32),
                ..Default::default()
            }),
        )
        .await;
        wait_for_text(&mut tui, &mut Vec::new(), "PROBE-READY").await;
        let terminal = tui.route.terminal_id.clone();
        let run = wait_for_agent(&runtime, &terminal, |agent| {
            agent.process_identity.as_ref().is_some_and(|identity| {
                identity.foreground_group == i32::try_from(identity.pid).ok()
            })
        })
        .await;
        let shell = run.process_identity.as_ref().unwrap();
        let scope = controller_scope(&tui, "process-probe");
        let client = Uuid::parse_str(&tui.route.client_id).unwrap();
        let code = issue_pairing(&mut tui).await;
        let reply = controller(
            &runtime,
            &scope,
            "bind-probe".into(),
            ControllerRequest::Register { code },
        )
        .await
        .unwrap();
        let Body::ClientBindingRegistered(binding) = reply else {
            panic!("missing binding");
        };
        let revision = binding.binding_revision;
        focus(&runtime, &scope, "before-exec", client, revision, &run)
            .await
            .unwrap();

        tui.input(tui.route.clone(), 1, b"\n").await;
        let job = wait_for_agent(&runtime, &terminal, |agent| {
            agent.process_identity.as_ref().is_some_and(|identity| {
                identity.foreground_group.is_some()
                    && identity.foreground_group != shell.foreground_group
            })
        })
        .await;
        let job_group = job.process_identity.unwrap().foreground_group.unwrap();
        let job_image = std::fs::read_link(format!("/proc/{job_group}/exe")).unwrap();
        assert!(job_image.ends_with("cat"));
        assert_eq!(job.agent_run_id, run.agent_run_id);
        assert_eq!(job.badge, AgentBadge::Unknown as i32);

        tui.input(tui.route.clone(), 2, b"\x04").await;
        wait_for_agent(&runtime, &terminal, |agent| {
            agent.process_identity.as_ref() == Some(shell)
        })
        .await;
        tui.input(tui.route.clone(), 3, b"\n").await;
        let proc_image = format!("/proc/{}/exe", shell.pid);
        while !std::fs::read_link(&proc_image).is_ok_and(|image| image.ends_with("sleep")) {
            sleep(Duration::from_millis(5)).await;
        }
        let cached = corvid_client::agents(&runtime).await.unwrap();
        assert!(
            cached.agents.iter().any(|agent| {
                agent.agent_run_id == run.agent_run_id && agent.terminal_available
            })
        );
        let gone = focus(&runtime, &scope, "after-exec", client, revision, &run)
            .await
            .unwrap_err();
        assert_eq!(gone.code, "TargetGone");

        let ended = wait_for_agent(&runtime, &terminal, |agent| {
            agent.end_reason == AgentEndReason::ContextReplaced as i32
        })
        .await;
        assert_eq!(ended.agent_run_id, run.agent_run_id);
        assert_eq!(ended.process_identity.as_ref(), Some(shell));
        assert_eq!(ended.process, AgentProcessState::Running as i32);
        assert!(!ended.terminal_available);
        assert!(
            corvid_client::clients(&runtime)
                .await
                .unwrap()
                .clients
                .iter()
                .any(|view| view.terminal_id.as_deref() == Some(terminal.as_str()))
        );
    })
    .await
    .unwrap();
}

async fn start_companion(
    runtime: &Path,
    directory: &Path,
    host_id: &str,
    slots: u32,
) -> (tokio::process::Child, PresentationClient) {
    let state = directory.join("companion");
    let socket = directory.join("presentation");
    initialize(
        &state,
        Uuid::parse_str(host_id).unwrap(),
        HostEndpoint::Local {
            runtime_dir: runtime.to_owned(),
        },
        slots,
    )
    .unwrap();
    let process = command(runtime)
        .args(["companion", "run", "--state-dir"])
        .arg(state)
        .arg("--socket-dir")
        .arg(&socket)
        .spawn()
        .unwrap();
    loop {
        if let Ok(companion) = PresentationClient::connect(&socket).await {
            return (process, companion);
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn presented(
    companion: &mut PresentationClient,
    condition: impl Fn(&PresentationSlots) -> bool,
) -> PresentationSlots {
    loop {
        let reply = companion
            .request(Body::GetPresentationSlots(GetPresentationSlots::default()))
            .await
            .unwrap();
        let Body::PresentationSlots(view) = reply else {
            panic!("missing presentation slots")
        };
        if condition(&view) {
            return view;
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn slot_run(view: &PresentationSlots, index: usize) -> Option<&str> {
    let run = view.slots.get(index)?.run.as_ref()?;
    Some(&run.key.as_ref()?.agent_run_id)
}

async fn wait_for_agent(
    runtime: &Path,
    terminal: &str,
    condition: impl Fn(&AgentInfo) -> bool,
) -> AgentInfo {
    loop {
        let agents = corvid_client::agents(runtime).await.unwrap();
        if let Some(agent) = agents
            .agents
            .into_iter()
            .find(|agent| agent.terminal_id == terminal && condition(agent))
        {
            return agent;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

struct ControlWire {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    session: Session,
}

impl ControlWire {
    async fn connect(runtime: &Path, capability: &str) -> Self {
        let directory = RuntimeDir::open(runtime).unwrap();
        let (read, write) = directory
            .connect(SocketRole::Control)
            .await
            .unwrap()
            .into_split();
        let mut writer = FrameWriter::start(write, FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader = FrameReader::start(read, FrameRole::StructuredOnly)
            .await
            .unwrap();
        let greeting = hello(
            "hello".into(),
            Some("test".into()),
            vec![capability.to_owned()],
        );
        writer.envelope(&greeting).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let session = check_hello_result(&greeting, &reply, PeerRole::Control).unwrap();
        Self {
            reader,
            writer,
            session,
        }
    }

    async fn request(&mut self, id: &str, body: Body) {
        let request = self.session.envelope(Some(id.to_owned()), body);
        self.writer.envelope(&request).await.unwrap();
    }

    async fn next(&mut self) -> Envelope {
        let reply = self.reader.next_envelope().await.unwrap().unwrap();
        self.session.validate_scope(&reply).unwrap();
        reply
    }
}

fn controller_scope(tui: &Tui, controller_id: &str) -> ControllerScope {
    ControllerScope {
        controller_id: controller_id.to_owned(),
        host_id: tui.session.host_id.clone(),
        server_epoch: tui.session.server_epoch.clone(),
    }
}

async fn issue_pairing(tui: &mut Tui) -> Uuid {
    tui.send(Body::IssuePairingCode(IssuePairingCode {})).await;
    let Body::PairingCode(code) = tui.next_body(&mut Vec::new()).await else {
        panic!("missing pairing code");
    };
    assert_eq!(code.expires_in_seconds, 60);
    Uuid::parse_str(&code.code).unwrap()
}

async fn focus(
    runtime: &Path,
    scope: &ControllerScope,
    request: &str,
    client: Uuid,
    revision: u64,
    run: &AgentInfo,
) -> Result<FocusApplied, ProtocolFault> {
    let result = controller(
        runtime,
        scope,
        request.to_owned(),
        ControllerRequest::Focus {
            client,
            revision,
            run: Uuid::parse_str(&run.agent_run_id).unwrap(),
        },
    )
    .await;
    match result {
        Ok(Body::FocusApplied(applied)) => Ok(applied),
        Ok(other) => panic!("unexpected focus response: {other:?}"),
        Err(fault) => Err(fault.downcast().unwrap()),
    }
}

#[tokio::test]
async fn expired_and_unmapped_command_prefixes_reach_the_original_terminal() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let argv = shell("stty raw -echo; printf 'READY'; head -c 4 | od -An -tx1; sleep 4");
        let mut tui = Tui::attach(&runtime, Some(argv)).await;
        let mut output = Vec::new();
        wait_for_text(&mut tui, &mut output, "READY").await;
        tui.enter_command_mode(1, &mut output).await;
        assert!(control(&runtime, "clients").await.contains("mode=Command"));

        while tui.client_state(&runtime).await.input_mode == Some(TuiInputMode::Command as i32) {
            sleep(Duration::from_millis(25)).await;
        }
        assert!(
            control(&runtime, "clients")
                .await
                .contains("mode=Passthrough")
        );
        tui.input(tui.route.clone(), 2, b"x").await;

        tui.enter_command_mode(3, &mut output).await;
        tui.input(tui.route.clone(), 4, b"r").await;
        wait_for_text(&mut tui, &mut output, "1d 78 1d 72").await;
        assert_eq!(
            tui.client_state(&runtime).await.input_mode,
            Some(TuiInputMode::Passthrough as i32)
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn registered_focus_fences_input_and_changes_only_the_named_client() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let fixture = temporary.path().join("claude");
    std::fs::write(
        &fixture,
        concat!(
            "#!/bin/sh\n",
            "[ \"$1\" != --version ] || exit 1\n",
            "stty -echo\n",
            "printf '%s-READY\\n' \"$1\"\n",
            "while IFS= read -r line; do printf '%s=%s\\n' \"$1\" \"$line\"; done\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut daemon_command = command(&runtime);
    let path =
        std::env::join_paths([temporary.path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    daemon_command.env("PATH", path);
    let _daemon = start_daemon(daemon_command, &runtime, &temporary.path().join("state")).await;
    let launch = |label: &str| LaunchCommand {
        argv: vec![label.as_bytes().to_vec()],
        agent_provider: Some(AgentProvider::Claude as i32),
        ..Default::default()
    };

    timeout(WAIT, async {
        let mut first = Tui::attach_command(&runtime, Some(launch("A"))).await;
        wait_for_text(&mut first, &mut Vec::new(), "A-READY").await;
        let a = wait_for_agent(&runtime, &first.route.terminal_id, |agent| agent.terminal_available).await;
        drop(first);
        while !corvid_client::clients(&runtime).await.unwrap().clients.is_empty() {
            sleep(Duration::from_millis(10)).await;
        }

        let mut main = Tui::attach_target(&runtime, Attach {
            create_tab: Some("Agent B".to_owned()),
            launch: Some(launch("B")),
            terminal_capabilities: vec!["tabs.v1".into()],
            ..Default::default()
        }).await;
        let mut other = Tui::attach_target(&runtime, Attach {
            create_workspace: Some("Agent C".to_owned()),
            launch: Some(launch("C")),
            terminal_capabilities: vec![PAIR_CAPABILITY.into()],
            ..Default::default()
        }).await;
        let mut output = Vec::new();
        wait_for_text(&mut main, &mut output, "B-READY").await;
        wait_for_text(&mut other, &mut Vec::new(), "C-READY").await;
        let b = wait_for_agent(&runtime, &main.route.terminal_id, |agent| agent.terminal_available).await;
        let c = wait_for_agent(&runtime, &other.route.terminal_id, |agent| agent.terminal_available).await;
        let scope = controller_scope(&main, "test");
        let client = Uuid::parse_str(&main.route.client_id).unwrap();
        let code = issue_pairing(&mut main).await;
        let Body::ClientBindingRegistered(binding) = controller(
            &runtime, &scope, "bind".into(), ControllerRequest::Register { code },
        ).await.unwrap() else { panic!("missing binding") };
        let revision = binding.binding_revision;
        let wrong_owner = focus(&runtime, &scope, "busy-c", client, revision, &c).await.unwrap_err();
        assert_eq!(wrong_owner.code, "WorkspaceBusy");
        let source_tab = &a.location.as_ref().unwrap().tab_id;
        let destination = &c.location.as_ref().unwrap().workspace_id;
        let refused = main.move_tab(source_tab, destination, &mut output).await;
        assert!(matches!(refused, Body::Error(fault) if fault.code == "WorkspaceBusy"));

        let original = main.route.clone();
        let paste_id = Uuid::new_v4().to_string();
        main.send(Body::PasteBegin(PasteBegin {
            route_token: Some(original.clone()), input_sequence: 1, paste_id: paste_id.clone(),
        })).await;
        main.send(Body::Ping(Ping {})).await;
        assert!(matches!(main.next_body(&mut output).await, Body::Pong(_)));
        let blocked = focus(&runtime, &scope, "paste-busy", client, revision, &a).await.unwrap_err();
        assert_eq!(blocked.code, "InputBusy");
        main.send(Body::PasteChunk(PasteChunk { paste_id: paste_id.clone(), data: b"kept-on-b\n".to_vec() })).await;
        main.send(Body::PasteEnd(PasteEnd { paste_id })).await;
        wait_for_text(&mut main, &mut output, "B=kept-on-b").await;

        main.enter_command_mode(2, &mut output).await;
        let main_before = main.client_state(&runtime).await;
        assert_eq!(main_before.input_mode, Some(TuiInputMode::Command as i32));
        assert_eq!(main_before.keymap, Some(ClientKeymap { tabs: true, pairing: false }));

        let mut other_output = Vec::new();
        other.enter_command_mode(1, &mut other_output).await;
        let other_before = other.client_state(&runtime).await;
        assert_eq!(other_before.input_mode, Some(TuiInputMode::Command as i32));
        assert_eq!(other_before.keymap, Some(ClientKeymap { tabs: false, pairing: true }));

        let applied = focus(&runtime, &scope, "focus-a", client, revision, &a).await.unwrap();
        assert_eq!(applied.agent_run_id, a.agent_run_id);
        assert_eq!(applied.terminal_id, a.terminal_id);
        assert_eq!(applied.view_revision, original.view_revision + 1);
        assert_eq!(other.client_state(&runtime).await, other_before);
        other.input(other.route.clone(), 2, b"r").await;
        assert!(matches!(other.next_body(&mut other_output).await, Body::PairingCode(_)));
        main.input(original, 3, b"retired-route\n").await;
        main.moved(&mut output, a.location.as_ref().unwrap()).await;
        let main_after = main.client_state(&runtime).await;
        assert_eq!(main_after.input_mode, Some(TuiInputMode::Passthrough as i32));
        assert_eq!(main_after.keymap, main_before.keymap);
        let Body::Error(fault) = main.next_body(&mut output).await else { panic!("missing stale route rejection") };
        assert_eq!(fault.code, "StaleRoute");
        main.input(main.route.clone(), 4, b"fresh\n").await;
        wait_for_text(&mut main, &mut output, "A=fresh").await;
        assert!(!String::from_utf8_lossy(&output).contains("retired-route"));

        other.input(other.route.clone(), 3, b"unchanged\n").await;
        wait_for_text(&mut other, &mut other_output, "C=unchanged").await;
        assert_eq!(other.client_state(&runtime).await.input_mode, Some(TuiInputMode::Passthrough as i32));
        focus(&runtime, &scope, "focus-b", client, revision, &b).await.unwrap();
        main.moved(&mut output, b.location.as_ref().unwrap()).await;

        let mut wire = ControlWire::connect(&runtime, FOCUS_CAPABILITY).await;
        let run = Uuid::parse_str(&a.agent_run_id).unwrap();
        let encoded_differently = FocusAgentRequest {
            controller_id: scope.controller_id.clone(),
            client_id: client.simple().to_string(),
            agent_run_id: run.simple().to_string(),
            binding_revision: Some(revision),
        };
        wire.request("focus-a", Body::FocusAgent(encoded_differently.clone())).await;
        assert_eq!(wire.next().await.body, Some(Body::FocusApplied(applied)));
        let clients = corvid_client::clients(&runtime).await.unwrap();
        assert!(clients.clients.iter().any(|view| view.client_id == client.to_string() && view.terminal_id.as_deref() == Some(&b.terminal_id)));

        let missing_revision = FocusAgentRequest { binding_revision: None, ..encoded_differently };
        wire.request("no-revision", Body::FocusAgent(missing_revision.clone())).await;
        assert!(matches!(wire.next().await.body, Some(Body::Error(error)) if error.code == "InvalidMessage"));
        let mut stale = wire.session.envelope(Some("old-epoch".to_owned()), Body::FocusAgent(missing_revision));
        stale.server_epoch = Some("retired-epoch".to_owned());
        wire.writer.envelope(&stale).await.unwrap();
        assert!(matches!(wire.next().await.body, Some(Body::Error(error)) if error.code == "EpochMismatch"));

        drop(other);
        while corvid_client::clients(&runtime).await.unwrap().clients.len() > 1 {
            sleep(Duration::from_millis(10)).await;
        }
        let unclaimed = focus(&runtime, &scope, "unclaimed-c", client, revision, &c).await.unwrap_err();
        assert_eq!(unclaimed.code, "WriterRequired");
        let refused = main.move_tab(source_tab, destination, &mut output).await;
        assert!(matches!(refused, Body::Error(fault) if fault.code == "WriterRequired"));
        main.send(Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
            workspace_id: destination.clone(),
        })).await;
        assert!(matches!(main.next_body(&mut output).await, Body::WriterClaimed(_)));

        verify_tab_move(&runtime, temporary.path(), &mut main, &a, destination).await;
        focus(&runtime, &scope, "preclaimed-c", client, revision, &c).await.unwrap();
        main.moved(&mut output, c.location.as_ref().unwrap()).await;
    }).await.unwrap();
}

async fn verify_tab_move(
    runtime: &Path,
    directory: &Path,
    client: &mut Tui,
    agent: &AgentInfo,
    destination: &str,
) {
    let (_companion, mut companion) =
        start_companion(runtime, directory, &client.session.host_id, 3).await;
    presented(&mut companion, |view| {
        view.connection == CompanionConnection::Ready as i32
    })
    .await;
    let code = issue_pairing(client).await;
    let registered = companion
        .request(Body::PairTerminal(PairTerminal {
            code: code.to_string(),
            wezterm: None,
        }))
        .await
        .unwrap();
    assert!(matches!(registered, Body::ClientBindingRegistered(_)));
    let before = presented(&mut companion, |view| {
        !view.bindings.is_empty()
            && view.slots.iter().any(|slot| {
                slot.run
                    .as_ref()
                    .and_then(|run| run.key.as_ref())
                    .is_some_and(|key| key.agent_run_id == agent.agent_run_id)
            })
    })
    .await;
    let slot = before
        .slots
        .iter()
        .find(|slot| {
            slot.run
                .as_ref()
                .and_then(|run| run.key.as_ref())
                .is_some_and(|key| key.agent_run_id == agent.agent_run_id)
        })
        .unwrap();
    let captured = FocusSlot {
        control_generation: before.control_generation.unwrap(),
        slot: Some(PresentedSlotToken {
            slot_id: slot.slot_id,
            revision: Some(slot.revision),
            run: slot.run.as_ref().unwrap().key.clone(),
        }),
        binding: Some(before.bindings[0].clone()),
    };

    let mut output = Vec::new();
    let visible = client.selection.tab_id.clone();
    let refused = client.move_tab(&visible, destination, &mut output).await;
    assert!(matches!(refused, Body::Error(fault) if fault.code == "InputBusy"));

    let mut watch = corvid_client::watch_snapshot(runtime).await.unwrap();
    let old = agent.location.as_ref().unwrap();
    let expected = ViewSelection {
        workspace_id: destination.to_owned(),
        ..old.clone()
    };
    let Body::TabMoved(moved) = client.move_tab(&old.tab_id, destination, &mut output).await else {
        panic!("missing tab relocation");
    };
    assert_eq!(moved.location.as_ref(), Some(&expected));
    while watch.state().snapshot().sequence() < moved.commit_sequence {
        watch.next_change().await.unwrap();
    }
    assert_moved_entities(&watch, agent, &expected, &client.route);

    let after = presented(&mut companion, |view| {
        view.remote_sequence
            .is_some_and(|seq| seq >= moved.commit_sequence)
    })
    .await;
    assert!(after.slots.contains(slot));
    let Body::SlotFocused(focused) = companion.request(Body::FocusSlot(captured)).await.unwrap()
    else {
        panic!("missing focus reply");
    };
    let focused = focused.remote.unwrap();
    assert_eq!(focused.agent_run_id, agent.agent_run_id);
    assert_eq!(focused.terminal_id, agent.terminal_id);
    assert_eq!(focused.selection.as_ref(), Some(&expected));
    client.moved(&mut output, &expected).await;
    wait_for_text(client, &mut output, "A=fresh").await;
}

fn assert_moved_entities(
    watch: &corvid_client::SnapshotWatch,
    original: &AgentInfo,
    location: &ViewSelection,
    unchanged: &RouteToken,
) {
    let snapshot = watch.state().snapshot();
    assert_eq!(
        snapshot
            .entities()
            .filter(|entry| matches!(entry.body, Some(EntityBody::Terminal(_))))
            .count(),
        3
    );
    assert!(snapshot.entities().any(|entry| matches!(&entry.body,
        Some(EntityBody::Terminal(terminal)) if terminal.terminal_id == original.terminal_id
            && terminal.location.as_ref() == Some(location))));
    assert!(snapshot.entities().any(|entry| matches!(&entry.body,
        Some(EntityBody::Agent(agent)) if agent.summary.as_ref().is_some_and(|agent| {
            agent.agent_run_id == original.agent_run_id && agent.terminal_id == original.terminal_id
                && agent.location.as_ref() == Some(location) && agent.process_identity == original.process_identity
        }))));
    assert!(snapshot.entities().any(|entry| matches!(&entry.body,
        Some(EntityBody::Client(client)) if client.client_id == unchanged.client_id
            && client.terminal_id.as_ref() == Some(&unchanged.terminal_id)
            && client.view_revision == Some(unchanged.view_revision))));
}

#[tokio::test]
async fn pairing_code_is_single_use_and_rpc_replay_preserves_the_first_revision() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let mut tui = Tui::attach_command(&runtime, Some(echo_fixture("PAIR"))).await;
        let scope = controller_scope(&tui, "pair-fixture");
        let client = Uuid::parse_str(&tui.route.client_id).unwrap();
        let code = issue_pairing(&mut tui).await;
        let first = controller(
            &runtime,
            &scope,
            "pair".into(),
            ControllerRequest::Register { code },
        )
        .await
        .unwrap();
        let replay = controller(
            &runtime,
            &scope,
            "pair".into(),
            ControllerRequest::Register { code },
        )
        .await
        .unwrap();
        assert_eq!(first, replay);
        let Body::ClientBindingRegistered(binding) = first else {
            panic!("missing binding")
        };
        assert_eq!(binding.client_id, client.to_string());

        let conflict = controller(
            &runtime,
            &scope,
            "pair".into(),
            ControllerRequest::Unregister {
                client,
                revision: binding.binding_revision,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            conflict.downcast::<ProtocolFault>().unwrap().code,
            "InvalidMessage"
        );

        let reused = controller(
            &runtime,
            &scope,
            "pair-again".into(),
            ControllerRequest::Register { code },
        )
        .await
        .unwrap_err();
        assert_eq!(
            reused.downcast::<ProtocolFault>().unwrap().code,
            "Unauthorized"
        );

        let refreshed = issue_pairing(&mut tui).await;
        let Body::ClientBindingRegistered(next) = controller(
            &runtime,
            &scope,
            "rotate".into(),
            ControllerRequest::Register { code: refreshed },
        )
        .await
        .unwrap() else {
            panic!("missing rotated binding")
        };
        assert!(next.binding_revision > binding.binding_revision);
        let old = controller(
            &runtime,
            &scope,
            "stale-unpair".into(),
            ControllerRequest::Unregister {
                client,
                revision: binding.binding_revision,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            old.downcast::<ProtocolFault>().unwrap().code,
            "StaleBinding"
        );

        let list = corvid_client::clients(&runtime).await.unwrap();
        assert_eq!(
            list.clients[0].bindings[0].binding_revision,
            next.binding_revision
        );
        drop(tui);
        while !corvid_client::clients(&runtime)
            .await
            .unwrap()
            .clients
            .is_empty()
        {
            sleep(Duration::from_millis(10)).await;
        }
        let disconnected = controller(
            &runtime,
            &scope,
            "unpair-after-detach".into(),
            ControllerRequest::Unregister {
                client,
                revision: next.binding_revision,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            disconnected.downcast::<ProtocolFault>().unwrap().code,
            "StaleBinding"
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn snapshot_watch_delivers_committed_routes_and_detach_in_order() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let mut watch = ControlWire::connect(&runtime, WATCH_CAPABILITY).await;
        watch
            .request("watch", Body::SnapshotWatch(SnapshotWatch::default()))
            .await;
        let begin = watch.next().await;
        assert!(matches!(begin.body, Some(Body::SnapshotBegin(_))));
        assert_eq!(begin.request_id.as_deref(), Some("watch"));
        assert_eq!(begin.sequence, Some(0));

        let tui = Tui::attach_target(
            &runtime,
            Attach {
                create_workspace: Some("Watched".to_owned()),
                launch: Some(echo_fixture("WATCH")),
                ..Default::default()
            },
        )
        .await;
        let mut sequence = 0;
        let mut ready = false;
        let mut workspace = false;
        let mut terminal = false;
        let mut client = false;
        while !(ready && workspace && terminal && client) {
            let frame = watch.next().await;
            assert_eq!(frame.subscription_id, begin.subscription_id);
            match frame.body.unwrap() {
                Body::SnapshotChunk(chunk) => {
                    assert_eq!(frame.sequence, Some(0));
                    assert_eq!(frame.request_id.as_deref(), Some("watch"));
                    assert!(
                        chunk
                            .entities
                            .iter()
                            .all(|entity| matches!(entity.body, Some(EntityBody::Workspace(_))))
                    );
                }
                Body::SnapshotEnd(_) => assert_eq!(frame.sequence, Some(0)),
                Body::WatchReady(_) => {
                    assert_eq!(frame.sequence, Some(sequence));
                    assert!(frame.request_id.is_none());
                    ready = true;
                }
                Body::SnapshotDelta(delta) => {
                    sequence += 1;
                    assert_eq!(frame.sequence, Some(sequence));
                    assert!(frame.request_id.is_none());
                    for entity in delta.upsert {
                        match entity.body.unwrap() {
                            EntityBody::Workspace(state)
                                if state.workspace_id == tui.selection.workspace_id =>
                            {
                                workspace = true;
                            }
                            EntityBody::Terminal(state) => {
                                assert!(workspace);
                                assert_eq!(state.terminal_id, tui.route.terminal_id);
                                assert_eq!(state.location.as_ref(), Some(&tui.selection));
                                terminal = true;
                            }
                            EntityBody::Client(state) => {
                                assert!(terminal);
                                assert_eq!(state.client_id, tui.route.client_id);
                                assert_eq!(state.selection.as_ref(), Some(&tui.selection));
                                assert_eq!(
                                    state.writer_leases[0].generation,
                                    tui.route.writer_lease_generation
                                );
                                client = true;
                            }
                            _ => {}
                        }
                    }
                }
                other => panic!("unexpected snapshot event: {other:?}"),
            }
        }

        watch
            .request("clients", Body::ListClients(ListClients {}))
            .await;
        let reply = watch.next().await;
        let Some(Body::ClientList(list)) = reply.body else {
            panic!("missing client list")
        };
        assert_eq!(reply.request_id.as_deref(), Some("clients"));
        assert_eq!(list.clients[0].client_id, tui.route.client_id);
        let client_id = tui.route.client_id.clone();
        let terminal_id = tui.route.terminal_id.clone();
        drop(tui);
        let detached = watch.next().await;
        let Some(Body::SnapshotDelta(delta)) = detached.body else {
            panic!("missing detach delta")
        };
        assert_eq!(detached.sequence, Some(sequence + 1));
        assert_eq!(delta.remove.len(), 1);
        assert_eq!(delta.remove[0].kind, EntityKind::Client as i32);
        assert_eq!(delta.remove[0].id, client_id);

        let mut one_shot = ControlWire::connect(&runtime, SNAPSHOT_CAPABILITY).await;
        one_shot
            .request("snapshot", Body::GetSnapshot(GetSnapshot::default()))
            .await;
        let start = one_shot.next().await;
        let Some(Body::SnapshotBegin(base)) = start.body else {
            panic!("missing snapshot begin")
        };
        assert_eq!(start.sequence, detached.sequence);
        assert!(start.subscription_id.is_none());
        let mut terminal_still_live = false;
        for index in 0..base.chunks {
            let reply = one_shot.next().await;
            let Some(Body::SnapshotChunk(chunk)) = reply.body else {
                panic!("missing snapshot chunk")
            };
            assert_eq!(chunk.index, index);
            for entity in chunk.entities {
                assert!(!matches!(entity.body, Some(EntityBody::Client(_))));
                if let Some(EntityBody::Terminal(terminal)) = entity.body {
                    terminal_still_live |= terminal.terminal_id == terminal_id;
                }
            }
        }
        assert!(terminal_still_live);
        assert!(matches!(
            one_shot.next().await.body,
            Some(Body::SnapshotEnd(_))
        ));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn snapshot_client_keeps_last_state_offline_and_resyncs_at_new_epoch() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let mut original = daemon(&runtime, &state).await;

    timeout(WAIT, async {
        let mut watch = corvid_client::watch_snapshot(&runtime).await.unwrap();
        assert!(watch.is_ready());
        let session = watch.state().session().clone();
        // A caller may cancel a pending read without dropping a partial frame.
        assert!(timeout(Duration::from_millis(20), watch.next_change()).await.is_err());

        let tui = Tui::attach_command(&runtime, Some(echo_fixture("SYNC"))).await;
        let client_id = tui.route.client_id.clone();
        let terminal_id = tui.route.terminal_id.clone();
        let has_client = |watch: &corvid_client::SnapshotWatch| watch.state().snapshot().entities()
            .any(|entry| matches!(&entry.body, Some(EntityBody::Client(client)) if client.client_id == client_id));
        while !has_client(&watch) {
            watch.next_change().await.unwrap();
        }
        drop(tui);
        while has_client(&watch) {
            watch.next_change().await.unwrap();
        }

        original.kill().await.unwrap();
        assert!(watch.next_change().await.is_err());
        assert!(!watch.is_ready());
        assert!(watch.state().snapshot().entities().any(|entry| matches!(&entry.body,
            Some(EntityBody::Terminal(terminal)) if terminal.terminal_id == terminal_id)));

        let _replacement = daemon(&runtime, &state).await;
        let replacement = corvid_client::snapshot(&runtime).await.unwrap();
        assert_eq!(replacement.session().host_id, session.host_id);
        assert_ne!(replacement.session().server_epoch, session.server_epoch);
        assert_eq!(replacement.snapshot().sequence(), 0);
        assert!(replacement.snapshot().entities().all(|entry| matches!(entry.body, Some(EntityBody::Workspace(_)))));
    }).await.unwrap();
}

#[tokio::test]
async fn separate_workspace_writers_and_tab_transitions_preserve_exact_routes() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let mut first = Tui::attach_command(&runtime, Some(echo_fixture("A"))).await;
        let mut other = Tui::attach_target(
            &runtime,
            Attach {
                create_workspace: Some("Other".into()),
                launch: Some(echo_fixture("B")),
                ..Default::default()
            },
        )
        .await;
        let (mut first_output, mut other_output) = (Vec::new(), Vec::new());
        wait_for_text(&mut first, &mut first_output, "A-READY").await;
        wait_for_text(&mut other, &mut other_output, "B-READY").await;
        assert_ne!(first.selection.workspace_id, other.selection.workspace_id);
        assert_ne!(first.route.terminal_id, other.route.terminal_id);

        first.select(&other.selection).await;
        let Body::Error(error) = first.next_body(&mut first_output).await else {
            panic!("expected conflict")
        };
        assert_eq!(error.code, "WorkspaceBusy");
        first.input(first.route.clone(), 1, b"FIRST\n").await;
        other.input(other.route.clone(), 1, b"OTHER\n").await;
        wait_for_text(&mut first, &mut first_output, "A=FIRST").await;
        wait_for_text(&mut other, &mut other_output, "B=OTHER").await;

        let first_selection = first.selection.clone();
        let first_terminal = first.route.terminal_id.clone();
        first
            .send(Body::Detach(corvid_protocol::wire::Detach {}))
            .await;
        while first.reader.next_frame().await.unwrap().is_some() {}
        drop(first);
        sleep(Duration::from_millis(40)).await;

        let mut second_tab = Tui::attach_target(
            &runtime,
            Attach {
                workspace_id: Some(first_selection.workspace_id.clone()),
                create_tab: Some("Second".into()),
                launch: Some(echo_fixture("C")),
                ..Default::default()
            },
        )
        .await;
        let mut switched_output = Vec::new();
        wait_for_text(&mut second_tab, &mut switched_output, "C-READY").await;
        let retired = second_tab.route.clone();
        second_tab.select(&first_selection).await;
        second_tab
            .moved(&mut switched_output, &first_selection)
            .await;
        assert_eq!(second_tab.route.terminal_id, first_terminal);
        assert_eq!(second_tab.route.client_id, retired.client_id);
        assert_eq!(second_tab.route.view_revision, retired.view_revision + 1);
        second_tab.input(retired, 1, b"BAD\n").await;
        second_tab
            .input(second_tab.route.clone(), 2, b"AFTER\n")
            .await;
        let Body::Error(error) = second_tab.next_body(&mut switched_output).await else {
            panic!("expected stale route")
        };
        assert_eq!(error.code, "StaleRoute");
        wait_for_text(&mut second_tab, &mut switched_output, "A=AFTER").await;
        assert!(!String::from_utf8_lossy(&switched_output).contains("A=BAD"));

        let other_selection = other.selection.clone();
        let other_terminal = other.route.terminal_id.clone();
        other.input(other.route.clone(), 2, b"UNCHANGED\n").await;
        wait_for_text(&mut other, &mut other_output, "B=UNCHANGED").await;
        other
            .send(Body::Detach(corvid_protocol::wire::Detach {}))
            .await;
        while other.reader.next_frame().await.unwrap().is_some() {}
        drop(other);
        sleep(Duration::from_millis(40)).await;

        second_tab.select(&other_selection).await;
        let Body::Error(error) = second_tab.next_body(&mut switched_output).await else {
            panic!("expected writer requirement")
        };
        assert_eq!(error.code, "WriterRequired");
        second_tab
            .send(Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
                workspace_id: other_selection.workspace_id.clone(),
            }))
            .await;
        let Body::WriterClaimed(claim) = second_tab.next_body(&mut switched_output).await else {
            panic!("expected claim")
        };
        second_tab.select(&other_selection).await;
        second_tab
            .moved(&mut switched_output, &other_selection)
            .await;
        assert_eq!(second_tab.route.terminal_id, other_terminal);
        assert_eq!(second_tab.route.writer_lease_generation, claim.generation);
    })
    .await
    .unwrap();
}

const TEST_PASTE_ID: &str = "00000000-0000-0000-0000-000000000001";

async fn begin_paste(tui: &mut Tui, sequence: u64) {
    tui.send(Body::PasteBegin(PasteBegin {
        route_token: Some(tui.route.clone()),
        paste_id: TEST_PASTE_ID.into(),
        input_sequence: sequence,
    }))
    .await;
}

async fn paste_chunk(tui: &mut Tui, data: Vec<u8>) {
    tui.send(Body::PasteChunk(PasteChunk {
        paste_id: TEST_PASTE_ID.into(),
        data,
    }))
    .await;
}

async fn end_paste(tui: &mut Tui) {
    tui.send(Body::PasteEnd(PasteEnd {
        paste_id: TEST_PASTE_ID.into(),
    }))
    .await;
}

async fn resize(tui: &mut Tui) {
    tui.send(Body::ResizeViewport(ResizeViewport {
        client_id: tui.route.client_id.clone(),
        connection_generation: tui.route.connection_generation,
        viewport: Some(Viewport {
            cols: 100,
            rows: 30,
        }),
    }))
    .await;
}

async fn wait_for_text(tui: &mut Tui, output: &mut Vec<u8>, needle: &str) {
    while !String::from_utf8_lossy(output).contains(needle) {
        assert!(tui.event(output).await.is_none());
    }
}

#[tokio::test]
async fn incomplete_and_oversized_pastes_cannot_write_across_resize() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let mut tui = Tui::attach(&runtime, Some(shell(
            "stty -echo; printf 'READY\\n'; while IFS= read -r line; do printf 'INPUT=%s\\n' \"$line\"; done"
        ))).await;
        let mut output = Vec::new();
        wait_for_text(&mut tui, &mut output, "READY").await;
        begin_paste(&mut tui, 1).await;
        paste_chunk(&mut tui, b"BAD\n".to_vec()).await;
        resize(&mut tui).await;
        let mut aborted = false;
        loop {
            match tui.event(&mut output).await {
                Some(Body::Error(error)) if error.code == "StaleRoute" => aborted = true,
                Some(Body::ViewChanged(view)) => {
                    tui.route = view.route_token.unwrap();
                    break;
                }
                None => {}
                _ => panic!("unexpected resize event"),
            }
        }
        assert!(aborted);
        end_paste(&mut tui).await;
        loop {
            if let Some(Body::Error(error)) = tui.event(&mut output).await {
                assert_eq!(error.code, "StaleRoute");
                break;
            }
        }

        begin_paste(&mut tui, 2).await;
        for _ in 0..128 {
            paste_chunk(&mut tui, vec![b'X'; 8192]).await;
        }
        paste_chunk(&mut tui, vec![b'\n']).await;
        loop {
            if let Some(Body::Error(error)) = tui.event(&mut output).await {
                assert_eq!(error.code, "LimitExceeded");
                break;
            }
        }
        begin_paste(&mut tui, 3).await;
        paste_chunk(&mut tui, b"YES\n".to_vec()).await;
        end_paste(&mut tui).await;
        wait_for_text(&mut tui, &mut output, "INPUT=YES").await;
        let transcript = String::from_utf8_lossy(&output);
        assert!(!transcript.contains("INPUT=BAD") && !transcript.contains("INPUT=XXXX"));
    }).await.unwrap();
}

#[tokio::test]
async fn accepted_paste_blocks_resize_until_its_full_batch_is_written() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let argv = shell("stty -echo -icanon min 1 time 0; printf 'READY\\n'; sleep .7; head -c 65536 >/dev/null; printf 'COMPLETE\\n'; sleep 10");
        let mut tui = Tui::attach(&runtime, Some(argv)).await;
        let mut output = Vec::new();
        wait_for_text(&mut tui, &mut output, "READY").await;
        begin_paste(&mut tui, 1).await;
        for _ in 0..8 {
            paste_chunk(&mut tui, vec![b'X'; 8192]).await;
        }
        end_paste(&mut tui).await;
        resize(&mut tui).await;
        loop {
            if let Some(Body::Error(error)) = tui.event(&mut output).await {
                assert_eq!(error.code, "InputBusy");
                break;
            }
        }
        wait_for_text(&mut tui, &mut output, "COMPLETE").await;
        resize(&mut tui).await;
        loop {
            if let Some(Body::ViewChanged(view)) = tui.event(&mut output).await {
                assert_eq!(view.view_revision, tui.route.view_revision + 1);
                break;
            }
        }
    }).await.unwrap();
}

#[tokio::test]
async fn stalled_partial_paste_reports_failure_and_fences_the_writer() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let argv = shell("stty -echo -icanon min 1 time 0; printf 'READY\\n'; sleep 15");
        let mut tui = Tui::attach(&runtime, Some(argv)).await;
        let mut output = Vec::new();
        wait_for_text(&mut tui, &mut output, "READY").await;
        begin_paste(&mut tui, 1).await;
        for _ in 0..8 {
            paste_chunk(&mut tui, vec![b'X'; 8192]).await;
        }
        end_paste(&mut tui).await;
        loop {
            if let Some(Body::Error(error)) = tui.event(&mut output).await {
                assert_eq!(error.code, "PartialDelivery");
                assert!(!error.retryable);
                break;
            }
        }
        assert!(tui.reader.next_frame().await.unwrap().is_none());
        let next = Tui::attach(&runtime, None).await;
        assert_eq!(next.route.terminal_id, tui.route.terminal_id);
        assert_ne!(next.route.client_id, tui.route.client_id);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn resize_fences_previous_route_before_any_pty_write() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let argv = shell("stty -echo; printf 'READY\\n'; while IFS= read -r line; do printf 'INPUT=%s\\n' \"$line\"; done");
        let mut tui = Tui::attach(&runtime, Some(argv)).await;
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains("READY") {
            assert!(tui.event(&mut output).await.is_none());
        }
        let old = tui.route.clone();
        tui.send(Body::ResizeViewport(ResizeViewport {
            client_id: old.client_id.clone(),
            connection_generation: old.connection_generation,
            viewport: Some(Viewport { cols: 100, rows: 30 }),
        })).await;
        let new = loop {
            if let Some(Body::ViewChanged(view)) = tui.event(&mut output).await {
                break view.route_token.unwrap();
            }
        };
        assert!(new.view_revision > old.view_revision);
        for (route, data) in [(old, b"BAD\n"), (new, b"YES\n")] {
            tui.send(Body::InputChunk(InputChunk {
                route_token: Some(route),
                input_sequence: 1,
                data: data.to_vec(),
            })).await;
        }
        let mut rejected = false;
        while !String::from_utf8_lossy(&output).contains("INPUT=YES") {
            if let Some(Body::Error(error)) = tui.event(&mut output).await {
                assert_eq!(error.code, "StaleRoute");
                rejected = true;
            }
        }
        assert!(rejected);
        assert!(!String::from_utf8_lossy(&output).contains("INPUT=BAD"));
    }).await.unwrap();
}

#[tokio::test]
async fn malformed_and_stalled_frames_leave_the_existing_pty_usable() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;

    timeout(Duration::from_secs(12), async {
        let mut owner = Tui::attach_command(&runtime, Some(echo_fixture("BOUNDARY"))).await;
        let mut output = Vec::new();
        wait_for_text(&mut owner, &mut output, "BOUNDARY-READY").await;
        let host = control(&runtime, "host").await;
        let clients = control(&runtime, "clients").await;

        for role in [SocketRole::Control, SocketRole::Client] {
            reject_invalid_frames(&runtime, role).await;
        }
        reject_required_capability(&runtime).await;

        let (mut stalled, mut write) = connect_raw(&runtime, SocketRole::Control).await;
        {
            let mut writer = FrameWriter::start(&mut write, FrameRole::StructuredOnly)
                .await
                .unwrap();
            let greeting = hello("stalled".into(), Some("test-controller".into()), vec![]);
            writer.envelope(&greeting).await.unwrap();
            let reply = stalled.next_envelope().await.unwrap().unwrap();
            check_hello_result(&greeting, &reply, PeerRole::Control).unwrap();
        }
        write.write_all(&[0, 0]).await.unwrap();
        let started = Instant::now();
        owner
            .input(owner.route.clone(), 1, b"while-stalled\n")
            .await;
        wait_for_text(&mut owner, &mut output, "BOUNDARY=while-stalled").await;
        assert_eq!(control(&runtime, "ping").await, "pong\n");

        let eof = timeout(
            COMMAND_TIMEOUT + Duration::from_secs(2),
            stalled.next_frame(),
        )
        .await
        .unwrap();
        assert!(eof.unwrap().is_none());
        assert!(started.elapsed() >= COMMAND_TIMEOUT.saturating_sub(Duration::from_millis(250)));

        owner
            .input(owner.route.clone(), 2, b"after-rejection\n")
            .await;
        wait_for_text(&mut owner, &mut output, "BOUNDARY=after-rejection").await;
        assert_eq!(control(&runtime, "host").await, host);
        assert_eq!(control(&runtime, "clients").await, clients);
    })
    .await
    .unwrap();
}

async fn connect_raw(
    runtime: &Path,
    role: SocketRole,
) -> (FrameReader<OwnedReadHalf>, OwnedWriteHalf) {
    let runtime = RuntimeDir::open(runtime).unwrap();
    let (read, write) = runtime.connect(role).await.unwrap().into_split();
    let frame_role = if role == SocketRole::Client {
        FrameRole::TuiOutput
    } else {
        FrameRole::StructuredOnly
    };
    let reader = FrameReader::start(read, frame_role).await.unwrap();
    (reader, write)
}

async fn reject_invalid_frames(runtime: &Path, role: SocketRole) {
    let invalid = [
        (0, &[][..]),
        (u32::MAX, &[]),
        (CONTROL_LIMIT as u32 + 1, &[]),
        (1, &[2]),
        (1, &[99]),
        (3, &[1, 0x52]),
        (6, &[1, 0x52, 3, 0x0a, 1, 0xff]),
    ];
    let mut attempts = vec![b"CRV0".to_vec(), b"CRV1\0\0".to_vec()];
    attempts.extend(invalid.into_iter().map(|(length, payload)| {
        let mut bytes = FRAME_PREAMBLE.to_vec();
        bytes.extend(length.to_be_bytes());
        bytes.extend(payload);
        bytes
    }));
    for bytes in attempts {
        let (mut reader, mut write) = connect_raw(runtime, role).await;
        write.write_all(&bytes).await.unwrap();
        write.shutdown().await.unwrap();
        assert!(reader.next_frame().await.unwrap().is_none());
    }
}

async fn reject_required_capability(runtime: &Path) {
    let (mut reader, write) = connect_raw(runtime, SocketRole::Client).await;
    let mut writer = FrameWriter::start(write, FrameRole::StructuredOnly)
        .await
        .unwrap();
    let greeting = hello(
        "missing-capability".into(),
        None,
        vec!["fixture_missing.v1".into()],
    );
    writer.envelope(&greeting).await.unwrap();
    let reply = reader.next_envelope().await.unwrap().unwrap();
    let fault = check_hello_result(&greeting, &reply, PeerRole::Tui).unwrap_err();
    assert_eq!(fault.code, "UnsupportedCapability");
    assert!(reader.next_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn stalled_render_and_snapshot_consumers_release_their_connections() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let layout = PressureLayout::seed(&state);
    let daemon = daemon(&runtime, &state).await;
    let daemon_pid = daemon.id().unwrap();

    timeout(Duration::from_secs(15), async {
        let mut independent = layout.open_views(&runtime).await;
        let mut watchers = stall_watchers(&runtime).await;
        let mut control = ControlWire::connect(&runtime, "ping.v1").await;

        let progress = temporary.path().join("progress");
        let mut stalled = attach_at(&runtime, &layout.flood, flooding_child(&progress)).await;
        let Some(Frame::Render(_)) = stalled.reader.next_frame().await.unwrap() else {
            panic!("expected initial render");
        };
        let stalled_since = Instant::now();
        let mut independent_output = Vec::new();
        let pressure = layout.churn(&mut independent, &mut control, &mut independent_output).await;
        assert_watch_disconnected(&mut watchers[0]).await;

        let recovered = corvid_client::watch_snapshot(&runtime).await.unwrap();
        assert!(recovered.state().snapshot().sequence() >= pressure.sequence);
        independent.input(independent.route.clone(), 1, b"OK\n").await;
        wait_for_text(&mut independent, &mut independent_output, "INDEPENDENT=OK").await;

        assert!(stalled.reader.next_frame().await.unwrap().is_none());
        let render_timeout = stalled_since.elapsed();
        let queries = std::fs::read_to_string(progress).unwrap().lines().count();
        assert!(queries > 20, "PTY stopped parsing output or replying to queries");

        let mut reattached = Tui::attach_target(&runtime, Attach {
            workspace_id: Some(layout.flood.workspace_id.clone()),
            ..Default::default()
        }).await;
        assert_eq!(reattached.route.terminal_id, stalled.route.terminal_id);
        assert_ne!(reattached.route.client_id, stalled.route.client_id);
        let Some(Frame::Render(frame)) = reattached.reader.next_frame().await.unwrap() else {
            panic!("expected full redraw");
        };
        assert!(frame.header.base_frame_id.is_none());
        reattached.send(Body::FrameApplied(FrameApplied {
            render_generation: frame.header.render_generation,
            frame_id: frame.header.frame_id,
        })).await;
        let mut redraw = frame.ansi;
        wait_for_text(&mut reattached, &mut redraw, "ORDERED-OUTPUT").await;

        let status = std::fs::read_to_string(format!("/proc/{daemon_pid}/status")).unwrap();
        let high_water = status.lines().find(|line| line.starts_with("VmHWM:")).unwrap();
        eprintln!("pressure moves={PRESSURE_MOVES} watch_count={PRESSURE_WATCHES} churn={:?} ping_max={:?} input_max={:?} render_disconnect={render_timeout:?} pty_queries={queries} {high_water}",
            pressure.elapsed, pressure.ping_max, pressure.input_max);
    }).await.unwrap();
}

struct PressureLayout {
    views: [ViewSelection; 2],
    hidden: ViewSelection,
    flood: ViewSelection,
}

struct PressureSample {
    sequence: u64,
    elapsed: Duration,
    ping_max: Duration,
    input_max: Duration,
}

impl PressureLayout {
    fn seed(state: &Path) -> Self {
        let (left, left_tab) = pressure_tab("Left", &Uuid::new_v4().to_string());
        let (right, right_tab) = pressure_tab("Right", &Uuid::new_v4().to_string());
        let (hidden, hidden_tab) = pressure_tab("Movable", &left.workspace_id);
        let (flood, flood_tab) = pressure_tab("Flood", &Uuid::new_v4().to_string());
        let mut left_tabs = vec![left_tab, hidden_tab];
        let mut right_tabs = vec![right_tab];
        let padding = "x".repeat(1024);
        for _ in 0..60 {
            left_tabs.push(pressure_tab(&padding, &left.workspace_id).1);
            right_tabs.push(pressure_tab(&padding, &right.workspace_id).1);
        }

        let layout = serde_json::json!({"schema_version": 1, "layout": {"workspaces": [
            {"id": left.workspace_id, "label": "Left", "tabs": left_tabs},
            {"id": right.workspace_id, "label": "Right", "tabs": right_tabs},
            {"id": flood.workspace_id, "label": "Flood", "tabs": [flood_tab]},
        ]}});
        RuntimeDir::create(state)
            .unwrap()
            .replace_private_file("state.json", &serde_json::to_vec(&layout).unwrap())
            .unwrap();
        Self {
            views: [left, right],
            hidden,
            flood,
        }
    }

    async fn open_views(&self, runtime: &Path) -> Tui {
        for selection in [&self.hidden, &self.views[1]] {
            drop(attach_at(runtime, selection, echo_fixture("INDEPENDENT")).await);
            while !corvid_client::clients(runtime)
                .await
                .unwrap()
                .clients
                .is_empty()
            {
                sleep(Duration::from_millis(10)).await;
            }
        }
        let mut tui = attach_at(runtime, &self.views[0], echo_fixture("INDEPENDENT")).await;
        tui.send(Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
            workspace_id: self.views[1].workspace_id.clone(),
        }))
        .await;
        assert!(matches!(
            tui.next_body(&mut Vec::new()).await,
            Body::WriterClaimed(_)
        ));
        tui
    }

    async fn churn(
        &self,
        tui: &mut Tui,
        control: &mut ControlWire,
        output: &mut Vec<u8>,
    ) -> PressureSample {
        let started = Instant::now();
        let mut sequence = 0;
        let mut ping_max = Duration::ZERO;
        let mut input_max = Duration::ZERO;
        for index in 0..PRESSURE_MOVES {
            let destination = &self.views[(index + 1) % self.views.len()];
            let moved = tui
                .move_tab(&self.hidden.tab_id, &destination.workspace_id, output)
                .await;
            let Body::TabMoved(moved) = moved else {
                panic!("unexpected move result {moved:?}")
            };
            sequence = moved.commit_sequence;

            if index % 8 == 0 {
                let start = Instant::now();
                control.request("ping", Body::Ping(Ping {})).await;
                let pong = timeout(Duration::from_secs(2), control.next())
                    .await
                    .unwrap();
                assert!(matches!(pong.body, Some(Body::Pong(_))));
                ping_max = ping_max.max(start.elapsed());

                let marker = format!("pressure-{index}");
                let start = Instant::now();
                tui.input(tui.route.clone(), 1, format!("{marker}\n").as_bytes())
                    .await;
                wait_for_text(tui, output, &format!("INDEPENDENT={marker}")).await;
                input_max = input_max.max(start.elapsed());
            }
            tui.select(destination).await;
            tui.moved(output, destination).await;
        }
        PressureSample {
            sequence,
            elapsed: started.elapsed(),
            ping_max,
            input_max,
        }
    }
}

fn pressure_tab(label: &str, workspace: &str) -> (ViewSelection, serde_json::Value) {
    let location = ViewSelection {
        workspace_id: workspace.to_owned(),
        tab_id: Uuid::new_v4().to_string(),
        pane_id: Uuid::new_v4().to_string(),
    };
    let tab = serde_json::json!({"id": location.tab_id, "label": label, "root": {
        "kind": "pane", "id": location.pane_id, "last_cwd": null,
    }});
    (location, tab)
}

async fn attach_at(runtime: &Path, selection: &ViewSelection, launch: LaunchCommand) -> Tui {
    Tui::attach_target(
        runtime,
        Attach {
            workspace_id: Some(selection.workspace_id.clone()),
            tab_id: Some(selection.tab_id.clone()),
            launch: Some(launch),
            ..Default::default()
        },
    )
    .await
}

fn flooding_child(progress: &Path) -> LaunchCommand {
    let mut argv = shell(
        r#"
        stty raw -echo
        i=0
        while :; do
            printf '%32768s\033[3;5H\033[6n' ''
            reply=$(head -c 6)
            [ "$reply" = "$(printf '\033[3;5R')" ] || exit 42
            i=$((i+1))
            printf '%s\n' "$i" >> "$1"
            printf '\033[4;1HORDERED-OUTPUT'
            sleep .02
        done
    "#,
    );
    argv.push(b"progress-fixture".to_vec());
    argv.push(progress.as_os_str().as_encoded_bytes().to_vec());
    LaunchCommand {
        argv,
        ..Default::default()
    }
}

async fn stall_watchers(runtime: &Path) -> Vec<ControlWire> {
    let mut watchers = Vec::new();
    for _ in 0..PRESSURE_WATCHES {
        let mut watch = ControlWire::connect(runtime, WATCH_CAPABILITY).await;
        watch
            .request("watch", Body::SnapshotWatch(SnapshotWatch::default()))
            .await;
        loop {
            match watch.next().await.body {
                Some(Body::WatchReady(_)) => break,
                Some(Body::SnapshotBegin(_) | Body::SnapshotChunk(_) | Body::SnapshotEnd(_)) => {}
                body => panic!("unexpected watch setup frame {body:?}"),
            }
        }
        watchers.push(watch);
    }
    let mut excess = ControlWire::connect(runtime, WATCH_CAPABILITY).await;
    excess
        .request("watch", Body::SnapshotWatch(SnapshotWatch::default()))
        .await;
    assert!(
        matches!(excess.next().await.body, Some(Body::Error(fault)) if fault.code == "LimitExceeded")
    );
    watchers
}

async fn assert_watch_disconnected(watch: &mut ControlWire) {
    timeout(Duration::from_secs(2), async {
        let mut last_sequence = None;
        loop {
            match watch.reader.next_envelope().await {
                Ok(None) | Err(FrameError::Truncated) => break,
                Ok(Some(frame)) if matches!(&frame.body, Some(Body::SnapshotDelta(_))) => {
                    let sequence = frame.sequence.unwrap();
                    assert!(last_sequence.is_none_or(|previous| sequence == previous + 1));
                    last_sequence = Some(sequence);
                }
                Ok(Some(frame)) if matches!(&frame.body, Some(Body::Error(fault)) if fault.code == "ResyncRequired") => {
                    assert!(watch.reader.next_envelope().await.unwrap().is_none());
                    break;
                }
                result => panic!("unexpected stalled watch result {result:?}"),
            }
        }
        assert!(last_sequence.is_some(), "watch closed before receiving any deltas");
    }).await.unwrap();
}
