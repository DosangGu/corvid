use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use serde::Deserialize;

use super::{delivery_gap, valid_id};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    hook_event_name: String,
    conversation_id: Option<String>,
    last_user_input: Option<String>,
    fully_idle: Option<bool>,
    termination_reason: Option<String>,
    error: Option<String>,
}

pub(super) fn fields(input: Option<&[u8]>) -> Option<CollectHook> {
    let gap = || delivery_gap(AgentProvider::Antigravity);
    let Some(input) = input.and_then(|input| serde_json::from_slice::<Input>(input).ok()) else {
        return Some(gap());
    };
    let event = input.event()?;
    // Subagents invoke hooks with a distinct conversationId and an empty lastUserInput.
    if input.last_user_input.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    Some(input.observation(event).unwrap_or_else(gap))
}

impl Input {
    fn event(&self) -> Option<HookEvent> {
        Some(match self.hook_event_name.as_str() {
            "SessionStart" => HookEvent::Startup,
            "PreInvocation" => HookEvent::ModelRequested,
            "PostInvocation" => HookEvent::ModelAttemptFinished,
            "Stop" => {
                if self.fully_idle == Some(false) {
                    return None;
                }
                let reason = self.termination_reason.as_deref();
                if reason.is_some_and(|reason| {
                    reason.eq_ignore_ascii_case("CANCELLED")
                        || reason.eq_ignore_ascii_case("INTERRUPTED")
                        || reason.eq_ignore_ascii_case("USER_CANCELLED")
                }) {
                    HookEvent::TurnInterrupted
                } else if reason.is_some_and(|reason| {
                    reason.eq_ignore_ascii_case("ERROR")
                        || reason.eq_ignore_ascii_case("MAX_STEPS_EXCEEDED")
                }) || self.error.as_deref().is_some_and(|error| !error.is_empty())
                {
                    HookEvent::ResponseFailed
                } else {
                    HookEvent::ResponseObserved
                }
            }
            _ => return None,
        })
    }

    fn observation(self, event: HookEvent) -> Option<CollectHook> {
        Some(CollectHook {
            provider: AgentProvider::Antigravity as i32,
            event: event as i32,
            session_id: Some(valid_id(self.conversation_id?)?),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_lifecycle_and_discards_private_fields() {
        for (event, expected) in [
            ("SessionStart", HookEvent::Startup),
            ("PreInvocation", HookEvent::ModelRequested),
            ("PostInvocation", HookEvent::ModelAttemptFinished),
            ("Stop", HookEvent::ResponseObserved),
        ] {
            let payload = json!({
                "hookEventName": event,
                "conversationId": "4f3ed43a-780b-4ce1-9558-7d63bc5c5313",
                "lastUserInput": "private prompt",
                "transcriptPath": "/tmp/private/transcript.jsonl",
                "fullyIdle": true,
                "terminationReason": "NO_TOOL_CALL",
                "error": "",
            });
            assert_eq!(
                fields(Some(payload.to_string().as_bytes())),
                Some(CollectHook {
                    provider: AgentProvider::Antigravity as i32,
                    event: expected as i32,
                    session_id: Some("4f3ed43a-780b-4ce1-9558-7d63bc5c5313".into()),
                    ..Default::default()
                })
            );
        }
    }

    #[test]
    fn maps_stop_error_interrupt_and_non_idle_states() {
        let base = json!({
            "hookEventName": "Stop",
            "conversationId": "conv-1",
            "lastUserInput": "run",
            "fullyIdle": true,
        });

        let mut error_stop = base.clone();
        error_stop["terminationReason"] = json!("ERROR");
        error_stop["error"] = json!("sensitive backend error");
        assert_eq!(
            fields(Some(error_stop.to_string().as_bytes()))
                .unwrap()
                .event,
            HookEvent::ResponseFailed as i32
        );

        let mut interrupted = base.clone();
        interrupted["terminationReason"] = json!("USER_CANCELLED");
        assert_eq!(
            fields(Some(interrupted.to_string().as_bytes()))
                .unwrap()
                .event,
            HookEvent::TurnInterrupted as i32
        );

        let mut non_idle = base;
        non_idle["fullyIdle"] = json!(false);
        assert!(fields(Some(non_idle.to_string().as_bytes())).is_none());
    }

    #[test]
    fn drops_subagent_and_unsupported_events_without_gap() {
        let subagent = json!({
            "hookEventName": "SessionStart",
            "conversationId": "child-conv",
            "lastUserInput": "",
        });
        assert!(fields(Some(subagent.to_string().as_bytes())).is_none());

        let unknown = json!({
            "hookEventName": "PostToolUse",
            "conversationId": "conv-1",
            "lastUserInput": "hi",
        });
        assert!(fields(Some(unknown.to_string().as_bytes())).is_none());
    }

    #[test]
    fn reports_delivery_gap_on_malformed_or_missing_conversation_id() {
        assert_eq!(
            fields(Some(b"{not-json")).unwrap().event,
            HookEvent::DeliveryGap as i32
        );
        let missing_id = json!({
            "hookEventName": "PreInvocation",
            "lastUserInput": "hi",
        });
        assert_eq!(
            fields(Some(missing_id.to_string().as_bytes()))
                .unwrap()
                .event,
            HookEvent::DeliveryGap as i32
        );
    }
}
