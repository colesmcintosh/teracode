//! Safe, version-aware adapters for supported coding-agent command-line tools.

mod adapter;
mod process;

pub use adapter::{
    AdapterError, AgentAdapter, Invocation, InvocationContext, ProbeReadiness, ProbeResult,
    adapters, parse_normalized_event,
};
pub use process::{ProcessEvent, RunOutput, StreamKind, SupervisorError, run_supervised};
