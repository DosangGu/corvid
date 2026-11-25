use super::contracts::TurnCorrelation;
use corvid_core::agent::Provider;
use corvid_protocol::wire::{
    ProviderAdapter, ProviderCapability, ProviderCapabilityKind as Kind,
    ProviderCorrelation as Field, ProviderExecutionMode as Mode, ProviderQualification as Level,
};

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
    let contract = super::contract(provider);
    let (mode, schema_revision, events) = if qualified {
        (
            Mode::InteractiveCli,
            contract.unix.schema_revision,
            contract.events,
        )
    } else {
        (Mode::ProcessOnly, 1, &[][..])
    };
    let mut correlation = vec![Field::ProcessIncarnation as i32];
    if qualified {
        correlation.extend([Field::DirectChild, Field::Session].map(|field| field as i32));
        if contract.turn_correlation == TurnCorrelation::ProviderTurnId {
            correlation.extend([Field::Turn, Field::Tool].map(|field| field as i32));
        }
        correlation.extend(contract.unix.extra_correlation.map(|field| field as i32));
    }
    ProviderAdapter {
        mode: mode as i32,
        schema_revision,
        qualified_events: events.iter().map(|event| *event as i32).collect(),
        correlation,
        capabilities: CAPABILITIES
            .iter()
            .map(|&kind| {
                let (level, reason) = if qualified {
                    qualification(contract, kind)
                } else {
                    (
                        Level::Unavailable,
                        "provider executable or version banner was not recognized",
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

fn qualification(contract: &super::contracts::Contract, kind: Kind) -> (Level, &'static str) {
    match kind {
        Kind::TurnStart => contract.unix.turn_start,
        Kind::ResponseBoundary => contract.unix.response_boundary,
        Kind::PermissionRequest => contract.unix.permission_request,
        Kind::RevalidateCurrentState => (
            Level::Unavailable,
            "the interactive hook feed cannot recover missed events",
        ),
        Kind::SubagentIdentity => contract.unix.subagent_identity,
        Kind::ExplicitQuestion => contract.unix.explicit_question,
        Kind::RequestResolution => contract.unix.request_resolution,
        Kind::ContextReplacement => contract.unix.context_replacement,
        Kind::Unspecified => unreachable!("unknown provider capability"),
    }
}
