#[cfg(feature = "compio")]
use compio::runtime::{JoinHandle, spawn};
use corvid_async::mpsc;
use corvid_protocol::FrameError;
use corvid_protocol::wire::Envelope;
#[cfg(not(feature = "compio"))]
use tokio::task::{JoinHandle, spawn};

use crate::platform::{FrameReader, ReadHalf};

/// Keep partially read frames intact while the connection services other events.
pub(crate) struct Incoming {
    pub(crate) requests: mpsc::Receiver<Result<Envelope, FrameError>>,
    _task: JoinHandle<()>,
}

impl Incoming {
    pub(crate) fn start(mut reader: FrameReader<ReadHalf>) -> Self {
        let (sender, requests) = mpsc::channel(4);
        let task = spawn(async move {
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
        Self {
            requests,
            _task: task,
        }
    }
}

#[cfg(not(feature = "compio"))]
impl Drop for Incoming {
    fn drop(&mut self) {
        self._task.abort();
    }
}
