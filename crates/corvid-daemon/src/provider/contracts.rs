use std::ffi::OsString;

use corvid_core::agent::{Confidence, ObservationKind, Provider};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::collector::{CLAUDE_VERSION, CODEX_VERSION};
use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::wire::{ProviderCorrelation as Field, ProviderQualification as Level};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use semver::Version;

pub(crate) struct Resume {
    pub(crate) arguments: fn(&str) -> Vec<OsString>,
    pub(crate) confirmation: Option<ResumeConfirmation>,
}

#[derive(Clone, Copy)]
pub(crate) enum ResumeConfirmation {
    AuthenticatedSession,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy)]
pub(crate) enum InProcessResume {
    CorrelatedEnd,
    Ambiguous(&'static str),
}

/// Provider semantics for launch, observation and recovery.
pub(crate) struct Contract {
    pub(crate) provider: Provider,
    pub(crate) wire_id: i32,
    pub(crate) events: &'static [HookEvent],
    pub(crate) valid_hook_fields: fn(HookEvent, &CollectHook) -> bool,
    pub(crate) prompt: fn(String) -> (ObservationKind, Confidence),
    pub(crate) question_confidence: Confidence,
    pub(crate) valid_session_id: fn(&str) -> bool,
    pub(crate) resume: Option<Resume>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) unix: UnixIntegration,
    #[cfg(windows)]
    pub(crate) windows: WindowsIntegration,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct UnixIntegration {
    pub(crate) program: &'static str,
    pub(crate) fixture_version: &'static str,
    pub(crate) schema_revision: u32,
    version_prefix: &'static str,
    version_suffix: &'static str,
    pub(crate) noninteractive_arguments: &'static [&'static str],
    pub(crate) managed_broker: bool,
    pub(crate) launch_prefix: &'static [&'static str],
    pub(crate) quarantine_unknown_session: bool,
    pub(crate) in_process_resume: InProcessResume,
    pub(crate) extra_correlation: Option<Field>,
    pub(crate) subagent_identity: (Level, &'static str),
    pub(crate) explicit_question: (Level, &'static str),
    pub(crate) request_resolution: (Level, &'static str),
    pub(crate) context_replacement: (Level, &'static str),
    pub(crate) hooks: super::hooks::Config,
}

#[cfg(windows)]
pub(crate) struct WindowsIntegration {
    pub(crate) program: &'static str,
    pub(crate) allows_hook_shell: bool,
    pub(crate) hooks: super::windows_hooks::Config,
}

const CLAUDE_EVENTS: &[HookEvent] = &[
    HookEvent::Startup,
    HookEvent::Resume,
    HookEvent::Clear,
    HookEvent::Compact,
    HookEvent::PromptSubmitted,
    HookEvent::QuestionStarted,
    HookEvent::QuestionResult,
    HookEvent::PlanApprovalStarted,
    HookEvent::PlanApprovalResult,
    HookEvent::InputBatchFinished,
    HookEvent::PermissionRequested,
    HookEvent::ResponseObserved,
    HookEvent::ContextEnded,
    HookEvent::ContextReplaced,
    HookEvent::ResumeContextEnded,
    HookEvent::SubagentStarted,
    HookEvent::SubagentResponseObserved,
    HookEvent::SubagentLinked,
    HookEvent::NestedSubagentLinked,
    HookEvent::ToolStarted,
    HookEvent::ToolFinished,
];

const CODEX_EVENTS: &[HookEvent] = &[
    HookEvent::Startup,
    HookEvent::Resume,
    HookEvent::Compact,
    HookEvent::PromptSubmitted,
    HookEvent::QuestionStarted,
    HookEvent::QuestionResult,
    HookEvent::PermissionRequested,
    HookEvent::ResponseObserved,
    HookEvent::ToolStarted,
    HookEvent::ToolFinished,
    HookEvent::TurnInterrupted,
    HookEvent::ModelRequested,
    HookEvent::ModelAttemptFinished,
];

pub(crate) const CONTRACTS: &[Contract] = &[CLAUDE, CODEX];

const CLAUDE: Contract = Contract {
    provider: Provider::Claude,
    wire_id: AgentProvider::Claude as i32,
    events: CLAUDE_EVENTS,
    valid_hook_fields: |event, request| {
        !matches!(event, HookEvent::ToolStarted | HookEvent::ToolFinished)
            || (request.turn_id.is_some()
                && request.tool_id.is_some()
                && request.actor_id.is_none())
    },
    prompt: |turn| {
        (
            ObservationKind::InteractivePromptSubmitted(turn),
            Confidence::Inferred,
        )
    },
    question_confidence: Confidence::Confirmed,
    valid_session_id: legacy_session_id,
    resume: Some(Resume {
        arguments: |id| vec!["--resume".into(), id.into()],
        confirmation: Some(ResumeConfirmation::AuthenticatedSession),
    }),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unix: UnixIntegration {
        program: "claude",
        fixture_version: CLAUDE_VERSION,
        schema_revision: 4,
        version_prefix: "",
        version_suffix: " (Claude Code)",
        noninteractive_arguments: &["-p", "--print"],
        managed_broker: false,
        launch_prefix: &[],
        quarantine_unknown_session: false,
        in_process_resume: InProcessResume::CorrelatedEnd,
        extra_correlation: Some(Field::Actor),
        subagent_identity: (
            Level::Partial,
            "child work requires a linked parent actor chain; process liveness is unknown",
        ),
        explicit_question: (
            Level::Partial,
            "qualified AskUserQuestion invocation carries a tool ID before the dialog",
        ),
        request_resolution: (
            Level::Partial,
            "input results correlate; a finished batch without a result leaves the outcome unverified",
        ),
        context_replacement: (
            Level::Partial,
            "clear and correlated resume replace the run; reused session activity is ambiguous",
        ),
        hooks: super::hooks::CLAUDE,
    },
    #[cfg(windows)]
    windows: WindowsIntegration {
        program: "claude.exe",
        allows_hook_shell: false,
        hooks: super::windows_hooks::CLAUDE,
    },
};

const CODEX: Contract = Contract {
    provider: Provider::Codex,
    wire_id: AgentProvider::Codex as i32,
    events: CODEX_EVENTS,
    valid_hook_fields: codex_hook_fields,
    prompt: |turn| (ObservationKind::TurnStarted(turn), Confidence::Inferred),
    question_confidence: Confidence::Inferred,
    valid_session_id: legacy_session_id,
    resume: Some(Resume {
        arguments: |id| vec!["resume".into(), id.into()],
        confirmation: Some(ResumeConfirmation::AuthenticatedSession),
    }),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unix: UnixIntegration {
        program: "codex",
        fixture_version: CODEX_VERSION,
        schema_revision: 2,
        version_prefix: "codex-cli ",
        version_suffix: "",
        noninteractive_arguments: &["e", "exec"],
        managed_broker: true,
        // A shared Codex app server can outlive the managed launcher.
        launch_prefix: &["-c", "features.daemon_auto_start=false"],
        quarantine_unknown_session: true,
        in_process_resume: InProcessResume::Ambiguous(
            "in-process Codex resume lacks a qualified boundary",
        ),
        extra_correlation: Some(Field::ManagedCodexBroker),
        subagent_identity: (Level::Unavailable, "child ownership is unqualified"),
        explicit_question: (
            Level::Partial,
            "request_user_input can be invoked without displaying a dialog",
        ),
        request_resolution: (
            Level::Partial,
            "Plan mode answers correlate by tool ID and interruptions by turn ID",
        ),
        context_replacement: (
            Level::Partial,
            "a fresh managed process can resume a session; in-process boundaries are ambiguous",
        ),
        hooks: super::hooks::CODEX,
    },
    #[cfg(windows)]
    windows: WindowsIntegration {
        program: "codex.exe",
        allows_hook_shell: true,
        hooks: super::windows_hooks::CODEX,
    },
};

#[cfg(test)]
const FIXTURE: Contract = Contract {
    provider: Provider::Fixture,
    wire_id: 99,
    events: &[
        HookEvent::Startup,
        HookEvent::Resume,
        HookEvent::Clear,
        HookEvent::PromptSubmitted,
        HookEvent::QuestionStarted,
        HookEvent::QuestionResult,
        HookEvent::PlanApprovalStarted,
        HookEvent::ResponseObserved,
    ],
    prompt: |turn| (ObservationKind::TurnStarted(turn), Confidence::Confirmed),
    valid_session_id: |id| id.strip_prefix("session/").is_some_and(legacy_session_id),
    resume: Some(Resume {
        arguments: |id| vec!["reopen".into(), id.into()],
        confirmation: Some(ResumeConfirmation::AuthenticatedSession),
    }),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unix: UnixIntegration {
        program: "fixture",
        ..CLAUDE.unix
    },
    #[cfg(windows)]
    windows: WindowsIntegration {
        program: "fixture.exe",
        ..CLAUDE.windows
    },
    ..CLAUDE
};

#[cfg(test)]
const FIXTURE_MANUAL: Contract = Contract {
    provider: Provider::FixtureManual,
    wire_id: 100,
    resume: None,
    ..FIXTURE
};

#[cfg(test)]
const FIXTURE_UNVERIFIED: Contract = Contract {
    provider: Provider::FixtureUnverified,
    wire_id: 101,
    resume: Some(Resume {
        arguments: |id| vec!["reopen".into(), id.into()],
        confirmation: None,
    }),
    ..FIXTURE
};

fn integrations() -> impl Iterator<Item = &'static Contract> {
    #[cfg(test)]
    let fixtures = [&FIXTURE, &FIXTURE_MANUAL, &FIXTURE_UNVERIFIED];
    #[cfg(not(test))]
    let fixtures = [];
    CONTRACTS.iter().chain(fixtures)
}

pub(super) fn registered(provider: Provider) -> Option<&'static Contract> {
    integrations().find(|contract| contract.provider == provider)
}

pub(crate) fn contract(provider: Provider) -> &'static Contract {
    registered(provider).expect("registered provider contract")
}

