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
const MAX_TITLE: usize = 160;
const MAX_BODY: usize = 256;
const MAX_HOST_LABEL: usize = 64;
const MAX_FOCUS_ERROR: usize = 256;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_error: Option<String>,
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
            self.history.iter().all(|item| item.title.len() <= MAX_TITLE
                && item.body.len() <= MAX_BODY
                && item
                    .focus_error
                    .as_ref()
                    .is_none_or(|error| error.len() <= MAX_FOCUS_ERROR)),
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
        self.observe_with_host(host, None, snapshot, now)
    }

    pub fn observe_with_host(
        &mut self,
        host: Uuid,
        host_label: Option<&str>,
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
            .map(|item| notice(host, host_label, epoch, item))
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
            let pending = item.reason == AttentionReason::Input as i32
                && item.request_status == Some(InputRequestStatus::Pending as i32);
            if relevant
                && self.settings.enabled
                && now >= self.settings.paused_until_ms
                && (pending || notice.created_ms.abs_diff(now) <= MAX_AGE_MS)
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

    pub fn record_focus(&mut self, mut notice: Notice, error: Option<String>) -> bool {
        debug_assert!(!notice.summary);
        let error = error.map(|mut error| {
            error.truncate(error.floor_char_boundary(MAX_FOCUS_ERROR));
            error
        });
        if let Some(entry) = self
            .history
            .iter_mut()
            .find(|entry| entry.key == notice.key)
        {
            if entry.focus_error == error {
                return false;
            }
            entry.focus_error = error;
        } else if error.is_some() {
            // The OS can retain a notice after it has rolled out of the history.
            if self.history.len() == MAX_HISTORY {
                self.history.pop_front();
            }
            notice.focus_error = error;
            self.history.push_back(notice);
        } else {
            return false;
        }
        true
    }
}

fn notice(
    host: Uuid,
    host_label: Option<&str>,
    epoch: Uuid,
    item: &PresentedAttention,
) -> Result<Notice> {
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
        AgentProvider::Antigravity => "Antigravity",
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
        body: format_notice_body(host, key.run, item.session_title.as_deref(), host_label),
        key,
        title,
        created_ms: item.created_unix_ms,
        focus_error: None,
    })
}

