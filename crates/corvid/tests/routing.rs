#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use corvid_protocol::snapshot::{SNAPSHOT_CAPABILITY, WATCH_CAPABILITY};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentBadge, AgentInfo, AgentProcessState, AgentProvider, Attach, AttachRole,
    ClaimWorkspaceWriter, EntityKind, Envelope, FrameApplied, GetSnapshot, InputChunk,
    LaunchCommand, ListClients, PasteBegin, PasteChunk, PasteEnd, ResizeViewport, RouteToken,
    SelectView, SnapshotWatch, ViewSelection, Viewport,
};
use corvid_protocol::{
    Frame, FrameReader, FrameRole, FrameWriter, PeerRole, Session, check_hello_result,
    decode_envelope, hello,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{sleep, timeout};

use common::{WAIT, command, daemon, start_daemon};

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

    async fn attach_target(runtime: &Path, attach: Attach) -> Self {
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
        let attach = Attach {
            viewport: Some(Viewport { cols: 80, rows: 24 }),
            terminal_capabilities: vec!["legacy_vt.v1".into()],
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

    async fn input(&mut self, route: RouteToken, sequence: u64, bytes: &[u8]) {
        self.send(Body::InputChunk(InputChunk {
            route_token: Some(route),
            input_sequence: sequence,
            data: bytes.to_vec(),
        }))
        .await;
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
    let mut command = command(&runtime);
    command.env("PATH", temporary.path());
    let _daemon = start_daemon(command, &runtime, &temporary.path().join("state")).await;
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
        let one_process = one.process_identity.unwrap();
        let two_process = two.process_identity.unwrap();
        assert_ne!(one_process.pid, two_process.pid);
        assert_eq!(one_process.boot_id, two_process.boot_id);
        assert!(one_process.start_ticks > 0);
        assert!(one_process.foreground_group.is_some());

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
    })
    .await
    .unwrap();
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

#[tokio::test]
async fn snapshot_watch_delivers_committed_routes_and_detach_in_order() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let mut watch = ControlWire::connect(&runtime, WATCH_CAPABILITY).await;
        watch
            .request("watch", Body::SnapshotWatch(SnapshotWatch {}))
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
            .request("snapshot", Body::GetSnapshot(GetSnapshot {}))
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
async fn stalled_render_ack_releases_writer_while_pty_and_control_progress() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;
    timeout(WAIT, async {
        let progress = temporary.path().join("progress");
        let mut argv = shell(
            "i=0; while :; do i=$((i+1)); printf '%s\\n' \"$i\" >> \"$1\"; printf '%32768d\\n' \"$i\"; sleep .02; done",
        );
        argv.push(b"progress-fixture".to_vec());
        argv.push(progress.as_os_str().as_encoded_bytes().to_vec());
        let mut stalled = Tui::attach(&runtime, Some(argv)).await;
        let Some(Frame::Render(_)) = stalled.reader.next_frame().await.unwrap() else {
            panic!("expected initial render");
        };
        sleep(Duration::from_millis(150)).await;
        let ping = timeout(Duration::from_secs(2), command(&runtime).args(["control", "ping"]).output())
            .await.unwrap().unwrap();
        assert!(ping.status.success());
        let mut independent = Tui::attach_target(&runtime, Attach {
            create_workspace: Some("Independent".into()),
            launch: Some(echo_fixture("INDEPENDENT")),
            ..Default::default()
        }).await;
        let mut independent_output = Vec::new();
        wait_for_text(&mut independent, &mut independent_output, "INDEPENDENT-READY").await;
        independent.input(independent.route.clone(), 1, b"OK\n").await;
        wait_for_text(&mut independent, &mut independent_output, "INDEPENDENT=OK").await;
        assert!(stalled.reader.next_frame().await.unwrap().is_none());
        let count = std::fs::read_to_string(progress).unwrap().lines().count();
        assert!(count > 20, "PTY stalled while waiting for render acknowledgement");

        let mut reattached = Tui::attach(&runtime, None).await;
        assert_eq!(reattached.route.terminal_id, stalled.route.terminal_id);
        assert_ne!(reattached.route.client_id, stalled.route.client_id);
        let Some(Frame::Render(frame)) = reattached.reader.next_frame().await.unwrap() else {
            panic!("expected full redraw");
        };
        assert!(frame.header.base_frame_id.is_none());
    }).await.unwrap();
}
