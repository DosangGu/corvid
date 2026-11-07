use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use serde::Deserialize;
use serde_json::{Map, Value, json};

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
            "SessionStart" if self.source.as_deref() == Some("startup") => HookEvent::Startup,
            "UserPromptSubmit" => HookEvent::PromptSubmitted,
            "PreToolUse" if tool == Some("Bash") => HookEvent::ToolStarted,
            "PostToolUse" if tool == Some("Bash") => HookEvent::ToolFinished,
            "PreToolUse" if tool == Some("request_user_input") => HookEvent::QuestionStarted,
            "PostToolUse" if tool == Some("request_user_input") => HookEvent::QuestionResult,
            "PermissionRequest" if tool == Some("Bash") => HookEvent::PermissionRequested,
            "Stop" if self.stop_hook_active == Some(false) => HookEvent::ResponseObserved,
            "Stop" if self.stop_hook_active.is_none() => HookEvent::DeliveryGap,
            "Interrupt" => HookEvent::TurnInterrupted,
            _ => return None,
        })
    }

    fn observation(self, event: HookEvent) -> Option<CollectHook> {
        if event == HookEvent::DeliveryGap {
            return None;
        }
        let turn_id = if event == HookEvent::Startup {
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

pub(super) fn settings(executable: &str) -> Value {
    let path = executable.replace('\'', "'\"'\"'");
    let command = format!("exec '{path}' observe --provider codex");
    let mut hooks = Map::new();
    for (event, matcher) in [
        ("SessionStart", Some("startup")),
        ("UserPromptSubmit", None),
        ("PreToolUse", Some("Bash|request_user_input")),
        ("PostToolUse", Some("Bash|request_user_input")),
        ("PermissionRequest", Some("Bash")),
        ("Stop", None),
        ("Interrupt", None),
    ] {
        let command = if event == "Stop" {
            format!("{command} --neutral-json")
        } else {
            command.clone()
        };
        let mut group =
            json!({ "hooks": [{ "type": "command", "command": command, "timeout": 1 }] });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        hooks.insert(event.to_owned(), json!([group]));
    }
    json!({ "hooks": hooks })
}
