use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use schemars::schema_for;
use thiserror::Error;

use crate::{
    AdapterCapabilities, AdapterKind, AgentProfile, GoalSpec, SkillMetadata, SkillReference,
    TEAM_PLAN_SCHEMA_VERSION, TaskKind, TaskNode, TeamPlan, ValidationStrategy,
};

const MAX_TEAM_SIZE: usize = 32;

#[derive(Debug, Clone)]
pub struct AdapterCandidate {
    pub kind: AdapterKind,
    pub installed: bool,
    pub ready: bool,
    pub capabilities: AdapterCapabilities,
    pub quality_tier: Option<u8>,
    pub speed_tier: Option<u8>,
    pub cost_tier: Option<u8>,
    pub historical_duration_ms: Option<u64>,
}

impl AdapterCandidate {
    fn score(&self, objective: crate::RoutingObjective, preferred: bool) -> i32 {
        let quality = i32::from(self.quality_tier.unwrap_or(0));
        let speed = i32::from(self.speed_tier.unwrap_or(0));
        // A higher configured cost tier means a less expensive provider.
        let cost = i32::from(self.cost_tier.unwrap_or(0));
        let history_bonus = self
            .historical_duration_ms
            .map_or(0, |duration| i32::from(duration < 120_000));
        let base = match objective {
            crate::RoutingObjective::Balanced => quality + speed + cost,
            crate::RoutingObjective::Quality => quality * 3 + speed + cost,
            crate::RoutingObjective::Speed => speed * 3 + quality + history_bonus,
            crate::RoutingObjective::Cost => cost * 3 + quality,
        };
        base + i32::from(preferred) * 10
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TeamPlanValidationError {
    #[error("team size must be between 1 and {MAX_TEAM_SIZE}")]
    InvalidTeamSize,
    #[error("max parallelism must be between 1 and the team size")]
    InvalidParallelism,
    #[error("workspace and autonomy policies must both be selected")]
    MissingPolicy,
    #[error("plan schema version {0} is unsupported")]
    SchemaVersion(u32),
    #[error("plan contains {actual} agents, expected {expected}")]
    AgentCount { expected: usize, actual: usize },
    #[error("duplicate or empty {kind} identifier: {id}")]
    InvalidIdentifier { kind: &'static str, id: String },
    #[error("task {task} references unknown agent {agent}")]
    UnknownAgent { task: String, agent: String },
    #[error("task {task} references unknown dependency {dependency}")]
    UnknownDependency { task: String, dependency: String },
    #[error("task graph contains a cycle")]
    Cycle,
    #[error("final integration task is missing or is not an integration task")]
    InvalidIntegrationTask,
    #[error("validation reviewer task does not exist")]
    InvalidReviewerTask,
    #[error("plan references unavailable skill {0}")]
    UnknownSkill(String),
    #[error("acceptance command executable cannot be empty")]
    EmptyCommand,
}

#[derive(Debug, Error)]
pub enum RecommendationError {
    #[error("no installed and policy-compatible adapter is available")]
    NoEligibleAdapter,
    #[error("planner failed and no deterministic fallback could be built: {0}")]
    Planner(String),
}

#[async_trait]
pub trait PlanGenerator: Send + Sync {
    async fn generate(
        &self,
        adapter: AdapterKind,
        prompt: &str,
        repair: bool,
    ) -> Result<String, String>;
}

#[derive(Debug, Default)]
pub struct RecommendationEngine;

impl RecommendationEngine {
    pub fn eligible<'a>(
        &self,
        goal: &GoalSpec,
        candidates: &'a [AdapterCandidate],
    ) -> Vec<&'a AdapterCandidate> {
        let policy = goal.autonomy_policy;
        let preferred: HashSet<_> = goal
            .provider_preferences
            .iter()
            .filter(|preference| preference.preferred)
            .map(|preference| preference.adapter)
            .collect();
        let mut eligible: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                candidate.installed
                    && candidate.ready
                    && candidate.capabilities.structured_output
                    && policy.is_none_or(|policy| candidate.capabilities.clone().supports(policy))
            })
            .collect();
        eligible.sort_by_key(|candidate| {
            std::cmp::Reverse(
                candidate.score(goal.routing_objective, preferred.contains(&candidate.kind)),
            )
        });
        eligible
    }

    pub async fn recommend<G: PlanGenerator>(
        &self,
        goal: &GoalSpec,
        candidates: &[AdapterCandidate],
        skills: &[SkillMetadata],
        generator: &G,
    ) -> Result<TeamPlan, RecommendationError> {
        validate_goal_configuration(goal, false)
            .map_err(|error| RecommendationError::Planner(error.to_string()))?;
        let eligible = self.eligible(goal, candidates);
        let Some(planner) = eligible.first() else {
            return Err(RecommendationError::NoEligibleAdapter);
        };
        let prompt = planning_prompt(goal, skills);
        let available_skills: HashSet<_> = skills.iter().map(|skill| skill.name.as_str()).collect();

        let first = generator.generate(planner.kind, &prompt, false).await;
        if let Ok(raw) = first
            && let Ok(plan) = serde_json::from_str::<TeamPlan>(&raw)
            && validate_plan_inner(goal, &plan, &available_skills, false).is_ok()
        {
            return Ok(plan);
        }

        let repair_prompt = format!(
            "{prompt}\nThe prior response was malformed or invalid. Return only one corrected JSON object."
        );
        let repaired = generator.generate(planner.kind, &repair_prompt, true).await;
        if let Ok(raw) = repaired
            && let Ok(plan) = serde_json::from_str::<TeamPlan>(&raw)
            && validate_plan_inner(goal, &plan, &available_skills, false).is_ok()
        {
            return Ok(plan);
        }

        self.fallback(goal, &eligible, skills)
    }

    pub fn fallback(
        &self,
        goal: &GoalSpec,
        eligible: &[&AdapterCandidate],
        skills: &[SkillMetadata],
    ) -> Result<TeamPlan, RecommendationError> {
        let Some(first) = eligible.first() else {
            return Err(RecommendationError::NoEligibleAdapter);
        };
        let read_only_candidate = eligible
            .iter()
            .find(|candidate| candidate.capabilities.read_only)
            .copied()
            .ok_or(RecommendationError::NoEligibleAdapter)?;

        let roles = role_names(goal.team_size);
        let factory_goal = goal_context(goal);
        let agents = roles
            .iter()
            .enumerate()
            .map(|(index, role)| {
                let candidate = if index == 0 || index == goal.team_size.saturating_sub(1) {
                    read_only_candidate
                } else {
                    eligible
                        .get(index % eligible.len())
                        .copied()
                        .unwrap_or(first)
                };
                AgentProfile {
                    id: format!("cell-{}", index + 1),
                    role: (*role).to_owned(),
                    adapter: candidate.kind,
                    model: None,
                    prompt: role_prompt(role, &factory_goal),
                    skills: relevant_skills(&goal.goal, skills),
                    capabilities: candidate.capabilities.clone(),
                }
            })
            .collect::<Vec<_>>();

        let mut tasks = vec![TaskNode {
            id: "survey".into(),
            kind: TaskKind::Analysis,
            dependencies: Vec::new(),
            assigned_agent: agents[0].id.clone(),
            objective: format!(
                "Inspect the repository and produce an implementation brief for: {factory_goal}"
            ),
            expected_artifacts: vec!["survey.md".into()],
            write_scope: Vec::new(),
            retry_limit: 1,
        }];

        let builder_indices: Vec<usize> = if goal.team_size <= 2 {
            vec![0]
        } else {
            (1..goal.team_size.saturating_sub(1)).collect()
        };
        let mut build_ids = Vec::new();
        for (position, agent_index) in builder_indices.into_iter().enumerate() {
            let id = format!("build-{}", position + 1);
            build_ids.push(id.clone());
            tasks.push(TaskNode {
                id,
                kind: TaskKind::Implementation,
                dependencies: vec!["survey".into()],
                assigned_agent: agents[agent_index].id.clone(),
                objective: format!(
                    "Implement a cohesive portion of the goal and report changed artifacts: {factory_goal}"
                ),
                expected_artifacts: vec!["implementation changes".into()],
                write_scope: vec![goal.repository.clone()],
                retry_limit: 2,
            });
        }

        let reviewer_index = goal.team_size.saturating_sub(1);
        tasks.push(TaskNode {
            id: "quality-gate".into(),
            kind: TaskKind::Review,
            dependencies: build_ids,
            assigned_agent: agents[reviewer_index].id.clone(),
            objective:
                "Review the combined changes for correctness, safety, scope, and maintainability"
                    .into(),
            expected_artifacts: vec!["review.md".into()],
            write_scope: Vec::new(),
            retry_limit: 2,
        });
        tasks.push(TaskNode {
            id: "integration".into(),
            kind: TaskKind::Integration,
            dependencies: vec!["quality-gate".into()],
            assigned_agent: agents[0].id.clone(),
            objective: "Resolve integration issues and leave a staged, acceptance-ready change"
                .into(),
            expected_artifacts: vec!["staged integration worktree".into()],
            write_scope: vec![goal.repository.clone()],
            retry_limit: 2,
        });

        let plan = TeamPlan {
            schema_version: TEAM_PLAN_SCHEMA_VERSION,
            name: "Software factory line".into(),
            agents,
            tasks,
            skills: skills
                .iter()
                .filter(|skill| relevant_skills(&goal.goal, std::slice::from_ref(skill)).len() == 1)
                .map(|skill| SkillReference {
                    name: skill.name.clone(),
                    source: skill.path.clone(),
                    description: skill.description.clone(),
                })
                .collect(),
            validation: ValidationStrategy {
                reviewer_task: "quality-gate".into(),
                commands: goal.acceptance_checks.clone(),
                max_rework_passes: 2,
            },
            final_integration_task: "integration".into(),
        };
        let available_skills: HashSet<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
        validate_plan_inner(goal, &plan, &available_skills, false)
            .map_err(|error| RecommendationError::Planner(error.to_string()))?;
        Ok(plan)
    }
}

