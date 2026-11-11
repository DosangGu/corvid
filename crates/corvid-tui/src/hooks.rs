mod codex;

use std::collections::BTreeSet;
use std::env;
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use clap::ValueEnum;
use corvid_core::agent::Provider;
use corvid_protocol::collector::{HOST_ID, LAUNCH_ASSOCIATION, MAX_BATCH_INPUTS, SERVER_EPOCH};
use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
#[cfg(target_os = "linux")]
use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
#[cfg(target_os = "linux")]
use nix::unistd::read;
use serde::Deserialize;
use serde_json::Value;
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
    claude_input_batch: bool,
    runtime: Option<PathBuf>,
    started: Instant,
) {
    let _ = observe_inner(provider, claude_input_batch, runtime, started);
    if matches!(provider, HookProvider::Codex) && neutral_json {
        let _ = writeln!(io::stdout().lock(), "{{}}");
    }
}

fn observe_inner(
    provider: HookProvider,
    claude_input_batch: bool,
    runtime: Option<PathBuf>,
    started: Instant,
) -> Result<()> {
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
        // Optional batches can exceed the source limit even for unrelated tools.
        HookProvider::Claude if claude_input_batch => input.as_deref().and_then(claude_batch),
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

#[cfg(target_os = "linux")]
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

#[cfg(windows)]
fn read_hook(deadline: Instant) -> Result<Vec<u8>> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType, ReadFile};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    ensure!(
        !input.is_null()
            && input != INVALID_HANDLE_VALUE
            && unsafe { GetFileType(input) } == FILE_TYPE_PIPE,
        "hook stdin is not a pipe"
    );
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        ensure!(Instant::now() < deadline, "hook input timed out");
        let mut available = 0;
        if unsafe { PeekNamedPipe(input, null_mut(), 0, null_mut(), &mut available, null_mut()) }
            == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(bytes);
            }
            return Err(error.into());
        }
        if available == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let mut read = 0;
        let count = available.min(buffer.len() as u32);
        if unsafe { ReadFile(input, buffer.as_mut_ptr(), count, &mut read, null_mut()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        ensure!(
            bytes.len() + read as usize <= SOURCE_LIMIT,
            "hook input exceeds bound"
        );
        bytes.extend_from_slice(&buffer[..read as usize]);
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

#[derive(Deserialize)]
struct ClaudeBatch {
    hook_event_name: String,
    session_id: String,
    prompt_id: String,
    agent_id: Option<String>,
    tool_calls: Vec<ClaudeBatchCall>,
}

#[derive(Deserialize)]
struct ClaudeBatchCall {
    tool_name: String,
    tool_use_id: Option<String>,
}

fn claude_batch(input: &[u8]) -> Option<CollectHook> {
    let batch: ClaudeBatch = serde_json::from_slice(input).ok()?;
    if batch.hook_event_name != "PostToolBatch" || batch.agent_id.is_some() {
        return None;
    }

    let finished_input_ids = batch
        .tool_calls
        .into_iter()
        .filter(|call| matches!(call.tool_name.as_str(), "AskUserQuestion" | "ExitPlanMode"))
        .take(MAX_BATCH_INPUTS + 1)
        .map(|call| valid_id(call.tool_use_id?))
        .collect::<Option<Vec<_>>>()?;
    let unique_ids: BTreeSet<_> = finished_input_ids.iter().collect();
    if !(1..=MAX_BATCH_INPUTS).contains(&finished_input_ids.len())
        || unique_ids.len() != finished_input_ids.len()
    {
        return None;
    }

    Some(CollectHook {
        provider: AgentProvider::Claude as i32,
        event: HookEvent::InputBatchFinished as i32,
        session_id: Some(valid_id(batch.session_id)?),
        turn_id: Some(valid_id(batch.prompt_id)?),
        finished_input_ids,
        ..Default::default()
    })
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
            "PostToolUse" if self.tool_name.as_deref() == Some("Agent") => {
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
        if !matches!(result.status.as_str(), "async_launched" | "completed") {
            return None;
        }
        valid_id(self.tool_use_id.take()?)?;
        let parent = optional_id(self.agent_id.take())?;
        if parent.as_ref() == Some(&result.actor) {
            return None;
        }
        let event = if parent.is_some() {
            HookEvent::NestedSubagentLinked
        } else {
            HookEvent::SubagentLinked
        };
        self.agent_id = Some(result.actor);
        let mut hook = self.subagent_observation(event)?;
        hook.parent_actor_id = parent;
        Some(hook)
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
    let provider = match provider {
        HookProvider::Claude => Provider::Claude,
        HookProvider::Codex => Provider::Codex,
    };
    corvid_daemon::observer_settings(provider)
}

#[cfg(test)]
mod tests {
    use super::{HookEvent, MAX_BATCH_INPUTS, claude_batch, claude_fields};
    use serde_json::json;

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

    #[test]
    fn claude_batches_keep_only_bounded_parent_input_ids() {
        let mut batch = json!({
            "hook_event_name": "PostToolBatch", "session_id": "session", "prompt_id": "turn",
            "tool_calls": [
                {"tool_name": "Read", "tool_use_id": "read", "tool_response": "untrusted"},
                {"tool_name": "ExitPlanMode", "tool_use_id": "plan", "tool_input": {"text": "private"}},
                {"tool_name": "AskUserQuestion", "tool_use_id": "question"}
            ]
        });
        let observation = claude_batch(batch.to_string().as_bytes()).unwrap();
        assert_eq!(observation.event, HookEvent::InputBatchFinished as i32);
        assert_eq!(observation.finished_input_ids, ["plan", "question"]);

        batch["agent_id"] = json!("child");
        assert!(claude_batch(batch.to_string().as_bytes()).is_none());
        batch["agent_id"] = serde_json::Value::Null;
        batch["tool_calls"] = json!(
            (0..=MAX_BATCH_INPUTS)
                .map(|id| json!({"tool_name": "ExitPlanMode", "tool_use_id": id.to_string()}))
                .collect::<Vec<_>>()
        );
        assert!(claude_batch(batch.to_string().as_bytes()).is_none());
    }
}
