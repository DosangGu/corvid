use corvid_protocol::wire::Envelope;
use corvid_protocol::{FrameError, FrameReader};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Keep partially read frames intact while the connection services other events.
pub(crate) struct Incoming {
    pub(crate) requests: mpsc::Receiver<Result<Envelope, FrameError>>,
    task: JoinHandle<()>,
}

impl Incoming {
    pub(crate) fn start(mut reader: FrameReader<impl AsyncRead + Unpin + Send + 'static>) -> Self {
        let (sender, requests) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            loop {
                match reader.next_envelope().await {
                    Ok(Some(request)) => {
                        if sender.send(Ok(request)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
        });
        Self { requests, task }
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.task.abort();
    }
}
