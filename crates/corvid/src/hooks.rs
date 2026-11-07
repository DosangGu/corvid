mod codex;

use std::env;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use corvid_protocol::collector::{HOST_ID, LAUNCH_ASSOCIATION, SERVER_EPOCH};
use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::read;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

const SOURCE_LIMIT: usize = 64 * 1024;
const SOURCE_TIMEOUT: Duration = Duration::from_millis(30);
const TOTAL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum HookProvider {
    Claude,
    Codex,
}

pub(crate) fn observe(
    provider: HookProvider,
    neutral_json: bool,
    runtime: Option<PathBuf>,
    started: Instant,
) {
    let _ = observe_inner(provider, runtime, started);
    if matches!(provider, HookProvider::Codex) && neutral_json {
        let _ = writeln!(io::stdout().lock(), "{{}}");
    }
}

fn observe_inner(provider: HookProvider, runtime: Option<PathBuf>, started: Instant) -> Result<()> {
    let launch = env::var(LAUNCH_ASSOCIATION)?;
    let host = env::var(HOST_ID)?;
    let epoch = env::var(SERVER_EPOCH)?;
    for id in [&launch, &host, &epoch] {
        ensure!(Uuid::parse_str(id).is_ok(), "invalid managed launch scope");
    }
    let runtime = crate::runtime_path(runtime)?;

    let deadline = started + TOTAL_TIMEOUT;
    let input = read_hook(started + SOURCE_TIMEOUT).ok();
    let fields = match provider {
        HookProvider::Claude => claude_fields(input.as_deref()),
        HookProvider::Codex => codex::fields(input.as_deref()),
    };
    let Some(mut fields) = fields else {
        return Ok(());
    };
    fields.launch_association = launch;
    ensure!(Instant::now() < deadline, "hook deadline elapsed");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(corvid_client::submit_hook(
            &runtime,
            &host,
            &epoch,
            fields,
            deadline.into(),
        ))
}

fn read_hook(deadline: Instant) -> Result<Vec<u8>> {
    let input = io::stdin();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let time_left = deadline.saturating_duration_since(Instant::now());
        ensure!(!time_left.is_zero(), "hook input timed out");
        let mut fds = [PollFd::new(input.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::try_from(time_left)?) {
            Ok(0) => anyhow::bail!("hook input timed out"),
            Err(Errno::EINTR) => continue,
            result => {
                result?;
            }
        }
        let count = match read(&input, &mut buffer) {
            Err(Errno::EINTR | Errno::EAGAIN) => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(bytes);
        }
        ensure!(
            bytes.len() + count <= SOURCE_LIMIT,
            "hook input exceeds bound"
        );
        bytes.extend_from_slice(&buffer[..count]);
    }
}

#[derive(Deserialize)]
struct ClaudeInput {
    hook_event_name: String,
    session_id: Option<String>,
    prompt_id: Option<String>,
    agent_id: Option<String>,
    source: Option<String>,
    reason: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    stop_hook_active: Option<bool>,
}

#[derive(Deserialize)]
struct ClaudeAgentResult {
    #[serde(rename = "agentId")]
    actor: String,
    status: String,
}

fn claude_fields(input: Option<&[u8]>) -> Option<CollectHook> {
    let gap = || delivery_gap(AgentProvider::Claude);
    let Some(payload) = input else {
        return Some(gap());
    };
    let Ok(input) = serde_json::from_slice::<ClaudeInput>(payload) else {
        return Some(gap());
    };
    let event = input.event()?;
    if matches!(
        event,
        HookEvent::SubagentStarted | HookEvent::SubagentResponseObserved
    ) {
        return input.subagent_observation(event);
    }
    if event == HookEvent::SubagentLinked {
        return input.subagent_link(payload);
    }
    if input.agent_id.is_some() {
        return None;
    }
    Some(input.observation(event).unwrap_or_else(gap))
}

fn delivery_gap(provider: AgentProvider) -> CollectHook {
    CollectHook {
        provider: provider as i32,
        event: HookEvent::DeliveryGap as i32,
        ..Default::default()
    }
}

