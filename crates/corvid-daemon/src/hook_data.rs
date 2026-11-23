use std::collections::BTreeSet;

use corvid_core::agent::{
    Confidence, EndReason, Evidence, ObservationKind, Provider, RequestKey, RequestKind,
    Resolution, Response, ResponseOutcome, Source, WorkKey,
};
use corvid_protocol::ProtocolFault;
use corvid_protocol::collector::MAX_BATCH_INPUTS;
use corvid_protocol::wire::{CollectHook, HookEvent};
use uuid::Uuid;

use crate::agents::evidence;
use crate::provider;

pub(crate) struct Hook {
    pub(crate) association: Uuid,
    pub(crate) observer_epoch: Option<Uuid>,
    pub(crate) delivery: Uuid,
    pub(crate) provider: Provider,
    pub(crate) event: HookEvent,
    pub(crate) session: Option<String>,
    pub(crate) turn: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) actor: Option<String>,
    pub(crate) parent_actor: Option<String>,
    pub(crate) finished_input_ids: Vec<String>,
}

impl Hook {
    pub(crate) fn decode(
        request: &CollectHook,
        request_id: Option<&str>,
    ) -> Result<Self, ProtocolFault> {
        let invalid = || ProtocolFault::new("InvalidMessage", "invalid hook observation");
        let parse_uuid = |value: &str| Uuid::parse_str(value).ok().filter(|id| !id.is_nil());
        let association = parse_uuid(&request.launch_association).ok_or_else(invalid)?;
        let observer_epoch = request
            .observer_epoch
            .as_deref()
            .map(|value| parse_uuid(value).ok_or_else(invalid))
            .transpose()?;
        let delivery = request_id.and_then(parse_uuid).ok_or_else(invalid)?;
        let event = HookEvent::try_from(request.event).map_err(|_| invalid())?;
        let provider = provider::from_wire(request.provider).ok_or_else(invalid)?;
        let contract = provider::contract(provider);
        let session_expected = event != HookEvent::DeliveryGap;
        if (!contract.events.contains(&event) && event != HookEvent::DeliveryGap)
            || !(contract.valid_hook_fields)(event, request)
            || session_expected != request.session_id.is_some()
            || ![
                &request.session_id,
                &request.turn_id,
                &request.tool_id,
                &request.actor_id,
                &request.parent_actor_id,
            ]
            .into_iter()
            .all(|id| id.as_deref().is_none_or(valid_id))
        {
            return Err(invalid());
        }
        if is_child_event(event)
            && (request.actor_id.is_none()
                || request.turn_id.is_none()
                || request.tool_id.is_some())
        {
            return Err(invalid());
        }
        if (event == HookEvent::NestedSubagentLinked) != request.parent_actor_id.is_some()
            || request.parent_actor_id.is_some() && request.parent_actor_id == request.actor_id
        {
            return Err(invalid());
        }
        if !valid_input_batch(request, event) {
            return Err(invalid());
        }
        Ok(Self {
            association,
            observer_epoch,
            delivery,
            provider,
            event,
            session: request.session_id.clone(),
            turn: request.turn_id.clone(),
            tool: request.tool_id.clone(),
            actor: request.actor_id.clone(),
            parent_actor: request.parent_actor_id.clone(),
            finished_input_ids: request.finished_input_ids.clone(),
        })
    }

    fn request_key(&self) -> Option<RequestKey> {
        Some(RequestKey {
            turn: Some(self.turn.clone()?),
            provider_request_id: self.tool.clone()?,
        })
    }

    fn work_key(&self) -> Option<WorkKey> {
        Some(WorkKey {
            turn: Some(self.turn.clone()?),
            item: self.tool.clone()?,
        })
    }

