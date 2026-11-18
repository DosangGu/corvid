use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, ensure};
use tokio::time::{Instant, sleep};

use corvid_agent_deck_core::notifications::{Key, Notice};

#[cfg(not(windows))]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

const LIMIT: usize = 4096;

/// Serialize attempted OS delivery across the per-host Deck processes.
pub struct Delivery(StateDirectory);

impl Delivery {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self(StateDirectory::create_all(
            &root.join("agent-deck-delivery"),
        )?))
    }

    pub async fn claim(&self, notices: Vec<Notice>) -> Result<Vec<Notice>> {
        if notices.is_empty() {
            return Ok(notices);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let _lock = loop {
            match self.0.lock_storage() {
                Ok(lock) => break lock,
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => sleep(Duration::from_millis(25)).await,
            }
        };
        let mut saved: VecDeque<Key> = self
            .0
            .read_private_file("attempted.json", 1024 * 1024)?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?
            .unwrap_or_default();
        ensure!(
            saved.len() <= LIMIT,
            "invalid notification delivery history"
        );
        let mut seen: BTreeSet<_> = saved.iter().cloned().collect();
        let claimed: Vec<_> = notices
            .into_iter()
            .filter(|notice| {
                if !seen.insert(notice.key.clone()) {
                    return false;
                }
                if saved.len() == LIMIT {
                    saved.pop_front();
                }
                saved.push_back(notice.key.clone());
                true
            })
            .collect();
        if !claimed.is_empty() {
            // Commit before touching the OS; a crash cannot cause another Deck to replay it.
            self.0
                .replace_private_file("attempted.json", &serde_json::to_vec(&saved)?)?;
        }
        Ok(claimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn two_decks_attempt_a_remote_notice_once_and_retain_the_claim_on_restart() -> Result<()>
    {
        #[cfg(not(windows))]
        let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap())?;
        #[cfg(windows)]
        let temporary = tempfile::tempdir()?;
        let first = Delivery::open(temporary.path())?;
        let second = Delivery::open(temporary.path())?;
        let notice = Notice {
            summary: false,
            key: Key {
                host: Uuid::new_v4(),
                epoch: Uuid::new_v4(),
                run: Uuid::new_v4(),
                attention: Uuid::new_v4(),
            },
            title: "Codex needs your input".into(),
            body: String::new(),
            created_ms: 1,
            focus_error: None,
        };
        let (a, b) = tokio::join!(
            first.claim(vec![notice.clone()]),
            second.claim(vec![notice.clone()])
        );
        assert_eq!(a?.len() + b?.len(), 1);
        drop(first);
        drop(second);
        assert!(
            Delivery::open(temporary.path())?
                .claim(vec![notice])
                .await?
                .is_empty()
        );
        Ok(())
    }
}