pub fn validate_goal(goal: &GoalSpec) -> Result<(), TeamPlanValidationError> {
    validate_goal_configuration(goal, true)
}

fn validate_goal_configuration(
    goal: &GoalSpec,
    require_policies: bool,
) -> Result<(), TeamPlanValidationError> {
    if !(1..=MAX_TEAM_SIZE).contains(&goal.team_size) {
        return Err(TeamPlanValidationError::InvalidTeamSize);
    }
    if goal.max_parallel == 0 || goal.max_parallel > goal.team_size {
        return Err(TeamPlanValidationError::InvalidParallelism);
    }
    if require_policies && (goal.workspace_policy.is_none() || goal.autonomy_policy.is_none()) {
        return Err(TeamPlanValidationError::MissingPolicy);
    }
    if goal
        .acceptance_checks
        .iter()
        .any(|command| command.executable.trim().is_empty())
    {
        return Err(TeamPlanValidationError::EmptyCommand);
    }
    Ok(())
}

pub fn validate_plan(
    goal: &GoalSpec,
    plan: &TeamPlan,
    available_skills: &HashSet<&str>,
) -> Result<(), TeamPlanValidationError> {
    validate_plan_inner(goal, plan, available_skills, true)
}

fn validate_plan_inner(
    goal: &GoalSpec,
    plan: &TeamPlan,
    available_skills: &HashSet<&str>,
    require_policies: bool,
) -> Result<(), TeamPlanValidationError> {
    validate_goal_configuration(goal, require_policies)?;
    if plan.schema_version != TEAM_PLAN_SCHEMA_VERSION {
        return Err(TeamPlanValidationError::SchemaVersion(plan.schema_version));
    }
    if plan.agents.len() != goal.team_size {
        return Err(TeamPlanValidationError::AgentCount {
            expected: goal.team_size,
            actual: plan.agents.len(),
        });
    }

    let agent_ids = unique_ids(plan.agents.iter().map(|agent| agent.id.as_str()), "agent")?;
    let task_ids = unique_ids(plan.tasks.iter().map(|task| task.id.as_str()), "task")?;
    for task in &plan.tasks {
        if !agent_ids.contains(task.assigned_agent.as_str()) {
            return Err(TeamPlanValidationError::UnknownAgent {
                task: task.id.clone(),
                agent: task.assigned_agent.clone(),
            });
        }
        for dependency in &task.dependencies {
            if !task_ids.contains(dependency.as_str()) {
                return Err(TeamPlanValidationError::UnknownDependency {
                    task: task.id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
    }
    ensure_acyclic(&plan.tasks)?;

    let integration = plan
        .tasks
        .iter()
        .find(|task| task.id == plan.final_integration_task);
    if !matches!(
        integration.map(|task| task.kind),
        Some(TaskKind::Integration)
    ) {
        return Err(TeamPlanValidationError::InvalidIntegrationTask);
    }
    if !task_ids.contains(plan.validation.reviewer_task.as_str()) {
        return Err(TeamPlanValidationError::InvalidReviewerTask);
    }
    for skill in &plan.skills {
        if !available_skills.contains(skill.name.as_str()) {
            return Err(TeamPlanValidationError::UnknownSkill(skill.name.clone()));
        }
    }
    if plan
        .validation
        .commands
        .iter()
        .any(|command| command.executable.trim().is_empty())
    {
        return Err(TeamPlanValidationError::EmptyCommand);
    }
    Ok(())
}

fn unique_ids<'a>(
    ids: impl Iterator<Item = &'a str>,
    kind: &'static str,
) -> Result<HashSet<&'a str>, TeamPlanValidationError> {
    let mut unique = HashSet::new();
    for id in ids {
        if id.trim().is_empty() || !unique.insert(id) {
            return Err(TeamPlanValidationError::InvalidIdentifier {
                kind,
                id: id.to_owned(),
            });
        }
    }
    Ok(unique)
}

fn ensure_acyclic(tasks: &[TaskNode]) -> Result<(), TeamPlanValidationError> {
    let mut inbound: HashMap<&str, usize> = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.dependencies.len()))
        .collect();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        for dependency in &task.dependencies {
            dependents
                .entry(dependency)
                .or_default()
                .push(task.id.as_str());
        }
    }
    let mut ready: VecDeque<_> = inbound
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect();
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for dependent in dependents.get(id).into_iter().flatten() {
            let count = inbound.get_mut(dependent).expect("task was indexed");
            *count -= 1;
            if *count == 0 {
                ready.push_back(dependent);
            }
        }
    }
    if visited == tasks.len() {
        Ok(())
    } else {
        Err(TeamPlanValidationError::Cycle)
    }
}