impl ClaudeInput {
    fn event(&self) -> Option<HookEvent> {
        let question = self.tool_name.as_deref() == Some("AskUserQuestion");
        let plan = self.tool_name.as_deref() == Some("ExitPlanMode");
        Some(match self.hook_event_name.as_str() {
            "SessionStart" => match self.source.as_deref() {
                Some("startup") => HookEvent::Startup,
                Some("resume") => HookEvent::Resume,
                Some("clear") => HookEvent::Clear,
                Some("compact") => HookEvent::Compact,
                None => HookEvent::DeliveryGap,
                _ => return None,
            },
            "UserPromptSubmit" => HookEvent::PromptSubmitted,
            "PreToolUse" if question => HookEvent::QuestionStarted,
            "PostToolUse" if question => HookEvent::QuestionResult,
            "PreToolUse" if plan => HookEvent::PlanApprovalStarted,
            "PostToolUse" if plan => HookEvent::PlanApprovalResult,
            "PostToolUse"
                if self.tool_name.as_deref() == Some("Agent") && self.agent_id.is_none() =>
            {
                HookEvent::SubagentLinked
            }
            "PermissionRequest" => HookEvent::PermissionRequested,
            "Stop" => HookEvent::ResponseObserved,
            "SubagentStart" => HookEvent::SubagentStarted,
            "SubagentStop" if self.stop_hook_active == Some(false) => {
                HookEvent::SubagentResponseObserved
            }
            "SessionEnd" => match self.reason.as_deref() {
                Some("clear") => HookEvent::ContextReplaced,
                Some("resume") => HookEvent::ResumeContextEnded,
                _ => HookEvent::ContextEnded,
            },
            _ => return None,
        })
    }

    fn subagent_observation(self, event: HookEvent) -> Option<CollectHook> {
        Some(CollectHook {
            provider: AgentProvider::Claude as i32,
            event: event as i32,
            session_id: Some(valid_id(self.session_id?)?),
            turn_id: Some(valid_id(self.prompt_id?)?),
            actor_id: Some(valid_id(self.agent_id?)?),
            ..Default::default()
        })
    }

    fn subagent_link(mut self, payload: &[u8]) -> Option<CollectHook> {
        #[derive(Deserialize)]
        struct ResultInput {
            tool_response: ClaudeAgentResult,
        }

        let result = serde_json::from_slice::<ResultInput>(payload)
            .ok()?
            .tool_response;
        if result.status != "async_launched" {
            return None;
        }
        valid_id(self.tool_use_id.take()?)?;
        self.agent_id = Some(result.actor);
        self.subagent_observation(HookEvent::SubagentLinked)
    }

    fn observation(self, event: HookEvent) -> Option<CollectHook> {
        if event == HookEvent::DeliveryGap {
            return None;
        }
        let tool_id = if matches!(
            event,
            HookEvent::QuestionStarted
                | HookEvent::QuestionResult
                | HookEvent::PlanApprovalStarted
                | HookEvent::PlanApprovalResult
        ) {
            self.tool_use_id
        } else {
            None
        };
        Some(CollectHook {
            provider: AgentProvider::Claude as i32,
            event: event as i32,
            session_id: Some(valid_id(self.session_id?)?),
            turn_id: optional_id(self.prompt_id)?,
            tool_id: optional_id(tool_id)?,
            ..Default::default()
        })
    }
}

fn valid_id(value: String) -> Option<String> {
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then_some(value)
}

fn optional_id(value: Option<String>) -> Option<Option<String>> {
    match value {
        Some(value) => valid_id(value).map(Some),
        None => Some(None),
    }
}

pub(crate) fn settings(provider: HookProvider) -> Result<Value> {
    let executable = env::current_exe()?.canonicalize()?;
    let executable = executable
        .to_str()
        .context("hook executable path must be UTF-8")?;
    match provider {
        HookProvider::Claude => {
            let handler = json!({
                "type": "command",
                "command": executable,
                "args": ["observe", "--provider", "claude"],
                "timeout": 1,
                "onFailure": "continue",
            });
            let mut hooks = Map::new();
            for event in [
                "SessionStart",
                "UserPromptSubmit",
                "PermissionRequest",
                "Stop",
                "SessionEnd",
                "SubagentStart",
                "SubagentStop",
            ] {
                hooks.insert(event.to_owned(), json!([{ "hooks": [handler.clone()] }]));
            }
            for (event, matcher) in [
                ("PreToolUse", "^(AskUserQuestion|ExitPlanMode)$"),
                ("PostToolUse", "^(AskUserQuestion|ExitPlanMode|Agent)$"),
            ] {
                hooks.insert(
                    event.to_owned(),
                    json!([{
                        "matcher": matcher,
                        "hooks": [handler.clone()],
                    }]),
                );
            }
            Ok(json!({ "hooks": hooks }))
        }
        HookProvider::Codex => Ok(codex::settings(executable)),
    }
}

#[cfg(test)]
mod tests {
    use super::claude_fields;

    #[test]
    fn unsupported_hooks_and_child_identifiers_do_not_report_a_parent_gap() {
        let unknown = br#"{"hook_event_name":"Notification"}"#;
        let child = br#"{
            "hook_event_name":"PostToolUse", "session_id":"session", "agent_id":"child",
            "tool_name":"AskUserQuestion", "tool_use_id":""
        }"#;
        assert!(claude_fields(Some(unknown)).is_none());
        assert!(claude_fields(Some(child)).is_none());
    }
}
