#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::time::Duration;

use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, FrameApplied, InputChunk, LaunchCommand, PasteBegin, PasteChunk, PasteEnd,
    ResizeViewport, RouteToken, Viewport,
};
use corvid_protocol::{
    Frame, FrameReader, FrameRole, FrameWriter, PeerRole, Session, check_hello_result,
    decode_envelope, hello,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{sleep, timeout};

use common::{WAIT, command, daemon};

struct Tui {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    session: Session,
    route: RouteToken,
}

impl Tui {
    async fn attach(runtime: &Path, argv: Option<Vec<Vec<u8>>>) -> Self {
        let launch = argv.map(|argv| LaunchCommand { argv, cwd: None });
        Self::attach_command(runtime, launch).await
    }

    async fn attach_command(runtime: &Path, launch: Option<LaunchCommand>) -> Self {
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
            workspace_id: None,
            viewport: Some(Viewport { cols: 80, rows: 24 }),
            terminal_capabilities: vec!["legacy_vt.v1".into()],
            requested_role: AttachRole::Writer as i32,
            launch,
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
        }
    }

    async fn send(&mut self, body: Body) {
        self.writer
            .envelope(&self.session.envelope(Some("request".into()), body))
            .await
            .unwrap();
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
