use std::thread;

use anyhow::{Context, Result, ensure};
use corvid_presentation::ipc::Completion;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, FocusDesktopAgent, PresentedRunKey};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, ProtocolFault};
use eframe::egui;
use tokio::sync::mpsc;
use uuid::Uuid;

pub(crate) struct DesktopLink {
    pid: u32,
    requests: mpsc::Sender<(Uuid, PresentedRunKey)>,
    pub results: mpsc::Receiver<Completion>,
}

impl DesktopLink {
    pub(crate) fn start(pid: u32, ctx: egui::Context) -> Result<Self> {
        let (requests, incoming) = mpsc::channel(1);
        let (completed, results) = mpsc::channel(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        thread::Builder::new()
            .name("corvid-agent-board-parent".into())
            .spawn(move || {
                let _ = runtime.block_on(bridge(incoming, completed, &ctx));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            })?;
        Ok(Self {
            pid,
            requests,
            results,
        })
    }

    pub(crate) fn focus(&self, id: Uuid, run: PresentedRunKey) -> bool {
        #[cfg(windows)]
        // The board owns the foreground after the click. Let the requesting
        // Desktop window raise itself when the daemon completes the focus.
        if self.pid != 0 && self.pid != u32::MAX {
            // The PID is the launcher supplied on this private connection.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(self.pid);
            }
        }
        #[cfg(not(windows))]
        let _ = self.pid;
        self.requests.try_send((id, run)).is_ok()
    }
}

async fn bridge(
    mut requests: mpsc::Receiver<(Uuid, PresentedRunKey)>,
    completed: mpsc::Sender<Completion>,
    ctx: &egui::Context,
) -> Result<()> {
    let mut writer = FrameWriter::start(tokio::io::stdout(), FrameRole::StructuredOnly).await?;
    let mut reader = FrameReader::start(tokio::io::stdin(), FrameRole::StructuredOnly).await?;
    let write = async {
        while let Some((id, run)) = requests.recv().await {
            writer
                .envelope(&Envelope {
                    protocol_major: PROTOCOL_MAJOR,
                    request_id: Some(id.to_string()),
                    body: Some(Body::FocusDesktopAgent(FocusDesktopAgent {
                        run: Some(run),
                    })),
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    };
    let read = async {
        while let Some(reply) = reader.next_envelope().await? {
            ensure!(
                reply.protocol_major == PROTOCOL_MAJOR,
                "unsupported Desktop focus reply"
            );
            let id = Uuid::parse_str(
                reply
                    .request_id
                    .as_deref()
                    .context("missing focus reply ID")?,
            )?;
            let (message, failed) = match reply.body {
                Some(Body::DesktopAgentFocused(_)) => ("Focused in Corvid Desktop.".into(), false),
                Some(Body::Error(error)) => (ProtocolFault::from_wire(&error).to_string(), true),
                _ => anyhow::bail!("unexpected Desktop focus reply"),
            };
            #[cfg(target_os = "linux")]
            if !failed {
                // Some X11 window managers keep the board above the activated Desktop.
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            completed
                .send(Completion {
                    id,
                    message,
                    failed,
                    paired: None,
                })
                .await?;
            ctx.request_repaint();
        }
        Ok(())
    };
    tokio::select! { result = write => result, result = read => result }
}