    pub(crate) fn evidence(&self, confidence: Confidence) -> Evidence {
        let mut evidence = hook_evidence(self.provider, confidence);
        if confidence == Confidence::Inferred {
            evidence.uncertainty = Some(
                match self.event {
                    HookEvent::ToolStarted => "other hooks can prevent the tool from running",
                    HookEvent::QuestionStarted => {
                        "tool invocation can precede or skip the question dialog"
                    }
                    _ => "other hooks can block a submitted prompt",
                }
                .to_owned(),
            );
        }
        evidence
    }

    pub(crate) fn observation(&self) -> Option<(ObservationKind, Confidence)> {
        let mut confidence = Confidence::Confirmed;
        let kind = match self.event {
            HookEvent::PromptSubmitted => {
                let turn = self.turn.clone()?;
                return Some((provider::contract(self.provider).prompt)(turn));
            }
            HookEvent::QuestionStarted => {
                confidence = provider::contract(self.provider).question_confidence;
                ObservationKind::InputRequested {
                    key: self.request_key(),
                    kind: RequestKind::Question,
                }
            }
            HookEvent::PlanApprovalStarted => ObservationKind::InputRequested {
                key: self.request_key(),
                kind: RequestKind::PlanApproval,
            },
            HookEvent::QuestionResult | HookEvent::PlanApprovalResult => {
                ObservationKind::InputResolved {
                    key: self.request_key()?,
                    resolution: Resolution::Resolved,
                }
            }
            HookEvent::InputBatchFinished => ObservationKind::InputBatchFinished {
                turn: self.turn.clone()?,
                tools: self.finished_input_ids.clone(),
            },
            HookEvent::PermissionRequested => ObservationKind::InputRequested {
                key: None,
                kind: RequestKind::Permission,
            },
            HookEvent::ToolStarted => {
                confidence = Confidence::Inferred;
                ObservationKind::WorkStarted(self.work_key()?)
            }
            HookEvent::ToolFinished => ObservationKind::ToolExecuted(self.work_key()?),
            HookEvent::ModelRequested => ObservationKind::ModelRequested(self.turn.clone()?),
            HookEvent::ModelAttemptFinished => {
                ObservationKind::ModelAttemptFinished(self.turn.clone()?)
            }
            HookEvent::TurnInterrupted => ObservationKind::Response {
                response: Response {
                    turn: Some(self.turn.clone()?),
                    outcome: ResponseOutcome::Interrupted,
                },
                final_boundary: true,
            },
            HookEvent::ResponseObserved => ObservationKind::Response {
                response: Response {
                    turn: self.turn.clone(),
                    outcome: ResponseOutcome::Observed,
                },
                final_boundary: false,
            },
            HookEvent::ContextEnded => ObservationKind::ContextEnded(EndReason::ContextEnded),
            HookEvent::ContextReplaced => ObservationKind::ContextEnded(EndReason::ContextReplaced),
            _ => return None,
        };
        Some((kind, confidence))
    }
}

fn valid_input_batch(request: &CollectHook, event: HookEvent) -> bool {
    let tools = &request.finished_input_ids;
    if event != HookEvent::InputBatchFinished {
        return tools.is_empty();
    }

    let mut seen = BTreeSet::new();
    request.turn_id.is_some()
        && request.tool_id.is_none()
        && request.actor_id.is_none()
        && (1..=MAX_BATCH_INPUTS).contains(&tools.len())
        && tools.iter().all(|tool| valid_id(tool) && seen.insert(tool))
}

fn is_child_event(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::SubagentStarted
            | HookEvent::SubagentResponseObserved
            | HookEvent::SubagentLinked
            | HookEvent::NestedSubagentLinked
    )
}

pub(crate) fn hook_evidence(provider: Provider, confidence: Confidence) -> Evidence {
    let mut evidence = evidence(Source::ProviderHook, confidence);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Identify the parser contract, not an unmeasured installed CLI release.
        evidence.source_version = crate::provider::contract(provider).evidence_version();
    }
    #[cfg(windows)]
    {
        let _ = provider;
        evidence.source_version = "native-hook.v2".to_owned();
    }
    evidence
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
