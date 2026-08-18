//! Provider-neutral contracts and orchestration primitives for TeraCode.

pub mod config;
pub mod model;
pub mod persistence;
pub mod planner;
pub mod scheduler;
pub mod skills;
pub mod state;
pub mod validation;
pub mod workspace;

pub use config::{AdapterTuning, ConfigError, TeraCodeConfig};
pub use model::*;
pub use persistence::{HistoryStore, RetentionPolicy};
pub use planner::{AdapterCandidate, RecommendationEngine, TeamPlanValidationError, validate_plan};
pub use scheduler::{ScheduleError, TaskExecution, TaskRunner, execute_plan};
pub use skills::{SkillIndex, SkillMetadata};
pub use state::{StateTransitionError, transition_agent, transition_run};
pub use validation::{ValidationError, run_acceptance_checks};
pub use workspace::{RepositoryStatus, WorkspaceError, WorkspaceManager, WorkspaceSet};
