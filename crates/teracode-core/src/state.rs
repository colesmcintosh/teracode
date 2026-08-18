use thiserror::Error;

use crate::{AgentState, RunState};

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid {machine} transition from {from} to {to}")]
pub struct StateTransitionError {
    machine: &'static str,
    from: String,
    to: String,
}

pub fn transition_run(from: RunState, to: RunState) -> Result<RunState, StateTransitionError> {
    let valid = matches!(
        (from, to),
        (RunState::Draft, RunState::Planning)
            | (
                RunState::Planning,
                RunState::AwaitingPolicy | RunState::Failed
            )
            | (
                RunState::AwaitingPolicy,
                RunState::Ready | RunState::Cancelled
            )
            | (RunState::Ready, RunState::Running | RunState::Cancelled)
            | (
                RunState::Running,
                RunState::Validating
                    | RunState::Blocked
                    | RunState::Failed
                    | RunState::Cancelled
                    | RunState::Interrupted
            )
            | (
                RunState::Validating,
                RunState::Reworking
                    | RunState::Integrating
                    | RunState::Blocked
                    | RunState::Failed
                    | RunState::Cancelled
                    | RunState::Interrupted
            )
            | (
                RunState::Reworking,
                RunState::Validating
                    | RunState::Blocked
                    | RunState::Failed
                    | RunState::Cancelled
                    | RunState::Interrupted
            )
            | (
                RunState::Integrating,
                RunState::Complete | RunState::Blocked | RunState::Failed | RunState::Interrupted
            )
    );
    valid.then_some(to).ok_or_else(|| StateTransitionError {
        machine: "run",
        from: format!("{from:?}"),
        to: format!("{to:?}"),
    })
}

pub fn transition_agent(
    from: AgentState,
    to: AgentState,
) -> Result<AgentState, StateTransitionError> {
    let valid = matches!(
        (from, to),
        (
            AgentState::Queued,
            AgentState::Starting | AgentState::Cancelled
        ) | (
            AgentState::Starting,
            AgentState::Working
                | AgentState::Failed
                | AgentState::Cancelled
                | AgentState::Interrupted
        ) | (
            AgentState::Working,
            AgentState::Waiting
                | AgentState::Reviewing
                | AgentState::Complete
                | AgentState::Failed
                | AgentState::Cancelled
                | AgentState::Interrupted
        ) | (
            AgentState::Waiting,
            AgentState::Working
                | AgentState::Failed
                | AgentState::Cancelled
                | AgentState::Interrupted
        ) | (
            AgentState::Reviewing,
            AgentState::Complete
                | AgentState::Failed
                | AgentState::Cancelled
                | AgentState::Interrupted
        )
    );
    valid.then_some(to).ok_or_else(|| StateTransitionError {
        machine: "agent",
        from: format!("{from:?}"),
        to: format!("{to:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_cannot_transition() {
        assert!(transition_run(RunState::Complete, RunState::Running).is_err());
        assert!(transition_agent(AgentState::Failed, AgentState::Working).is_err());
    }

    #[test]
    fn rework_returns_to_validation() {
        assert_eq!(
            transition_run(RunState::Reworking, RunState::Validating),
            Ok(RunState::Validating)
        );
    }
}