pub(crate) fn from_wire(provider: i32) -> Option<Provider> {
    integrations()
        .find(|contract| contract.wire_id == provider)
        .map(|contract| contract.provider)
}

fn legacy_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn codex_hook_fields(event: HookEvent, request: &CollectHook) -> bool {
    let turn_expected = !matches!(
        event,
        HookEvent::Startup | HookEvent::Resume | HookEvent::Compact | HookEvent::DeliveryGap
    );
    let tool_expected = matches!(
        event,
        HookEvent::ToolStarted
            | HookEvent::ToolFinished
            | HookEvent::QuestionStarted
            | HookEvent::QuestionResult
    );
    turn_expected == request.turn_id.is_some() && tool_expected == request.tool_id.is_some()
}

impl Contract {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn interactive_invocation(&self, argv: &[OsString]) -> bool {
        let Some(first) = argv.first().and_then(|arg| arg.to_str()) else {
            return true;
        };
        !matches!(first, "-h" | "--help" | "-V" | "--version")
            && !self.unix.noninteractive_arguments.contains(&first)
    }
}

impl Contract {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn version(&self, output: &str) -> Option<Version> {
        // A fixture release is evidence about the adapter, not an admission range.
        // Every provider still validates actual hook fields and process ownership.
        output
            .trim()
            .strip_prefix(self.unix.version_prefix)
            .and_then(|version| version.strip_suffix(self.unix.version_suffix))
            .and_then(|version| Version::parse(version).ok())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn evidence_version(&self) -> String {
        format!("{}-hooks.v{}", self.unix.program, self.unix.schema_revision)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn accepts_older_newer_and_prerelease_providers_without_a_version_range() {
        for contract in CONTRACTS {
            for version in [
                "0.1.0",
                "1.0.0",
                "2.1.300",
                "2.1.325",
                "0.166.0",
                "0.168.0-alpha.1",
                "3.0.0",
                contract.unix.fixture_version,
            ] {
                let banner = format!(
                    "{}{version}{}\n",
                    contract.unix.version_prefix, contract.unix.version_suffix
                );
                assert!(contract.version(&banner).is_some(), "{banner}");
            }
        }
    }

    #[test]
    fn rejects_malformed_and_other_provider_version_banners() {
        for contract in CONTRACTS {
            for version in ["2.1", "02.1.325", "garbage", "2.1.325\nunexpected", ""] {
                let banner = format!(
                    "{}{version}{}",
                    contract.unix.version_prefix, contract.unix.version_suffix
                );
                assert!(!contract.version(&banner).is_some(), "{banner}");
            }
            for other in CONTRACTS {
                if other.provider == contract.provider {
                    continue;
                }
                let banner = format!(
                    "{}{}{}",
                    other.unix.version_prefix,
                    other.unix.fixture_version,
                    other.unix.version_suffix
                );
                assert!(!contract.version(&banner).is_some(), "{banner}");
            }
        }
    }
}
