use std::collections::{BTreeSet, VecDeque};

use anyhow::{Result, ensure};
use corvid_companion::PRESENTATION_ATTENTION_LIMIT;
use corvid_protocol::wire::{
    AgentProvider, AttentionReason, CompanionConnection, InputRequestKind, InputRequestStatus,
    PresentationAttention, PresentedAttention, PresentedRunKey,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_SEEN: usize = 4096;
const MAX_HISTORY: usize = 128;
const MAX_AGE_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub host: Uuid,
    pub epoch: Uuid,
    pub run: Uuid,
    pub attention: Uuid,
}

impl Key {
    pub fn run(&self) -> PresentedRunKey {
        PresentedRunKey {
            host_id: self.host.to_string(),
            remote_epoch: self.epoch.to_string(),
            agent_run_id: self.run.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Notice {
    #[serde(default)]
    pub summary: bool,
    pub key: Key,
    pub title: String,
    pub body: String,
    pub created_ms: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub input: bool,
    pub responses: bool,
    pub failures: bool,
    pub sound: bool,
    pub paused_until_ms: u64,
    pub start_at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            input: true,
            responses: true,
            failures: true,
            sound: false,
            paused_until_ms: 0,
            start_at_login: false,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Ledger {
    pub schema_version: u32,
    epochs: VecDeque<Uuid>,
    seen: VecDeque<Key>,
    pub history: VecDeque<Notice>,
    pub settings: Settings,
}

pub struct Observation {
    pub notices: Vec<Notice>,
    pub changed: bool,
}

impl Ledger {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version <= 1
                && self.epochs.len() <= 8
                && self.seen.len() <= MAX_SEEN
                && self.history.len() <= MAX_HISTORY,
            "unsupported or damaged Agent Deck notification storage"
        );
        ensure!(
            self.history
                .iter()
                .all(|item| item.title.len() <= 160 && item.body.len() <= 256),
            "invalid notification history"
        );
        Ok(())
    }

    pub fn observe(
        &mut self,
        host: Uuid,
        snapshot: &PresentationAttention,
        now: u64,
    ) -> Result<Observation> {
        ensure!(
            snapshot.attention.len() <= PRESENTATION_ATTENTION_LIMIT,
            "too many presentation alerts"
        );
        if snapshot.connection != CompanionConnection::Ready as i32 {
            return Ok(Observation {
                notices: Vec::new(),
                changed: false,
            });
        }
        let epoch = id(snapshot.remote_epoch.as_deref().unwrap_or_default())?;
        id(snapshot.control_generation.as_deref().unwrap_or_default())?;
        let baseline = !self.epochs.contains(&epoch);
        // Validate the complete response before recording or displaying any item.
        let entries: Vec<_> = snapshot
            .attention
            .iter()
            .map(|item| notice(host, epoch, item))
            .collect::<Result<_>>()?;
        if baseline {
            if self.epochs.len() == 8 {
                self.epochs.pop_front();
            }
            self.epochs.push_back(epoch);
        }
        let mut seen: BTreeSet<_> = self.seen.iter().cloned().collect();
        let mut fresh = Vec::new();
        let mut changed = baseline || self.schema_version != 1;
        for (item, notice) in snapshot.attention.iter().zip(entries).rev() {
            if !seen.insert(notice.key.clone()) {
                continue;
            }
            changed = true;
            if self.seen.len() == MAX_SEEN {
                self.seen.pop_front();
            }
            self.seen.push_back(notice.key.clone());
            if self.history.len() == MAX_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(notice.clone());
            let relevant = match AttentionReason::try_from(item.reason)? {
                AttentionReason::Input => {
                    self.settings.input
                        && !matches!(
                            item.request_status
                                .and_then(|status| InputRequestStatus::try_from(status).ok()),
                            Some(InputRequestStatus::Resolved | InputRequestStatus::Cancelled)
                        )
                }
                AttentionReason::Response => self.settings.responses,
                AttentionReason::Failure => self.settings.failures,
                _ => false,
            };
            if !baseline
                && relevant
                && self.settings.enabled
                && now >= self.settings.paused_until_ms
                && notice.created_ms <= now
                && now - notice.created_ms <= MAX_AGE_MS
            {
                fresh.push(notice);
            }
        }
        self.schema_version = 1;
        Ok(Observation {
            notices: fresh,
            changed,
        })
    }
}

fn notice(host: Uuid, epoch: Uuid, item: &PresentedAttention) -> Result<Notice> {
    if let Some(status) = item.request_status {
        ensure!(
            InputRequestStatus::try_from(status)? != InputRequestStatus::Unspecified,
            "invalid request status"
        );
    }
    if let Some(kind) = item.request_kind {
        ensure!(
            InputRequestKind::try_from(kind)? != InputRequestKind::Unspecified,
            "invalid request kind"
        );
    }
    let run = item
        .run
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("alert has no run"))?;
    ensure!(
        id(&run.host_id)? == host && id(&run.remote_epoch)? == epoch,
        "alert belongs to another host or epoch"
    );
    let key = Key {
        host,
        epoch,
        run: id(&run.agent_run_id)?,
        attention: id(&item.attention_id)?,
    };
    let provider = match AgentProvider::try_from(item.provider)? {
        AgentProvider::Claude => "Claude",
        AgentProvider::Codex => "Codex",
        _ => anyhow::bail!("unknown notification provider"),
    };
    let title = match AttentionReason::try_from(item.reason)? {
        AttentionReason::Input
            if item.request_status == Some(InputRequestStatus::Pending as i32) =>
        {
            match item
                .request_kind
                .and_then(|kind| InputRequestKind::try_from(kind).ok())
            {
                Some(InputRequestKind::Permission | InputRequestKind::PlanApproval) => {
                    format!("{provider} needs your approval")
                }
                _ => format!("{provider} needs your input"),
            }
        }
        AttentionReason::Input => format!("Check the {provider} terminal"),
        AttentionReason::Response => format!("{provider} has a new response"),
        AttentionReason::Failure => format!("{provider} reported an error"),
        _ => anyhow::bail!("unknown notification reason"),
    };
    Ok(Notice {
        summary: false,
        body: format!(
            "Host {} · Agent {}",
            &host.simple().to_string()[..8],
            &key.run.simple().to_string()[..8]
        ),
        key,
        title,
        created_ms: item.created_unix_ms,
    })
}

fn id(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value)?;
    ensure!(!id.is_nil(), "nil notification identity");
    Ok(id)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> (Uuid, PresentationAttention) {
        let host = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        (
            host,
            PresentationAttention {
                connection: CompanionConnection::Ready as i32,
                remote_epoch: Some(epoch.to_string()),
                control_generation: Some(Uuid::new_v4().to_string()),
                ..Default::default()
            },
        )
    }
    fn alert(host: Uuid, snapshot: &PresentationAttention, time: u64) -> PresentedAttention {
        PresentedAttention {
            run: Some(PresentedRunKey {
                host_id: host.to_string(),
                remote_epoch: snapshot.remote_epoch.clone().unwrap(),
                agent_run_id: Uuid::new_v4().to_string(),
            }),
            attention_id: Uuid::new_v4().to_string(),
            created_unix_ms: time,
            reason: AttentionReason::Response as i32,
            provider: AgentProvider::Claude as i32,
            ..Default::default()
        }
    }
    #[test]
    fn seeds_then_notifies_once_across_reconnect_and_restart() {
        let (host, mut snapshot) = snapshot();
        let mut ledger = Ledger::default();
        snapshot.attention.push(alert(host, &snapshot, 1000));
        assert!(
            ledger
                .observe(host, &snapshot, 1000)
                .unwrap()
                .notices
                .is_empty()
        );
        snapshot.attention.push(alert(host, &snapshot, 1100));
        assert_eq!(
            ledger.observe(host, &snapshot, 1100).unwrap().notices.len(),
            1
        );
        let mut restored: Ledger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        snapshot.control_generation = Some(Uuid::new_v4().to_string());
        let observed = restored.observe(host, &snapshot, 1200).unwrap();
        assert!(observed.notices.is_empty());
        assert!(!observed.changed);
    }
    #[test]
    fn snoozed_and_resolved_requests_never_replay() {
        let (host, mut snapshot) = snapshot();
        let mut ledger = Ledger::default();
        ledger.observe(host, &snapshot, 1000).unwrap();
        ledger.settings.paused_until_ms = 2000;
        snapshot.attention.push(alert(host, &snapshot, 1100));
        assert!(
            ledger
                .observe(host, &snapshot, 1100)
                .unwrap()
                .notices
                .is_empty()
        );
        assert!(
            ledger
                .observe(host, &snapshot, 2100)
                .unwrap()
                .notices
                .is_empty()
        );
        let mut request = alert(host, &snapshot, 2200);
        request.reason = AttentionReason::Input as i32;
        request.request_status = Some(InputRequestStatus::Resolved as i32);
        snapshot.attention.push(request);
        assert!(
            ledger
                .observe(host, &snapshot, 2200)
                .unwrap()
                .notices
                .is_empty()
        );
    }
    #[test]
    fn refuses_cross_host_and_unknown_events_before_mutation() {
        let (host, mut snapshot) = snapshot();
        let mut ledger = Ledger::default();
        snapshot
            .attention
            .push(alert(Uuid::new_v4(), &snapshot, 1000));
        assert!(ledger.observe(host, &snapshot, 1000).is_err());
        assert!(ledger.seen.is_empty() && ledger.epochs.is_empty());
        snapshot.attention[0] = alert(host, &snapshot, 1000);
        snapshot.attention[0].reason = 999;
        assert!(ledger.observe(host, &snapshot, 1000).is_err());
        assert!(ledger.history.is_empty());
    }
}
