use corvid_core::agent::{AgentRun, Evidence, RequestKey, RequestKind, RequestStatus};
use serde::{Deserialize, Serialize};

const MAX_RETAINED_INPUTS: usize = 16;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RetainedInput {
    tool: String,
    kind: RequestKind,
}

impl RetainedInput {
    pub(crate) fn collect(run: &AgentRun) -> Vec<Self> {
        let Some(turn) = run.current_open_turn() else {
            return Vec::new();
        };
        run.requests
            .values()
            .filter_map(|request| {
                let key = request.key.as_ref()?;
                (key.turn.as_deref() == Some(turn)
                    && matches!(
                        request.kind,
                        RequestKind::Question | RequestKind::PlanApproval
                    )
                    && matches!(
                        request.state.value,
                        RequestStatus::Pending | RequestStatus::Unverified
                    ))
                .then(|| Self {
                    tool: key.provider_request_id.clone(),
                    kind: request.kind,
                })
            })
            .take(MAX_RETAINED_INPUTS)
            .collect()
    }

    pub(crate) fn valid_all(turn: Option<&str>, inputs: &[Self]) -> bool {
        inputs.len() <= MAX_RETAINED_INPUTS
            && (turn.is_some() || inputs.is_empty())
            && inputs.iter().all(|input| {
                valid_id(&input.tool)
                    && matches!(
                        input.kind,
                        RequestKind::Question | RequestKind::PlanApproval
                    )
            })
    }

    pub(crate) fn restore(self, run: &mut AgentRun, turn: &str, evidence: &Evidence) {
        run.recover_input(
            RequestKey {
                turn: Some(turn.to_owned()),
                provider_request_id: self.tool,
            },
            self.kind,
            evidence,
        );
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