fn sanitize_text(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(
                    *ch,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn truncate_bytes_with_ellipsis(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    const ELLIPSIS: &str = "…";
    if max_bytes < ELLIPSIS.len() {
        return String::new();
    }
    let end = text.floor_char_boundary(max_bytes - ELLIPSIS.len());
    format!("{}{ELLIPSIS}", &text[..end])
}

fn format_notice_body(
    host: Uuid,
    run: Uuid,
    session_title: Option<&str>,
    host_label: Option<&str>,
) -> String {
    let title = session_title.and_then(sanitize_text);
    let host_label = host_label.and_then(sanitize_text);
    match (title, host_label) {
        (Some(title), Some(host_label)) => {
            let suffix = format!(
                " · {}",
                truncate_bytes_with_ellipsis(&host_label, MAX_HOST_LABEL)
            );
            let max_title = MAX_BODY.saturating_sub(suffix.len());
            format!(
                "{}{suffix}",
                truncate_bytes_with_ellipsis(&title, max_title)
            )
        }
        (Some(title), None) => truncate_bytes_with_ellipsis(&title, MAX_BODY),
        (None, Some(host_label)) => {
            let suffix = format!(" · Agent {}", &run.simple().to_string()[..8]);
            let max_host = MAX_BODY.saturating_sub(suffix.len());
            format!(
                "{}{suffix}",
                truncate_bytes_with_ellipsis(&host_label, max_host)
            )
        }
        (None, None) => format!(
            "Host {} · Agent {}",
            &host.simple().to_string()[..8],
            &run.simple().to_string()[..8]
        ),
    }
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
    fn stores_focus_errors_on_the_exact_notice_and_clears_them_after_success() {
        let (host, mut snapshot) = snapshot();
        snapshot.attention.push(alert(host, &snapshot, 1));
        snapshot.attention.push(alert(host, &snapshot, 1));
        let mut ledger = Ledger::default();
        let notices = ledger.observe(host, &snapshot, 1).unwrap().notices;
        let stored = serde_json::to_vec(&ledger).unwrap();
        let mut ledger: Ledger = serde_json::from_slice(&stored).unwrap();
        assert!(
            ledger
                .history
                .iter()
                .all(|entry| entry.focus_error.is_none())
        );
        let notice = notices[0].clone();
        assert!(ledger.record_focus(notice.clone(), Some("창".repeat(MAX_FOCUS_ERROR))));
        assert_eq!(
            ledger.history[0].focus_error.as_ref().unwrap().len(),
            MAX_FOCUS_ERROR - MAX_FOCUS_ERROR % "창".len()
        );
        assert!(ledger.history[1].focus_error.is_none());
        ledger.validate().unwrap();
        assert!(ledger.record_focus(notice, None));
        assert!(ledger.history[0].focus_error.is_none());
    }

    #[test]
    fn formats_antigravity_notifications() {
        let (host, mut snapshot) = snapshot();
        let mut notice = alert(host, &snapshot, 1000);
        notice.provider = AgentProvider::Antigravity as i32;
        snapshot.attention.push(notice);
        let mut ledger = Ledger::default();
        ledger.observe(host, &snapshot, 1000).unwrap();

        let mut next = alert(host, &snapshot, 2000);
        next.provider = AgentProvider::Antigravity as i32;
        snapshot.attention.push(next);
        let notices = ledger.observe(host, &snapshot, 2000).unwrap().notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "Antigravity has a new response");
    }

    #[test]
    fn imports_existing_alerts_then_notifies_once_across_reconnect_and_restart() {
        let (host, mut snapshot) = snapshot();
        let mut ledger = Ledger::default();
        snapshot.attention.push(alert(host, &snapshot, 1000));
        assert_eq!(
            ledger.observe(host, &snapshot, 1000).unwrap().notices.len(),
            1
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
    fn first_connection_surfaces_old_pending_input_and_recent_responses_but_not_old_popups() {
        let (host, mut snapshot) = snapshot();
        let now = MAX_AGE_MS * 2;
        let mut pending = alert(host, &snapshot, 1);
        pending.reason = AttentionReason::Input as i32;
        pending.request_kind = Some(InputRequestKind::Permission as i32);
        pending.request_status = Some(InputRequestStatus::Pending as i32);
        let mut resolved = pending.clone();
        resolved.attention_id = Uuid::new_v4().to_string();
        resolved.request_status = Some(InputRequestStatus::Resolved as i32);
        snapshot.attention = vec![
            pending,
            resolved,
            alert(host, &snapshot, 1),
            // A small remote clock skew must not silently discard the alert.
            alert(host, &snapshot, now + 1000),
        ];
        let mut ledger = Ledger::default();
        let observed = ledger.observe(host, &snapshot, now).unwrap();
        assert_eq!(observed.notices.len(), 2);
        assert_eq!(ledger.history.len(), 4);
        assert!(
            ledger
                .observe(host, &snapshot, now + 2000)
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

    #[test]
    fn uses_sanitized_thread_title_and_host_label_in_notification_body() {
        let (host, mut attention) = snapshot();
        let mut titled = alert(host, &attention, 1000);
        titled.session_title = Some("  \u{202e}Fix login\n bug  ".into());
        let mut blank = alert(host, &attention, 1001);
        blank.session_title = Some(" \t\u{200e} ".into());
        let mut long_korean = alert(host, &attention, 1002);
        long_korean.session_title = Some("스레드제목".repeat(40));

        attention.attention = vec![long_korean, blank, titled];
        let mut ledger = Ledger::default();
        let observed = ledger
            .observe_with_host(host, Some("SSH · prod"), &attention, 1002)
            .unwrap();
        assert_eq!(observed.notices.len(), 3);
        assert_eq!(observed.notices[0].body, "Fix login bug · SSH · prod");
        assert!(observed.notices[1].body.starts_with("SSH · prod · Agent "));
        assert!(observed.notices[2].body.ends_with("… · SSH · prod"));
        assert!(observed.notices[2].body.len() <= MAX_BODY);
        ledger.validate().unwrap();

        let (local_host, mut local_snapshot) = snapshot();
        let mut local_alert = alert(local_host, &local_snapshot, 1000);
        local_alert.session_title = Some("Refactor auth".into());
        local_snapshot.attention = vec![local_alert];
        let local_notices = ledger
            .observe_with_host(local_host, None, &local_snapshot, 1000)
            .unwrap()
            .notices;
        assert_eq!(local_notices[0].body, "Refactor auth");
    }
}
