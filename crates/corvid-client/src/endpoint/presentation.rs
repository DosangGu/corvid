use anyhow::{Context, Result};
use corvid_protocol::wire::Envelope;
use corvid_protocol::{Frame, FrameError, Session, decode_envelope};

use super::native::{self, PresentationAddress, ReadQueue, WriteQueue};

/// A private presentation connection owned by the Compio endpoint driver.
pub struct CompioPresentation {
    session: Session,
    wire: Option<(ReadQueue, WriteQueue)>,
}

impl CompioPresentation {
    pub async fn connect(address: &PresentationAddress, capability: &str) -> Result<Self> {
        let (session, receive, send) = native::presentation(address, capability).await?;
        Ok(Self {
            session,
            wire: Some((receive, send)),
        })
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Cancelling an exchange retires the connection.
    pub async fn exchange(&mut self, request: Envelope) -> Result<Envelope> {
        let (mut receive, send) = self
            .wire
            .take()
            .context("presentation connection is closed")?;
        native::send(&send, request).await?;
        let frame = receive
            .recv()
            .await
            .context("companion closed during command")??;
        let Frame::Structured(bytes) = frame else {
            return Err(FrameError::WrongRole.into());
        };
        let reply = decode_envelope(&bytes)?;
        self.wire = Some((receive, send));
        Ok(reply)
    }
}
