use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use corvid_protocol::snapshot::{MAX_CHUNK_ENTITIES, MAX_SNAPSHOT_BYTES};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Envelope, HistoricalRun, SnapshotBegin, SnapshotChunk, SnapshotEnd, SnapshotEntity, WatchReady,
};
use corvid_protocol::{COMMAND_TIMEOUT, FrameWriter, ProtocolFault, Session};
use prost::Message;
use tokio::io::AsyncWrite;
use tokio::net::unix::OwnedWriteHalf;
use tokio::time::timeout;

use crate::publication::{BODY_BUDGET, Capture, Delta, Watch, resync};
use crate::workspaces::Workspaces;

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct LiveWatch {
    pub(crate) watch: Watch,
    pub(crate) sequence: u64,
}

pub(crate) async fn transfer(
    capture: Capture,
    history: Option<Arc<Vec<HistoricalRun>>>,
    request_id: Option<String>,
    session: &Session,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    workspaces: &Workspaces,
) -> Result<Option<LiveWatch>> {
    let Capture {
        sequence,
        entities,
        watch,
    } = capture;
    let subscription = watch.as_ref().map(|watch| watch.id.clone());
    let mut stream = Stream {
        session,
        writer,
        request_id,
        subscription,
    };
    let outcome = timeout(
        SNAPSHOT_TIMEOUT,
        stream.start(sequence, entities, history, watch, workspaces),
    )
    .await?;
    if let Err(error) = &outcome
        && let Some(fault) = error.downcast_ref::<ProtocolFault>()
    {
        stream.failure(fault).await?;
    }
    outcome
}

struct Stream<'a> {
    session: &'a Session,
    writer: &'a mut FrameWriter<OwnedWriteHalf>,
    request_id: Option<String>,
    subscription: Option<String>,
}

impl Stream<'_> {
    async fn start(
        &mut self,
        sequence: u64,
        entities: Vec<Arc<SnapshotEntity>>,
        history: Option<Arc<Vec<HistoricalRun>>>,
        watch: Option<Watch>,
        workspaces: &Workspaces,
    ) -> Result<Option<LiveWatch>> {
        let chunks = tokio::task::spawn_blocking(move || {
            let history = history.as_deref().map(Vec::as_slice).unwrap_or_default();
            chunks(&entities, history)
        })
        .await??;
        self.base_frame(
            sequence,
            Body::SnapshotBegin(SnapshotBegin {
                chunks: chunks.len() as u32,
                capabilities: self.session.capabilities.clone(),
            }),
            watch.as_ref(),
        )
        .await?;
        for chunk in chunks {
            self.base_frame(sequence, Body::SnapshotChunk(chunk), watch.as_ref())
                .await?;
        }
        self.base_frame(sequence, Body::SnapshotEnd(SnapshotEnd {}), watch.as_ref())
            .await?;
        let Some(mut watch) = watch else {
            return Ok(None);
        };

        let watermark = workspaces.public_sequence()?;
        let mut sent = sequence;
        while sent < watermark {
            let delta = watch.next().await?;
            send_delta(self.session, self.writer, &watch, &mut sent, &delta).await?;
        }
        watch.check()?;
        let ready = event(
            self.session,
            &watch.id,
            Some(sent),
            Body::WatchReady(WatchReady {}),
        );
        write_frame(self.writer, &ready, Some(&watch)).await?;
        Ok(Some(LiveWatch {
            watch,
            sequence: sent,
        }))
    }

    async fn base_frame(&mut self, sequence: u64, body: Body, watch: Option<&Watch>) -> Result<()> {
        let mut frame = self.session.envelope(self.request_id.clone(), body);
        frame.subscription_id = self.subscription.clone();
        frame.sequence = Some(sequence);
        write_frame(self.writer, &frame, watch).await
    }

    async fn failure(&mut self, fault: &ProtocolFault) -> Result<()> {
        let mut reply = self.session.envelope(
            self.request_id.clone(),
            Body::Error(fault.clone().into_wire()),
        );
        reply.subscription_id = self.subscription.clone();
        write_frame(self.writer, &reply, None).await
    }
}

pub(crate) async fn send_delta(
    session: &Session,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    watch: &Watch,
    sequence: &mut u64,
    delta: &Delta,
) -> Result<()> {
    if sequence.checked_add(1) != Some(delta.sequence) {
        return Err(resync().into());
    }
    let frame = event(
        session,
        &watch.id,
        Some(delta.sequence),
        Body::SnapshotDelta(delta.body()),
    );
    write_frame(writer, &frame, Some(watch)).await?;
    *sequence = delta.sequence;
    Ok(())
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    frame: &Envelope,
    watch: Option<&Watch>,
) -> Result<()> {
    let write = timeout(COMMAND_TIMEOUT, writer.envelope(frame));
    if let Some(watch) = watch {
        watch.check()?;
        tokio::select! {
            biased;
            // The writer may have sent a prefix. Close the stream instead of
            // appending a structured error inside an incomplete frame.
            () = watch.invalidated() => bail!("snapshot watch invalidated during frame write"),
            result = write => result??,
        }
    } else {
        write.await??;
    }
    Ok(())
}

