use corvid_core::agent::Provider;
use corvid_protocol::wire::{
    HookEvent, ProviderAdapter, ProviderCapability, ProviderCapabilityKind as Kind,
    ProviderCorrelation as Field, ProviderExecutionMode as Mode, ProviderQualification as Level,
};

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
    HookEvent::PermissionRequested,
    HookEvent::ResponseObserved,
    HookEvent::ContextEnded,
    HookEvent::ContextReplaced,
    HookEvent::ResumeContextEnded,
    HookEvent::SubagentStarted,
    HookEvent::SubagentResponseObserved,
    HookEvent::SubagentLinked,
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
];

const CAPABILITIES: &[Kind] = &[
    Kind::TurnStart,
    Kind::ResponseBoundary,
    Kind::ExplicitQuestion,
    Kind::PermissionRequest,
    Kind::RequestResolution,
    Kind::SubagentIdentity,
    Kind::RevalidateCurrentState,
    Kind::ContextReplacement,
];

pub(crate) fn adapter(provider: Provider, qualified: bool) -> ProviderAdapter {
    let (mode, events) = match (provider, qualified) {
        (Provider::Claude, true) => (Mode::InteractiveCli, CLAUDE_EVENTS),
        (Provider::Codex, true) => (Mode::InteractiveCli, CODEX_EVENTS),
        (_, false) => (Mode::ProcessOnly, &[][..]),
    };
    let mut correlation = vec![Field::ProcessIncarnation as i32];
    if qualified {
        correlation.extend(
            [Field::DirectChild, Field::Session, Field::Turn, Field::Tool]
                .map(|field| field as i32),
        );
        correlation.push(match provider {
            Provider::Claude => Field::Actor as i32,
            Provider::Codex => Field::ManagedCodexBroker as i32,
        });
    }
    ProviderAdapter {
        mode: mode as i32,
        schema_revision: 1,
        qualified_events: events.iter().map(|event| *event as i32).collect(),
        correlation,
        capabilities: CAPABILITIES
            .iter()
            .map(|&kind| {
                let (level, reason) = if qualified {
                    qualification(provider, kind)
                } else {
                    (
                        Level::Unavailable,
                        "provider release did not pass the hook version check",
                    )
                };
                ProviderCapability {
                    kind: kind as i32,
                    qualification: level as i32,
                    reason: reason.to_owned(),
                }
            })
            .collect(),
    }
}

fn qualification(provider: Provider, kind: Kind) -> (Level, &'static str) {
    match (provider, kind) {
        (_, Kind::TurnStart) => (
            Level::Partial,
            "prompt submission can precede rejection by another hook",
        ),
        (_, Kind::ResponseBoundary) => (
            Level::Partial,
            "Stop gives response evidence before finality is known",
        ),
        (_, Kind::PermissionRequest) => (
            Level::Partial,
            "the permission hook lacks an exact request ID",
        ),
        (Provider::Claude, Kind::SubagentIdentity) => (
            Level::Partial,
            "child work is scoped by session, prompt, and actor; process liveness is unknown",
        ),
        (Provider::Codex, Kind::SubagentIdentity) => {
            (Level::Unavailable, "child ownership is unqualified")
        }
        (_, Kind::RevalidateCurrentState) => (
            Level::Unavailable,
            "the interactive hook feed cannot recover missed events",
        ),
        (Provider::Claude, Kind::ExplicitQuestion) => (
            Level::Partial,
            "qualified AskUserQuestion invocation carries a tool ID before the dialog",
        ),
        (Provider::Claude, Kind::RequestResolution) => (
            Level::Partial,
            "question answers and plan results correlate; manual dismissals and permissions can lack results",
        ),
        (Provider::Claude, Kind::ContextReplacement) => (
            Level::Partial,
            "clear and correlated resume replace the run; reused session activity is ambiguous",
        ),
        (Provider::Codex, Kind::ExplicitQuestion) => (
            Level::Partial,
            "request_user_input can be invoked without displaying a dialog",
        ),
        (Provider::Codex, Kind::RequestResolution) => (
            Level::Partial,
            "Plan mode answers correlate by tool ID and interruptions by turn ID",
        ),
        (Provider::Codex, Kind::ContextReplacement) => (
            Level::Partial,
            "a fresh managed process can resume a session; in-process boundaries are ambiguous",
        ),
        (_, Kind::Unspecified) => unreachable!("unknown provider capability"),
    }
}
