use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentRestoreMode, Attach, PaneRestoreResult, PaneRestoreStatus, RestoreWorkspace,
    ViewSelection, WorkspaceRestored,
};
use corvid_protocol::{
    ATTACH_RECOVERY_CAPABILITY, Frame, ProtocolFault, RESTORE_AGENTS_CAPABILITY, Session,
    decode_envelope,
};
use uuid::Uuid;

use crate::InteractiveConnection;
use crate::endpoint_time::timeout;

#[derive(Debug)]
pub struct AttachRecovery {
    pub view: ViewSelection,
    pub pane: PaneRestoreResult,
}

impl AttachRecovery {
    pub fn request(session: &Session, attach: &mut Attach) -> bool {
        let supported = Self::supported(session);
        if supported {
            attach
                .terminal_capabilities
                .push(ATTACH_RECOVERY_CAPABILITY.into());
        }
        supported
    }

    pub fn supported(session: &Session) -> bool {
        [ATTACH_RECOVERY_CAPABILITY, RESTORE_AGENTS_CAPABILITY]
            .iter()
            .all(|cap| session.capabilities.iter().any(|name| name == cap))
    }

    pub fn should_resume(&self) -> bool {
        matches!(
            self.pane.status(),
            PaneRestoreStatus::Available | PaneRestoreStatus::Starting
        )
    }

    pub fn from_reply(attach: &Attach, mut reply: WorkspaceRestored) -> Result<Self> {
        ensure!(reply.panes.len() == 1, "expected a single recovery pane");
        let pane = &reply.panes[0];
        let view = pane.location.clone().context("missing recovery pane")?;
        validate_result(
            &RestoreWorkspace {
                workspace_id: attach
                    .workspace_id
                    .clone()
                    .unwrap_or_else(|| view.workspace_id.clone()),
                pane_id: attach.pane_id.clone(),
                mode: AgentRestoreMode::Inspect as i32,
            },
            &reply,
        )?;
        ensure!(
            attach.tab_id.as_ref().is_none_or(|id| *id == view.tab_id)
                && pane.terminal_id.is_none()
                && matches!(
                    pane.status(),
                    PaneRestoreStatus::Available
                        | PaneRestoreStatus::Failed
                        | PaneRestoreStatus::Starting
                ),
            "invalid attach recovery offer"
        );
        Ok(Self {
            view,
            pane: reply.panes.remove(0),
        })
    }
}

impl std::fmt::Display for AttachRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = &self.pane.error {
            return f.write_str(&error.message);
        }
        if self.pane.status() == PaneRestoreStatus::Starting {
            return f.write_str("The saved agent conversation is being restored.");
        }
        f.write_str("A saved agent conversation is available in this pane.")
    }
}

impl std::error::Error for AttachRecovery {}

impl InteractiveConnection {
    /// Before attaching, restore exact saved conversations and wait for pane results.
    /// The daemon continues accepted attempts if this connection is lost.
    pub async fn restore_workspace(
        &mut self,
        mut request: RestoreWorkspace,
    ) -> Result<WorkspaceRestored> {
        ensure!(
            self.session()
                .capabilities
                .iter()
                .any(|cap| cap == RESTORE_AGENTS_CAPABILITY),
            ProtocolFault::new(
                "UnsupportedCapability",
                "daemon does not support agent session restoration"
            )
        );
        let wait = request.mode != AgentRestoreMode::Inspect as i32;
        timeout(Duration::from_secs(45), async {
            loop {
                let id = self.send(Body::RestoreWorkspace(request.clone())).await?;
                let Frame::Structured(bytes) = self.receive().await? else {
                    anyhow::bail!("unexpected render during workspace restoration");
                };
                let reply = decode_envelope(&bytes)?;
                self.session().validate_scope(&reply)?;
                ensure!(
                    reply.request_id.as_deref() == Some(id.as_str()),
                    "restore reply ID mismatch"
                );
                let result = match reply.body {
                    Some(Body::WorkspaceRestored(result)) => result,
                    Some(Body::Error(error)) => return Err(ProtocolFault::from_wire(&error).into()),
                    _ => anyhow::bail!("unexpected workspace restoration reply"),
                };
                validate_result(&request, &result)?;
                if !wait
                    || result
                        .panes
                        .iter()
                        .all(|pane| pane.status() != PaneRestoreStatus::Starting)
                {
                    return Ok(result);
                }
                // Polling must never launch again if a completed terminal subsequently exits.
                request.mode = AgentRestoreMode::Inspect as i32;
                crate::endpoint_time::wait_until(
                    std::time::Instant::now() + Duration::from_millis(100),
                )
                .await?;
            }
        })
        .await
        .context("workspace restoration is still running; inspect or retry after reconnecting")?
    }
}

fn validate_result(request: &RestoreWorkspace, result: &WorkspaceRestored) -> Result<()> {
    let mut panes = HashSet::new();
    ensure!(
        !result.panes.is_empty() && result.panes.len() <= 1024,
        "invalid restoration result size"
    );
    for pane in &result.panes {
        let location = pane
            .location
            .as_ref()
            .context("restore result has no pane")?;
        ensure!(
            location.workspace_id == request.workspace_id
                && request
                    .pane_id
                    .as_ref()
                    .is_none_or(|id| id == &location.pane_id)
                && [&location.workspace_id, &location.tab_id, &location.pane_id]
                    .into_iter()
                    .all(|id| Uuid::parse_str(id).is_ok_and(|id| !id.is_nil()))
                && panes.insert(&location.pane_id),
            "restore result targets an invalid pane"
        );
        let status = PaneRestoreStatus::try_from(pane.status)?;
        ensure!(
            status != PaneRestoreStatus::Unspecified,
            "unknown restoration state"
        );
        ensure!(
            pane.terminal_id
                .as_ref()
                .is_none_or(|id| Uuid::parse_str(id).is_ok_and(|id| !id.is_nil())),
            "invalid restored terminal ID"
        );
        ensure!(
            (status == PaneRestoreStatus::Failed) == pane.error.is_some(),
            "invalid restoration error state"
        );
        if let Some(error) = &pane.error {
            ensure!(
                ProtocolFault::from_wire(error).code != "InvalidMessage",
                "invalid restoration error"
            );
        }
    }
    Ok(())
}
