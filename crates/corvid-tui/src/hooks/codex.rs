use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use serde::Deserialize;

use super::{delivery_gap, optional_id, valid_id};

#[derive(Deserialize)]
struct Input {
    hook_event_name: String,
    session_id: String,
    source: Option<String>,
    turn_id: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    agent_id: Option<String>,
    stop_hook_active: Option<bool>,
}

pub(super) fn fields(input: Option<&[u8]>) -> Option<CollectHook> {
    let gap = || delivery_gap(AgentProvider::Codex);
    let Some(input) = input.and_then(|input| serde_json::from_slice::<Input>(input).ok()) else {
        return Some(gap());
    };
    let event = input.event()?;
    Some(input.observation(event).unwrap_or_else(gap))
}

impl Input {
    fn event(&self) -> Option<HookEvent> {
        let tool = self.tool_name.as_deref();
        Some(match self.hook_event_name.as_str() {
            "SessionStart" => match self.source.as_deref()? {
                "startup" => HookEvent::Startup,
                "resume" => HookEvent::Resume,
                "compact" => HookEvent::Compact,
                _ => return None,
            },
            "UserPromptSubmit" => HookEvent::PromptSubmitted,
            "PreModelRequest" => HookEvent::ModelRequested,
            "PostModelRequest" => HookEvent::ModelAttemptFinished,
            "PreToolUse" if tool == Some("Bash") => HookEvent::ToolStarted,
            "PostToolUse" if tool == Some("Bash") => HookEvent::ToolFinished,
            "PreToolUse" if tool == Some("request_user_input") => HookEvent::QuestionStarted,
            "PostToolUse" if tool == Some("request_user_input") => HookEvent::QuestionResult,
            "PermissionRequest" if tool == Some("Bash") => HookEvent::PermissionRequested,
            "Stop" if self.stop_hook_active.is_some() => HookEvent::ResponseObserved,
            "Stop" => HookEvent::DeliveryGap,
            "Interrupt" => HookEvent::TurnInterrupted,
            _ => return None,
        })
    }

    fn observation(self, event: HookEvent) -> Option<CollectHook> {
        if event == HookEvent::DeliveryGap {
            return None;
        }
        let turn_id = if matches!(
            event,
            HookEvent::Startup | HookEvent::Resume | HookEvent::Compact
        ) {
            None
        } else {
            Some(valid_id(self.turn_id?)?)
        };
        let tool_id = match event {
            HookEvent::ToolStarted
            | HookEvent::ToolFinished
            | HookEvent::QuestionStarted
            | HookEvent::QuestionResult => Some(valid_id(self.tool_use_id?)?),
            _ => None,
        };
        Some(CollectHook {
            provider: AgentProvider::Codex as i32,
            event: event as i32,
            session_id: Some(valid_id(self.session_id)?),
            turn_id,
            tool_id,
            actor_id: optional_id(self.agent_id)?,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_request_hooks_retain_only_correlation_fields() {
        for (event, expected) in [
            ("PreModelRequest", HookEvent::ModelRequested),
            ("PostModelRequest", HookEvent::ModelAttemptFinished),
            ("Stop", HookEvent::ResponseObserved),
        ] {
            let payload = serde_json::json!({
                "hook_event_name": event,
                "session_id": "session",
                "turn_id": "turn",
                "agent_id": "child",
                "status": "failed",
                "error": "private",
                "stop_hook_active": true,
            });
            assert_eq!(
                fields(Some(payload.to_string().as_bytes())),
                Some(CollectHook {
                    event: expected as i32,
                    provider: AgentProvider::Codex as i32,
                    session_id: Some("session".into()),
                    turn_id: Some("turn".into()),
                    actor_id: Some("child".into()),
                    ..Default::default()
                })
            );
        }
    }
}