pub(crate) fn event(session: &Session, id: &str, sequence: Option<u64>, body: Body) -> Envelope {
    let mut frame = session.envelope(None, body);
    frame.subscription_id = Some(id.to_owned());
    frame.sequence = sequence;
    frame
}

enum SnapshotItem<'a> {
    Entity(&'a SnapshotEntity),
    Historical(&'a HistoricalRun),
}

impl SnapshotItem<'_> {
    fn encoded_len(&self) -> usize {
        match self {
            Self::Entity(entity) => entity.encoded_len(),
            Self::Historical(run) => run.encoded_len(),
        }
    }

    fn append(self, chunk: &mut SnapshotChunk) {
        match self {
            Self::Entity(entity) => chunk.entities.push(entity.clone()),
            Self::Historical(run) => chunk.history.push(run.clone()),
        }
    }
}

fn chunks(
    entities: &[Arc<SnapshotEntity>],
    history: &[HistoricalRun],
) -> Result<Vec<SnapshotChunk>, ProtocolFault> {
    let too_large = || ProtocolFault::new("LimitExceeded", "snapshot exceeds its transfer bound");
    let mut chunks = Vec::new();
    let mut current = SnapshotChunk::default();
    let mut chunk_size = 8;
    let mut total = 0;
    let items = entities
        .iter()
        .map(|entity| SnapshotItem::Entity(entity))
        .chain(history.iter().map(SnapshotItem::Historical));
    for item in items {
        let size = item.encoded_len() + 6;
        if size + 8 > BODY_BUDGET {
            return Err(too_large());
        }
        if chunk_size + size > BODY_BUDGET
            || current.entities.len() + current.history.len() == MAX_CHUNK_ENTITIES
        {
            current.index = chunks.len() as u32;
            chunks.push(std::mem::take(&mut current));
            total += chunk_size;
            chunk_size = 8;
        }
        chunk_size += size;
        if total + chunk_size > MAX_SNAPSHOT_BYTES {
            return Err(too_large());
        }
        item.append(&mut current);
    }
    if !current.entities.is_empty() || !current.history.is_empty() {
        current.index = chunks.len() as u32;
        chunks.push(current);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use corvid_protocol::wire::WorkspaceState;
    use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
    use corvid_protocol::{FRAME_PREAMBLE, FrameRole};
    use tokio::io::{AsyncReadExt, duplex};

    use super::*;
    use crate::publication::{Group, Publication};

    #[tokio::test]
    async fn watch_overflow_closes_an_incomplete_frame_before_the_write_deadline() {
        let mut publication = Publication::default();
        let watch = publication.capture(true).unwrap().watch.unwrap();
        let (server, mut client) = duplex(64);
        let mut writer = FrameWriter::start(server, FrameRole::StructuredOnly)
            .await
            .unwrap();
        let frame = Envelope {
            body: Some(Body::SnapshotChunk(SnapshotChunk {
                entities: vec![workspace("initial".repeat(1024))],
                ..Default::default()
            })),
            ..Default::default()
        };

        let payload_len = frame.encoded_len();
        let task =
            tokio::spawn(async move { write_frame(&mut writer, &frame, Some(&watch)).await });
        let mut prefix = [0; 9];
        client.read_exact(&mut prefix).await.unwrap();
        assert_eq!(prefix[..4], FRAME_PREAMBLE);
        assert_eq!(
            u32::from_be_bytes(prefix[4..8].try_into().unwrap()) as usize,
            payload_len + 1
        );

        for index in 0..96 {
            let label = format!("{index}{}", "x".repeat(64 * 1024));
            publication.commit(vec![(Group::Layout, vec![workspace(label)])]);
        }
        let error = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "snapshot watch invalidated during frame write"
        );

        let mut tail = Vec::new();
        client.read_to_end(&mut tail).await.unwrap();
        assert!(
            tail.len() < payload_len,
            "an invalidated frame must end with EOF"
        );
    }

    fn workspace(label: String) -> SnapshotEntity {
        SnapshotEntity {
            body: Some(EntityBody::Workspace(WorkspaceState {
                workspace_id: "work".to_owned(),
                label,
                tabs: Vec::new(),
            })),
        }
    }
}