fn planning_prompt(goal: &GoalSpec, skills: &[SkillMetadata]) -> String {
    let schema = serde_json::to_string_pretty(&schema_for!(TeamPlan))
        .expect("the static TeamPlan schema is serializable");
    let skill_names = skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let goal_contract =
        serde_json::to_string_pretty(goal).expect("the GoalSpec contract is serializable");
    format!(
        "Design a provider-neutral software factory from this GoalSpec:\n{goal_contract}\nAvailable skills: [{skill_names}]. Return only JSON matching this schema:\n{schema}"
    )
}

fn goal_context(goal: &GoalSpec) -> String {
    if goal.constraints.is_empty() {
        return goal.goal.clone();
    }
    format!(
        "{}\nOperating constraints:\n- {}",
        goal.goal,
        goal.constraints.join("\n- ")
    )
}

fn role_names(team_size: usize) -> Vec<&'static str> {
    const ROLES: [&str; 8] = [
        "Line architect",
        "Implementation engineer",
        "Test engineer",
        "Quality inspector",
        "Security reviewer",
        "Documentation engineer",
        "Integration engineer",
        "Performance reviewer",
    ];
    (0..team_size)
        .map(|index| ROLES[index % ROLES.len()])
        .collect()
}

fn role_prompt(role: &str, goal: &str) -> String {
    format!(
        "You are the {role} in a TeraCode production line. Work only on your assigned task, preserve repository conventions, communicate through requested artifacts, and verify your work. Factory goal: {goal}"
    )
}

