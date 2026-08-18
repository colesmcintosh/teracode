use std::{fmt, path::PathBuf, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const TEAM_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    Claude,
    Codex,
    OpenCode,
    Cursor,
    Grok,
    Droid,
}

impl AdapterKind {
    pub const ALL: [Self; 6] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Cursor,
        Self::Grok,
        Self::Droid,
    ];

    pub const fn executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Cursor => "agent",
            Self::Grok => "grok",
            Self::Droid => "droid",
        }
    }
}

impl fmt::Display for AdapterKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
            Self::Grok => "Grok Build",
            Self::Droid => "Factory Droid",
        };
        formatter.write_str(name)
    }
}

impl FromStr for AdapterKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" | "open-code" => Ok(Self::OpenCode),
            "cursor" | "agent" => Ok(Self::Cursor),
            "grok" | "grok-build" => Ok(Self::Grok),
            "droid" | "factory-droid" => Ok(Self::Droid),
            _ => Err(format!("unknown adapter: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingObjective {
    Balanced,
    Quality,
    Speed,
    Cost,
}

impl RoutingObjective {
    pub const ALL: [Self; 4] = [Self::Balanced, Self::Quality, Self::Speed, Self::Cost];
}

impl fmt::Display for RoutingObjective {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Balanced => "balanced",
            Self::Quality => "quality",
            Self::Speed => "speed",
            Self::Cost => "cost",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspacePolicy {
    WorktreePerAgent,
    SharedWorkspace,
    ReadOnlyThenExecutor,
}

impl fmt::Display for WorkspacePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WorktreePerAgent => "worktree per agent",
            Self::SharedWorkspace => "shared workspace",
            Self::ReadOnlyThenExecutor => "read-only then executor",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AutonomyPolicy {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl AutonomyPolicy {
    pub const fn is_dangerous(self) -> bool {
        matches!(self, Self::FullAccess)
    }
}

impl fmt::Display for AutonomyPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace write",
            Self::FullAccess => "full access",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AcceptanceCommand {
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_check_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub required: bool,
}

const fn default_check_timeout() -> u64 {
    300
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderPreference {
    pub adapter: AdapterKind,
    #[serde(default)]
    pub preferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalSpec {
    pub repository: PathBuf,
    pub goal: String,
    pub team_size: usize,
    pub max_parallel: usize,
    pub routing_objective: RoutingObjective,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub provider_preferences: Vec<ProviderPreference>,
    #[serde(default)]
    pub acceptance_checks: Vec<AcceptanceCommand>,
    pub workspace_policy: Option<WorkspacePolicy>,
    pub autonomy_policy: Option<AutonomyPolicy>,
}

impl GoalSpec {
    pub fn new(repository: PathBuf) -> Self {
        Self {
            repository,
            goal: String::new(),
            team_size: 4,
            max_parallel: 4,
            routing_objective: RoutingObjective::Balanced,
            constraints: Vec::new(),
            provider_preferences: Vec::new(),
            acceptance_checks: Vec::new(),
            workspace_policy: None,
            autonomy_policy: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdapterCapabilities {
    pub structured_output: bool,
    pub resume: bool,
    pub model_selection: bool,
    pub read_only: bool,
    pub workspace_write: bool,
    pub full_access: bool,
}

impl AdapterCapabilities {
    pub const fn supports(self, policy: AutonomyPolicy) -> bool {
        match policy {
            AutonomyPolicy::ReadOnly => self.read_only,
            AutonomyPolicy::WorkspaceWrite => self.workspace_write,
            AutonomyPolicy::FullAccess => self.full_access,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentProfile {
    pub id: String,
    pub role: String,
    pub adapter: AdapterKind,
    pub model: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    pub capabilities: AdapterCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Analysis,
    Implementation,
    Review,
    Integration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskNode {
    pub id: String,
    pub kind: TaskKind,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub assigned_agent: String,
    pub objective: String,
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    #[serde(default)]
    pub write_scope: Vec<PathBuf>,
    #[serde(default = "default_retry_limit")]
    pub retry_limit: u8,
}

const fn default_retry_limit() -> u8 {
    2
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillReference {
    pub name: String,
    pub source: PathBuf,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidationStrategy {
    pub reviewer_task: String,
    #[serde(default)]
    pub commands: Vec<AcceptanceCommand>,
    #[serde(default = "default_rework_passes")]
    pub max_rework_passes: u8,
}

const fn default_rework_passes() -> u8 {
    2
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TeamPlan {
    pub schema_version: u32,
    pub name: String,
    pub agents: Vec<AgentProfile>,
    pub tasks: Vec<TaskNode>,
    #[serde(default)]
    pub skills: Vec<SkillReference>,
    pub validation: ValidationStrategy,
    pub final_integration_task: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FactoryBlueprint {
    pub id: Uuid,
    pub name: String,
    pub goal_template: GoalSpec,
    pub plan: TeamPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    Draft,
    Planning,
    AwaitingPolicy,
    Ready,
    Running,
    Validating,
    Reworking,
    Integrating,
    Complete,
    Blocked,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Blocked | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AgentState {
    Queued,
    Starting,
    Working,
    Waiting,
    Reviewing,
    Complete,
    Failed,
    Cancelled,
    Interrupted,
}

impl AgentState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Lifecycle {
        state: AgentState,
        message: Option<String>,
    },
    AssistantText {
        text: String,
    },
    ToolActivity {
        tool: String,
        detail: Option<String>,
    },
    FileActivity {
        path: PathBuf,
        action: String,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    },
    Approval {
        prompt: String,
    },
    Warning {
        message: String,
    },
    Completed {
        session_id: Option<String>,
        summary: Option<String>,
    },
    Failed {
        message: String,
        retryable: bool,
    },
    Diagnostic {
        source: String,
        raw: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub command: AcceptanceCommand,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub duration_ms: u64,
}
