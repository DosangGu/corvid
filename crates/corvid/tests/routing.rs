#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::time::Duration;

use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, FrameApplied, InputChunk, LaunchCommand, ResizeViewport, RouteToken,
    Viewport,
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
            launch: argv.map(|argv| LaunchCommand { argv, cwd: None }),
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

fn shell(script: &str) -> Vec<Vec<u8>> {
    ["/bin/sh", "-c", script]
        .map(|arg| arg.as_bytes().to_vec())
        .to_vec()
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