fn relevant_skills(goal: &str, skills: &[SkillMetadata]) -> Vec<String> {
    let goal = goal.to_ascii_lowercase();
    skills
        .iter()
        .filter(|skill| {
            skill
                .name
                .split(['-', '_', ' '])
                .filter(|token| token.len() >= 4)
                .any(|token| goal.contains(&token.to_ascii_lowercase()))
                || skill.description.as_ref().is_some_and(|description| {
                    description
                        .split_whitespace()
                        .filter(|token| token.len() >= 5)
                        .any(|token| goal.contains(&token.to_ascii_lowercase()))
                })
        })
        .map(|skill| skill.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::{AcceptanceCommand, AutonomyPolicy, RoutingObjective, WorkspacePolicy};

    fn goal() -> GoalSpec {
        GoalSpec {
            repository: PathBuf::from("/repo"),
            goal: "Build tested API".into(),
            team_size: 4,
            max_parallel: 2,
            routing_objective: RoutingObjective::Balanced,
            constraints: Vec::new(),
            provider_preferences: Vec::new(),
            acceptance_checks: vec![AcceptanceCommand {
                executable: "cargo".into(),
                args: vec!["test".into()],
                timeout_secs: 300,
                required: true,
            }],
            workspace_policy: Some(WorkspacePolicy::WorktreePerAgent),
            autonomy_policy: Some(AutonomyPolicy::WorkspaceWrite),
        }
    }

    fn candidate() -> AdapterCandidate {
        AdapterCandidate {
            kind: AdapterKind::Codex,
            installed: true,
            ready: true,
            capabilities: AdapterCapabilities {
                structured_output: true,
                resume: true,
                model_selection: true,
                read_only: true,
                workspace_write: true,
                full_access: true,
            },
            quality_tier: Some(3),
            speed_tier: Some(2),
            cost_tier: None,
            historical_duration_ms: None,
        }
    }

    struct RecordedGenerator {
        responses: Mutex<VecDeque<String>>,
        calls: Arc<Mutex<Vec<bool>>>,
    }

    #[async_trait]
    impl PlanGenerator for RecordedGenerator {
        async fn generate(
            &self,
            _adapter: AdapterKind,
            _prompt: &str,
            repair: bool,
        ) -> Result<String, String> {
            self.calls.lock().unwrap().push(repair);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "missing response".into())
        }
    }

    #[test]
    fn fallback_is_valid_and_deterministic() {
        let goal = goal();
        let candidate = candidate();
        let engine = RecommendationEngine;
        let first = engine.fallback(&goal, &[&candidate], &[]).unwrap();
        let second = engine.fallback(&goal, &[&candidate], &[]).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.agents.len(), 4);
        assert_eq!(first.final_integration_task, "integration");
    }

    #[test]
    fn cycle_is_rejected() {
        let goal = goal();
        let candidate = candidate();
        let engine = RecommendationEngine;
        let mut plan = engine.fallback(&goal, &[&candidate], &[]).unwrap();
        plan.tasks[0].dependencies = vec!["integration".into()];
        assert_eq!(
            validate_plan(&goal, &plan, &HashSet::new()),
            Err(TeamPlanValidationError::Cycle)
        );
    }

    #[test]
    fn concurrency_cannot_exceed_worker_identities() {
        let mut goal = goal();
        goal.max_parallel = 5;
        assert_eq!(
            validate_goal(&goal),
            Err(TeamPlanValidationError::InvalidParallelism)
        );
    }

    #[test]
    fn quality_routing_prefers_configured_quality() {
        let mut goal = goal();
        goal.routing_objective = RoutingObjective::Quality;
        let low = candidate();
        let mut high = candidate();
        high.kind = AdapterKind::Claude;
        high.quality_tier = Some(5);
        let candidates = vec![low, high];
        let ranked = RecommendationEngine.eligible(&goal, &candidates);
        assert_eq!(ranked[0].kind, AdapterKind::Claude);
    }

    #[tokio::test]
    async fn malformed_planner_output_gets_one_repair_attempt() {
        let goal = goal();
        let candidate = candidate();
        let expected = RecommendationEngine
            .fallback(&goal, &[&candidate], &[])
            .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let generator = RecordedGenerator {
            responses: Mutex::new(VecDeque::from([
                "not-json".into(),
                serde_json::to_string(&expected).unwrap(),
            ])),
            calls: Arc::clone(&calls),
        };

        let plan = RecommendationEngine
            .recommend(&goal, &[candidate], &[], &generator)
            .await
            .unwrap();

        assert_eq!(plan, expected);
        assert_eq!(*calls.lock().unwrap(), [false, true]);
    }
}
