use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    fmt::Write as _,
    io,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use teracode_adapters::{ProbeReadiness, ProbeResult};
use teracode_core::{
    AcceptanceCommand, AdapterCandidate, AdapterKind, AgentEvent, AgentState, AutonomyPolicy,
    FactoryBlueprint, GoalSpec, HistoryStore, ProviderPreference, RecommendationEngine,
    RepositoryStatus, RoutingObjective, RunState, SkillIndex, TeamPlan, TeraCodeConfig,
    WorkspacePolicy, validate_plan,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::runner::{FactoryOutcome, FactoryUpdate, execute_factory};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Doctor,
    Switchboard,
    Proposal,
    Policy,
    Run,
    Result,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProposalSection {
    Agents,
    Tasks,
    Checks,
}

impl ProposalSection {
    fn next(self) -> Self {
        match self {
            Self::Agents => Self::Tasks,
            Self::Tasks => Self::Checks,
            Self::Checks => Self::Agents,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    basalt: Color,
    rail: Color,
    paper: Color,
    signal: Color,
    active: Color,
    success: Color,
    fault: Color,
}

impl Palette {
    fn detect() -> Self {
        let truecolor = std::env::var("COLORTERM")
            .is_ok_and(|value| value.contains("truecolor") || value.contains("24bit"));
        if truecolor {
            Self {
                basalt: Color::Rgb(0x16, 0x1a, 0x20),
                rail: Color::Rgb(0x30, 0x39, 0x46),
                paper: Color::Rgb(0xd7, 0xde, 0xe8),
                signal: Color::Rgb(0xe5, 0xa8, 0x4b),
                active: Color::Rgb(0x67, 0xc7, 0xd4),
                success: Color::Rgb(0x6f, 0xb9, 0x8f),
                fault: Color::Rgb(0xe0, 0x6c, 0x75),
            }
        } else {
            Self {
                basalt: Color::Indexed(234),
                rail: Color::Indexed(238),
                paper: Color::Indexed(253),
                signal: Color::Indexed(215),
                active: Color::Indexed(80),
                success: Color::Indexed(78),
                fault: Color::Indexed(168),
            }
        }
    }
}

pub struct App {
    stage: Stage,
    goal: GoalSpec,
    repository: RepositoryStatus,
    probes: Vec<ProbeResult>,
    skills: SkillIndex,
    plan: Option<TeamPlan>,
    palette: Palette,
    ascii: bool,
    switch_focus: usize,
    selected_provider: usize,
    proposal_section: ProposalSection,
    selected_agent: usize,
    selected_task: usize,
    selected_check: usize,
    selected_skill: usize,
    selected_history: usize,
    editing: bool,
    editing_model: bool,
    editing_prompt: bool,
    editing_check_args: bool,
    check_args_buffer: String,
    dangerous_confirmed: bool,
    message: String,
    task_states: HashMap<String, AgentState>,
    logs: VecDeque<String>,
    outcome: Option<FactoryOutcome>,
    run_id: Option<Uuid>,
    run_started: Option<Instant>,
    run_elapsed: Option<Duration>,
    cancellation: Option<CancellationToken>,
    launch_area: Rect,
    history: HistoryStore,
    config: TeraCodeConfig,
    history_rows: Vec<serde_json::Value>,
}

impl App {
    pub fn new(
        repository: RepositoryStatus,
        probes: Vec<ProbeResult>,
        skills: SkillIndex,
        history: HistoryStore,
        config: TeraCodeConfig,
        ascii: bool,
    ) -> Self {
        let mut goal = GoalSpec::new(repository.path.clone());
        goal.acceptance_checks = discover_acceptance_checks(&repository.path);
        Self {
            stage: Stage::Doctor,
            goal,
            repository,
            probes,
            skills,
            plan: None,
            palette: Palette::detect(),
            ascii,
            switch_focus: 0,
            selected_provider: 0,
            proposal_section: ProposalSection::Agents,
            selected_agent: 0,
            selected_task: 0,
            selected_check: 0,
            selected_skill: 0,
            selected_history: 0,
            editing: false,
            editing_model: false,
            editing_prompt: false,
            editing_check_args: false,
            check_args_buffer: String::new(),
            dangerous_confirmed: false,
            message: "Inspect the line, then press Enter to configure a factory.".into(),
            task_states: HashMap::new(),
            logs: VecDeque::new(),
            outcome: None,
            run_id: None,
            run_started: None,
            run_elapsed: None,
            cancellation: None,
            launch_area: Rect::default(),
            history,
            config,
            history_rows: Vec::new(),
        }
    }

    fn installed_adapters(&self) -> Vec<AdapterKind> {
        self.probes
            .iter()
            .filter(|probe| probe.installed && probe.capabilities.structured_output)
            .map(|probe| probe.adapter)
            .collect()
    }

    fn propose(&mut self) {
        if self.goal.goal.trim().is_empty() {
            self.message = "Goal is required before a line can be designed.".into();
            return;
        }
        let mut candidates = self
            .probes
            .iter()
            .map(|probe| {
                let tuning = self.config.tuning(probe.adapter);
                AdapterCandidate {
                    kind: probe.adapter,
                    installed: probe.installed,
                    ready: probe.readiness != ProbeReadiness::Unavailable,
                    capabilities: probe.capabilities.clone(),
                    quality_tier: tuning.and_then(|tuning| tuning.quality_tier),
                    speed_tier: tuning.and_then(|tuning| tuning.speed_tier),
                    cost_tier: tuning.and_then(|tuning| tuning.cost_tier),
                    historical_duration_ms: None,
                }
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| {
            self.config
                .provider_priority
                .iter()
                .position(|adapter| *adapter == candidate.kind)
                .unwrap_or(usize::MAX)
        });
        let engine = RecommendationEngine;
        let eligible = engine.eligible(&self.goal, &candidates);
        match engine.fallback(&self.goal, &eligible, &self.skills.all()) {
            Ok(plan) => {
                self.plan = Some(plan);
                self.stage = Stage::Proposal;
                self.message =
                    "Blueprint ready. Edit cells, routing, skills, and quality gates.".into();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn cycle_workspace(&mut self) {
        self.goal.workspace_policy = Some(match self.goal.workspace_policy {
            None if self.repository.is_git => WorkspacePolicy::WorktreePerAgent,
            None | Some(WorkspacePolicy::WorktreePerAgent) => WorkspacePolicy::SharedWorkspace,
            Some(WorkspacePolicy::SharedWorkspace) if self.repository.is_git => {
                WorkspacePolicy::ReadOnlyThenExecutor
            }
            Some(WorkspacePolicy::SharedWorkspace | WorkspacePolicy::ReadOnlyThenExecutor) => {
                if self.repository.is_git {
                    WorkspacePolicy::WorktreePerAgent
                } else {
                    WorkspacePolicy::SharedWorkspace
                }
            }
        });
    }

    fn cycle_autonomy(&mut self) {
        self.goal.autonomy_policy = Some(match self.goal.autonomy_policy {
            None | Some(AutonomyPolicy::FullAccess) => AutonomyPolicy::ReadOnly,
            Some(AutonomyPolicy::ReadOnly) => AutonomyPolicy::WorkspaceWrite,
            Some(AutonomyPolicy::WorkspaceWrite) => AutonomyPolicy::FullAccess,
        });
        self.dangerous_confirmed = false;
    }

    fn policy_error(&self) -> Option<String> {
        let workspace = self.goal.workspace_policy?;
        let autonomy = self.goal.autonomy_policy?;
        if autonomy.is_dangerous() && !self.dangerous_confirmed {
            return Some("Full access needs the separate ! confirmation.".into());
        }
        let plan = self.plan.as_ref()?;
        for task in &plan.tasks {
            let profile = plan
                .agents
                .iter()
                .find(|agent| agent.id == task.assigned_agent)?;
            let required = if matches!(
                task.kind,
                teracode_core::TaskKind::Analysis | teracode_core::TaskKind::Review
            ) || (workspace == WorkspacePolicy::ReadOnlyThenExecutor
                && task.kind != teracode_core::TaskKind::Integration)
            {
                AutonomyPolicy::ReadOnly
            } else {
                autonomy
            };
            if !profile.capabilities.clone().supports(required) {
                return Some(format!(
                    "{} cannot run task {} under {}. Edit its provider assignment.",
                    profile.adapter, task.id, required
                ));
            }
        }
        let skills = self.skills.all();
        let available = skills.iter().map(|skill| skill.name.as_str()).collect();
        validate_plan(&self.goal, plan, &available)
            .err()
            .map(|error| error.to_string())
    }

    fn begin_run(&mut self) -> Option<(Uuid, GoalSpec, TeamPlan, bool, CancellationToken)> {
        if self.goal.workspace_policy.is_none() || self.goal.autonomy_policy.is_none() {
            self.message = "Choose both workspace isolation and autonomy before launch.".into();
            return None;
        }
        if let Some(error) = self.policy_error() {
            self.message = error;
            return None;
        }
        let mut plan = self.plan.clone()?;
        for agent in &mut plan.agents {
            let bundle = match self.skills.load_prompt_bundle(&agent.skills) {
                Ok(bundle) => bundle,
                Err(error) => {
                    self.message = format!("Cannot load selected skill instructions: {error}");
                    return None;
                }
            };
            if !bundle.is_empty() {
                let _ = write!(agent.prompt, "\n\nRun-scoped skill instructions:\n{bundle}");
            }
        }
        let run_id = Uuid::new_v4();
        if let Err(error) = self.history.create_run(run_id, &self.goal, Some(&plan)) {
            self.message = format!("Cannot persist run history: {error}");
            return None;
        }
        for probe in &self.probes {
            let _ = self
                .history
                .record_probe(Some(run_id), &probe.adapter.to_string(), probe);
        }
        for state in [
            RunState::Planning,
            RunState::AwaitingPolicy,
            RunState::Ready,
            RunState::Running,
        ] {
            if let Err(error) = self.history.set_state(run_id, state, None) {
                self.message = format!("Cannot persist run state: {error}");
                return None;
            }
        }
        self.run_id = Some(run_id);
        self.task_states = plan
            .tasks
            .iter()
            .map(|task| (task.id.clone(), AgentState::Queued))
            .collect();
        self.logs.clear();
        self.outcome = None;
        self.run_started = Some(Instant::now());
        self.run_elapsed = None;
        self.stage = Stage::Run;
        self.message = "FABRICATION / production cells are starting".into();
        let cancellation = CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        Some((
            run_id,
            self.goal.clone(),
            plan,
            self.dangerous_confirmed,
            cancellation,
        ))
    }

    fn handle_update(&mut self, update: FactoryUpdate) {
        let run_id = self.run_id;
        match update {
            FactoryUpdate::Status(message) => self.message = message,
            FactoryUpdate::TaskState {
                task_id,
                agent_id,
                state,
            } => {
                self.task_states.insert(task_id.clone(), state);
                let event = AgentEvent::Lifecycle {
                    state,
                    message: None,
                };
                if let Some(run_id) = run_id {
                    let _ =
                        self.history
                            .record_event(run_id, Some(&task_id), Some(&agent_id), &event);
                }
                self.push_log(format!("[{}] {task_id} / {agent_id}", state_label(state)));
            }
            FactoryUpdate::Event {
                task_id,
                agent_id,
                event,
            } => {
                if let Some(run_id) = run_id {
                    let _ =
                        self.history
                            .record_event(run_id, Some(&task_id), Some(&agent_id), &event);
                }
                self.push_log(format_event(&task_id, &event));
            }
            FactoryUpdate::Check(check) => {
                if let Some(run_id) = run_id {
                    let _ = self.history.record_check(run_id, &check);
                }
                self.push_log(format!(
                    "[{}] {} {:?}",
                    if check.passed { "PASS" } else { "FAIL" },
                    check.command.executable,
                    check.command.args
                ));
            }
            FactoryUpdate::Workspace {
                integration_path,
                warning,
            } => {
                if let Some(run_id) = run_id {
                    let _ = self.history.set_integration_path(run_id, &integration_path);
                    let metadata = serde_json::json!({
                        "workspace_policy": self.goal.workspace_policy,
                        "warning": warning.as_deref(),
                    });
                    let _ = self.history.record_artifact(
                        run_id,
                        None,
                        "integration-worktree",
                        &integration_path,
                        Some(&metadata),
                    );
                }
                if let Some(warning) = warning {
                    self.push_log(format!("[WARN] {warning}"));
                }
                self.push_log(format!("[LINE] {}", integration_path.display()));
            }
            FactoryUpdate::Finished(outcome) => {
                self.run_elapsed = self.run_started.map(|started| started.elapsed());
                if let Some(run_id) = run_id {
                    let _ =
                        self.history
                            .set_state(run_id, outcome.state, outcome.reason.as_deref());
                }
                self.message = match outcome.state {
                    RunState::Complete => "COMPLETE / quality gate passed".into(),
                    _ => format!(
                        "{} / line stopped",
                        format!("{:?}", outcome.state).to_uppercase()
                    ),
                };
                self.outcome = Some(outcome);
                self.cancellation = None;
                self.stage = Stage::Result;
            }
        }
    }

    fn push_log(&mut self, line: String) {
        self.logs.push_back(line);
        while self.logs.len() > 200 {
            self.logs.pop_front();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.stage == Stage::Run {
                if let Some(cancellation) = &self.cancellation {
                    cancellation.cancel();
                    self.message = "CANCELLING / stopping all process groups".into();
                }
                return AppAction::None;
            }
            return AppAction::Quit;
        }
        if self.editing {
            return self.handle_edit_key(key);
        }
        match self.stage {
            Stage::Doctor => match key.code {
                KeyCode::Enter => {
                    self.stage = Stage::Switchboard;
                    self.message = "Define what this factory must produce.".into();
                    AppAction::None
                }
                KeyCode::Char('q') => AppAction::Quit,
                _ => AppAction::None,
            },
            Stage::Switchboard => self.handle_switchboard_key(key),
            Stage::Proposal => self.handle_proposal_key(key),
            Stage::Policy => self.handle_policy_key(key),
            Stage::Run => match key.code {
                KeyCode::Char('q') => {
                    if let Some(cancellation) = &self.cancellation {
                        cancellation.cancel();
                        self.message = "CANCELLING / stopping all process groups".into();
                    }
                    AppAction::None
                }
                _ => AppAction::None,
            },
            Stage::Result => self.handle_result_key(key),
            Stage::History => match key.code {
                KeyCode::Up => {
                    self.selected_history = self.selected_history.saturating_sub(1);
                    AppAction::None
                }
                KeyCode::Down => {
                    self.selected_history =
                        (self.selected_history + 1).min(self.history_rows.len().saturating_sub(1));
                    AppAction::None
                }
                KeyCode::Enter | KeyCode::Char('r') => {
                    self.retry_selected_history();
                    AppAction::None
                }
                KeyCode::Char('e') => {
                    self.export_selected_history();
                    AppAction::None
                }
                KeyCode::Char('d') => {
                    self.delete_selected_history();
                    AppAction::None
                }
                KeyCode::Char('b') | KeyCode::Esc => {
                    self.stage = Stage::Result;
                    AppAction::None
                }
                KeyCode::Char('q') => AppAction::Quit,
                _ => AppAction::None,
            },
        }
    }

    fn handle_switchboard_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Tab => self.switch_focus = (self.switch_focus + 1) % 6,
            KeyCode::BackTab => self.switch_focus = (self.switch_focus + 5) % 6,
            KeyCode::Left | KeyCode::Char('-') if self.switch_focus == 1 => {
                self.goal.team_size = self.goal.team_size.saturating_sub(1).max(1);
                self.goal.max_parallel = self.goal.max_parallel.min(self.goal.team_size);
            }
            KeyCode::Right | KeyCode::Char('+') if self.switch_focus == 1 => {
                self.goal.team_size = (self.goal.team_size + 1).min(32);
            }
            KeyCode::Left | KeyCode::Char('-') if self.switch_focus == 2 => {
                self.goal.max_parallel = self.goal.max_parallel.saturating_sub(1).max(1);
            }
            KeyCode::Right | KeyCode::Char('+') if self.switch_focus == 2 => {
                self.goal.max_parallel = (self.goal.max_parallel + 1).min(self.goal.team_size);
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.switch_focus == 3 => {
                let position = RoutingObjective::ALL
                    .iter()
                    .position(|objective| *objective == self.goal.routing_objective)
                    .unwrap_or(0);
                let delta = usize::from(matches!(key.code, KeyCode::Right | KeyCode::Char(' ')));
                self.goal.routing_objective = if delta == 1 {
                    RoutingObjective::ALL[(position + 1) % RoutingObjective::ALL.len()]
                } else {
                    RoutingObjective::ALL
                        [(position + RoutingObjective::ALL.len() - 1) % RoutingObjective::ALL.len()]
                };
            }
            KeyCode::Left | KeyCode::Right if self.switch_focus == 4 => {
                let count = AdapterKind::ALL.len();
                if key.code == KeyCode::Right {
                    self.selected_provider = (self.selected_provider + 1) % count;
                } else {
                    self.selected_provider = (self.selected_provider + count - 1) % count;
                }
            }
            KeyCode::Char(' ') if self.switch_focus == 4 => {
                let adapter = AdapterKind::ALL[self.selected_provider];
                if let Some(preference) = self
                    .goal
                    .provider_preferences
                    .iter_mut()
                    .find(|preference| preference.adapter == adapter)
                {
                    preference.preferred = !preference.preferred;
                } else {
                    self.goal.provider_preferences.push(ProviderPreference {
                        adapter,
                        preferred: true,
                    });
                }
            }
            KeyCode::Backspace if self.switch_focus == 0 => {
                self.goal.goal.pop();
            }
            KeyCode::Backspace if self.switch_focus == 5 => {
                if let Some(constraint) = self.goal.constraints.first_mut() {
                    constraint.pop();
                }
            }
            KeyCode::Char(character) if self.switch_focus == 0 => self.goal.goal.push(character),
            KeyCode::Char(character) if self.switch_focus == 5 => {
                if self.goal.constraints.is_empty() {
                    self.goal.constraints.push(String::new());
                }
                self.goal.constraints[0].push(character);
            }
            KeyCode::Enter => self.propose(),
            KeyCode::Esc => self.stage = Stage::Doctor,
            _ => {}
        }
        AppAction::None
    }

    fn handle_proposal_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Tab => self.proposal_section = self.proposal_section.next(),
            KeyCode::Up => self.move_proposal_selection(-1),
            KeyCode::Down => self.move_proposal_selection(1),
            KeyCode::Char('a') if self.proposal_section == ProposalSection::Agents => {
                let installed = self.installed_adapters();
                if let Some(agent) = self
                    .plan
                    .as_mut()
                    .and_then(|plan| plan.agents.get_mut(self.selected_agent))
                    && !installed.is_empty()
                {
                    let current = installed
                        .iter()
                        .position(|adapter| *adapter == agent.adapter)
                        .unwrap_or(0);
                    agent.adapter = installed[(current + 1) % installed.len()];
                    if let Some(probe) = self
                        .probes
                        .iter()
                        .find(|probe| probe.adapter == agent.adapter)
                    {
                        agent.capabilities = probe.capabilities.clone();
                    }
                }
            }
            KeyCode::Char('m') if self.proposal_section == ProposalSection::Agents => {
                self.editing_model = true;
                self.editing_prompt = false;
                self.editing = true;
            }
            KeyCode::Char('p') if self.proposal_section == ProposalSection::Agents => {
                self.editing_model = false;
                self.editing_prompt = true;
                self.editing = true;
            }
            KeyCode::Char('[') if self.proposal_section == ProposalSection::Agents => {
                self.selected_skill = self.selected_skill.saturating_sub(1);
            }
            KeyCode::Char(']') if self.proposal_section == ProposalSection::Agents => {
                self.selected_skill = (self.selected_skill + 1).min(
                    self.plan
                        .as_ref()
                        .map_or(0, |plan| plan.skills.len().saturating_sub(1)),
                );
            }
            KeyCode::Char('e') => {
                self.editing_model = false;
                self.editing_prompt = false;
                self.editing = true;
            }
            KeyCode::Char('d') if self.proposal_section == ProposalSection::Tasks => {
                if self.selected_task > 0
                    && let Some(plan) = &mut self.plan
                {
                    let dependency = plan.tasks[self.selected_task - 1].id.clone();
                    let task = &mut plan.tasks[self.selected_task];
                    if task.dependencies.contains(&dependency) {
                        task.dependencies.retain(|value| value != &dependency);
                    } else {
                        task.dependencies.push(dependency);
                    }
                }
            }
            KeyCode::Char('x') if self.proposal_section == ProposalSection::Tasks => {
                if let Some(plan) = &mut self.plan
                    && !plan.agents.is_empty()
                {
                    let task = &mut plan.tasks[self.selected_task];
                    let current = plan
                        .agents
                        .iter()
                        .position(|agent| agent.id == task.assigned_agent)
                        .unwrap_or(0);
                    task.assigned_agent
                        .clone_from(&plan.agents[(current + 1) % plan.agents.len()].id);
                }
            }
            KeyCode::Char('b') => {
                if let Some(plan) = &self.plan {
                    let blueprint = FactoryBlueprint {
                        id: Uuid::new_v4(),
                        name: plan.name.clone(),
                        goal_template: self.goal.clone(),
                        plan: plan.clone(),
                    };
                    self.message = match self.history.save_blueprint(&blueprint) {
                        Ok(()) => format!(
                            "Saved reusable factory blueprint {} ({})",
                            blueprint.name, blueprint.id
                        ),
                        Err(error) => format!("Blueprint save failed: {error}"),
                    };
                }
            }
            KeyCode::Char('s') if self.proposal_section == ProposalSection::Agents => {
                if let Some(plan) = &mut self.plan
                    && let Some(skill) = plan.skills.get(self.selected_skill)
                    && let Some(agent) = plan.agents.get_mut(self.selected_agent)
                {
                    if agent.skills.contains(&skill.name) {
                        agent.skills.retain(|name| name != &skill.name);
                    } else {
                        agent.skills.push(skill.name.clone());
                    }
                }
            }
            KeyCode::Char('g') if self.proposal_section == ProposalSection::Checks => {
                if let Some(command) = self
                    .plan
                    .as_ref()
                    .and_then(|plan| plan.validation.commands.get(self.selected_check))
                {
                    self.check_args_buffer =
                        serde_json::to_string(&command.args).unwrap_or_else(|_| "[]".into());
                    self.editing_check_args = true;
                    self.editing = true;
                }
            }
            KeyCode::Char('r') if self.proposal_section == ProposalSection::Checks => {
                if let Some(command) = self
                    .plan
                    .as_mut()
                    .and_then(|plan| plan.validation.commands.get_mut(self.selected_check))
                {
                    command.required = !command.required;
                }
            }
            KeyCode::Char('[') if self.proposal_section == ProposalSection::Checks => {
                if let Some(command) = self
                    .plan
                    .as_mut()
                    .and_then(|plan| plan.validation.commands.get_mut(self.selected_check))
                {
                    command.timeout_secs = command.timeout_secs.saturating_sub(30).max(1);
                }
            }
            KeyCode::Char(']') if self.proposal_section == ProposalSection::Checks => {
                if let Some(command) = self
                    .plan
                    .as_mut()
                    .and_then(|plan| plan.validation.commands.get_mut(self.selected_check))
                {
                    command.timeout_secs = command.timeout_secs.saturating_add(30);
                }
            }
            KeyCode::Char('+') if self.proposal_section == ProposalSection::Checks => {
                if let Some(plan) = &mut self.plan {
                    plan.validation.commands.push(AcceptanceCommand {
                        executable: String::new(),
                        args: Vec::new(),
                        timeout_secs: 300,
                        required: true,
                    });
                    self.selected_check = plan.validation.commands.len().saturating_sub(1);
                    self.editing = true;
                }
            }
            KeyCode::Char('-') if self.proposal_section == ProposalSection::Checks => {
                if let Some(plan) = &mut self.plan
                    && !plan.validation.commands.is_empty()
                {
                    plan.validation.commands.remove(self.selected_check);
                    self.selected_check = self
                        .selected_check
                        .min(plan.validation.commands.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                self.stage = Stage::Policy;
                self.message = "Select both controls. There are no executable defaults.".into();
            }
            KeyCode::Esc => self.stage = Stage::Switchboard,
            _ => {}
        }
        AppAction::None
    }

    fn move_proposal_selection(&mut self, delta: isize) {
        let (selected, len) = match self.proposal_section {
            ProposalSection::Agents => (
                &mut self.selected_agent,
                self.plan.as_ref().map_or(0, |plan| plan.agents.len()),
            ),
            ProposalSection::Tasks => (
                &mut self.selected_task,
                self.plan.as_ref().map_or(0, |plan| plan.tasks.len()),
            ),
            ProposalSection::Checks => (
                &mut self.selected_check,
                self.plan
                    .as_ref()
                    .map_or(0, |plan| plan.validation.commands.len()),
            ),
        };
        if len > 0 {
            *selected = selected
                .saturating_add_signed(delta)
                .min(len.saturating_sub(1));
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> AppAction {
        if self.editing_check_args {
            match key.code {
                KeyCode::Enter => {
                    match serde_json::from_str::<Vec<String>>(&self.check_args_buffer) {
                        Ok(args) => {
                            if let Some(command) = self.plan.as_mut().and_then(|plan| {
                                plan.validation.commands.get_mut(self.selected_check)
                            }) {
                                command.args = args;
                            }
                            self.editing = false;
                            self.editing_check_args = false;
                        }
                        Err(error) => {
                            self.message =
                                format!("Arguments must be a JSON string array: {error}");
                        }
                    }
                }
                KeyCode::Esc => {
                    self.editing = false;
                    self.editing_check_args = false;
                }
                KeyCode::Backspace => {
                    self.check_args_buffer.pop();
                }
                KeyCode::Char(character) => self.check_args_buffer.push(character),
                _ => {}
            }
            return AppAction::None;
        }
        if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            self.editing = false;
            self.editing_model = false;
            self.editing_prompt = false;
            return AppAction::None;
        }
        let Some(plan) = &mut self.plan else {
            self.editing = false;
            return AppAction::None;
        };
        let value = match self.proposal_section {
            ProposalSection::Agents if self.editing_model => plan.agents[self.selected_agent]
                .model
                .get_or_insert_with(String::new),
            ProposalSection::Agents if self.editing_prompt => {
                &mut plan.agents[self.selected_agent].prompt
            }
            ProposalSection::Agents => &mut plan.agents[self.selected_agent].role,
            ProposalSection::Tasks => &mut plan.tasks[self.selected_task].objective,
            ProposalSection::Checks => {
                if plan.validation.commands.is_empty() {
                    self.editing = false;
                    return AppAction::None;
                }
                &mut plan.validation.commands[self.selected_check].executable
            }
        };
        match key.code {
            KeyCode::Backspace => {
                value.pop();
            }
            KeyCode::Char(character) => value.push(character),
            _ => {}
        }
        AppAction::None
    }

    fn handle_policy_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('w') => self.cycle_workspace(),
            KeyCode::Char('a') => self.cycle_autonomy(),
            KeyCode::Char('!') if self.goal.autonomy_policy == Some(AutonomyPolicy::FullAccess) => {
                self.dangerous_confirmed = !self.dangerous_confirmed;
            }
            KeyCode::Enter => return AppAction::Launch,
            KeyCode::Esc => self.stage = Stage::Proposal,
            _ => {}
        }
        AppAction::None
    }

    fn handle_result_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('r') => {
                self.stage = Stage::Policy;
                self.message = "Review policies, then launch a clean retry.".into();
            }
            KeyCode::Char('h') => {
                self.history_rows = self.history.latest_runs(100).unwrap_or_default();
                self.selected_history = 0;
                self.stage = Stage::History;
            }
            KeyCode::Char('e') => {
                if let Some(run_id) = self.run_id {
                    let path = self.goal.repository.join(format!("teracode-{run_id}.json"));
                    self.message = match self.history.export_run(run_id, &path) {
                        Ok(()) => format!("Exported {}", path.display()),
                        Err(error) => format!("Export failed: {error}"),
                    };
                }
            }
            KeyCode::Char('b') => {
                if let Some(plan) = &self.plan {
                    let blueprint = FactoryBlueprint {
                        id: Uuid::new_v4(),
                        name: plan.name.clone(),
                        goal_template: self.goal.clone(),
                        plan: plan.clone(),
                    };
                    self.message = match self.history.save_blueprint(&blueprint) {
                        Ok(()) => format!(
                            "Saved reusable factory blueprint {} ({})",
                            blueprint.name, blueprint.id
                        ),
                        Err(error) => format!("Blueprint save failed: {error}"),
                    };
                }
            }
            KeyCode::Char('d') => {
                if let Some(run_id) = self.run_id {
                    self.message = match self.history.delete_run(run_id) {
                        Ok(true) => {
                            "Deleted this run from local history; worktrees were preserved.".into()
                        }
                        Ok(false) => "Run was already absent from local history.".into(),
                        Err(error) => format!("Delete failed: {error}"),
                    };
                }
            }
            KeyCode::Char('q') => return AppAction::Quit,
            _ => {}
        }
        AppAction::None
    }

    fn retry_selected_history(&mut self) {
        let Some(run) = self.history_rows.get(self.selected_history).cloned() else {
            self.message = "No historical run is selected.".into();
            return;
        };
        let parsed = serde_json::from_value::<GoalSpec>(run["goal"].clone()).and_then(|goal| {
            serde_json::from_value::<TeamPlan>(run["plan"].clone()).map(|plan| (goal, plan))
        });
        let Ok((mut goal, plan)) = parsed else {
            self.message = "This run does not contain a retryable goal and blueprint.".into();
            return;
        };
        if goal.repository != self.repository.path {
            self.message = format!(
                "Retry is limited to the active repository: {}",
                self.repository.path.display()
            );
            return;
        }
        goal.workspace_policy = None;
        goal.autonomy_policy = None;
        self.goal = goal;
        self.plan = Some(plan);
        self.dangerous_confirmed = false;
        self.outcome = None;
        self.run_id = None;
        self.run_started = None;
        self.run_elapsed = None;
        self.stage = Stage::Policy;
        self.message =
            "Historical blueprint loaded. Re-select both policies for a clean retry.".into();
    }

    fn export_selected_history(&mut self) {
        let Some(id) = self
            .history_rows
            .get(self.selected_history)
            .and_then(|run| run["id"].as_str())
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            self.message = "No historical run is selected.".into();
            return;
        };
        let path = self.goal.repository.join(format!("teracode-{id}.json"));
        self.message = match self.history.export_run(id, &path) {
            Ok(()) => format!("Exported {}", path.display()),
            Err(error) => format!("Export failed: {error}"),
        };
    }

    fn delete_selected_history(&mut self) {
        let Some(id) = self
            .history_rows
            .get(self.selected_history)
            .and_then(|run| run["id"].as_str())
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            self.message = "No historical run is selected.".into();
            return;
        };
        self.message = match self.history.delete_run(id) {
            Ok(true) => "Deleted the selected local run; worktrees were preserved.".into(),
            Ok(false) => "The selected run was already absent.".into(),
            Err(error) => format!("Delete failed: {error}"),
        };
        self.history_rows = self.history.latest_runs(100).unwrap_or_default();
        self.selected_history = self
            .selected_history
            .min(self.history_rows.len().saturating_sub(1));
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> AppAction {
        if self.stage == Stage::Policy
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.launch_area.contains((mouse.column, mouse.row).into())
        {
            AppAction::Launch
        } else {
            AppAction::None
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(self.palette.basalt)),
            area,
        );
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(2),
            ])
            .split(area);
        self.render_header(frame, sections[0]);
        self.render_rail(frame, sections[1]);
        match self.stage {
            Stage::Doctor => self.render_doctor(frame, sections[2]),
            Stage::Switchboard => self.render_switchboard(frame, sections[2]),
            Stage::Proposal => self.render_proposal(frame, sections[2]),
            Stage::Policy => self.render_policy(frame, sections[2]),
            Stage::Run => self.render_run(frame, sections[2]),
            Stage::Result => self.render_result(frame, sections[2]),
            Stage::History => self.render_history(frame, sections[2]),
        }
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " STATUS ",
                    Style::default()
                        .fg(self.palette.basalt)
                        .bg(self.palette.signal),
                ),
                Span::styled(&self.message, Style::default().fg(self.palette.paper)),
            ]))
            .style(Style::default().bg(self.palette.rail)),
            sections[3],
        );
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let title = if area.width < 95 {
            " TERACODE / FACTORY FOUNDRY "
        } else {
            " TERACODE / THE FACTORY FOR SOFTWARE FACTORIES "
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    title,
                    Style::default()
                        .fg(self.palette.basalt)
                        .bg(self.palette.active)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {} ", self.repository.path.display()),
                    Style::default().fg(self.palette.paper),
                ),
            ]))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(self.palette.rail)),
            ),
            area,
        );
    }

    fn render_rail(&self, frame: &mut Frame, area: Rect) {
        let labels = ["PLAN", "WORK", "VERIFY", "REWORK", "INTEGRATE"];
        let active = match self.stage {
            Stage::Doctor | Stage::Switchboard | Stage::Proposal | Stage::Policy => 0,
            Stage::Run if self.message.starts_with("QUALITY") => 2,
            Stage::Run if self.message.starts_with("REWORK") => 3,
            Stage::Run => 1,
            Stage::Result
                if self
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.state == RunState::Complete) =>
            {
                4
            }
            Stage::Result | Stage::History => 2,
        };
        let connector = if self.ascii { "---" } else { "━━━" };
        let mut spans = Vec::new();
        for (index, label) in labels.iter().enumerate() {
            let color = match index.cmp(&active) {
                Ordering::Less => self.palette.success,
                Ordering::Equal => self.palette.signal,
                Ordering::Greater => self.palette.rail,
            };
            spans.push(Span::styled(
                format!("[{label}]"),
                Style::default().fg(color).add_modifier(if index == active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ));
            if index + 1 < labels.len() {
                spans.push(Span::styled(
                    connector,
                    Style::default().fg(self.palette.rail),
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(self.palette.rail)),
            ),
            area,
        );
    }

    fn render_doctor(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(vec![
            Span::styled(
                if self.repository.is_git {
                    "[OK] "
                } else {
                    "[WARN] "
                },
                Style::default().fg(if self.repository.is_git {
                    self.palette.success
                } else {
                    self.palette.signal
                }),
            ),
            Span::raw(if self.repository.is_git {
                format!(
                    "Git / {} / {}",
                    self.repository.branch.as_deref().unwrap_or("detached"),
                    if self.repository.dirty {
                        "dirty source"
                    } else {
                        "clean source"
                    }
                )
            } else {
                "Non-Git directory / shared workspace only".into()
            }),
        ])];
        for probe in &self.probes {
            let (label, color) = if probe.installed {
                ("OK", self.palette.success)
            } else {
                ("MISS", self.palette.fault)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("[{label:4}] "), Style::default().fg(color)),
                Span::styled(
                    format!("{:<14}", probe.adapter),
                    Style::default().fg(self.palette.paper),
                ),
                Span::raw(probe.version.as_deref().unwrap_or("not installed")),
                Span::styled(
                    format!(
                        "  json:{} resume:{} model:{}",
                        yes_no(probe.capabilities.structured_output),
                        yes_no(probe.capabilities.resume),
                        yes_no(probe.capabilities.model_selection)
                    ),
                    Style::default().fg(self.palette.active),
                ),
            ]));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel(
                    " DOCTOR / LINE READINESS ",
                    self.palette.active,
                    true,
                ))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_switchboard(&self, frame: &mut Frame, area: Rect) {
        let selected_adapter = AdapterKind::ALL[self.selected_provider];
        let preferred = self
            .goal
            .provider_preferences
            .iter()
            .any(|preference| preference.adapter == selected_adapter && preference.preferred);
        let fields = [
            format!(
                "Goal             {}",
                if self.goal.goal.is_empty() {
                    "<type the product outcome>"
                } else {
                    &self.goal.goal
                }
            ),
            format!("Worker cells     {}", self.goal.team_size),
            format!("Parallel lanes   {}", self.goal.max_parallel),
            format!("Routing priority {}", self.goal.routing_objective),
            format!(
                "Provider bias     {} [{}]",
                selected_adapter,
                if preferred { "PREFERRED" } else { "neutral" }
            ),
            format!(
                "Constraint        {}",
                self.goal
                    .constraints
                    .first()
                    .filter(|constraint| !constraint.is_empty())
                    .map_or("<optional operating constraint>", String::as_str)
            ),
        ];
        let lines = fields
            .iter()
            .enumerate()
            .map(|(index, value)| {
                Line::from(vec![
                    Span::styled(
                        if index == self.switch_focus {
                            "> "
                        } else {
                            "  "
                        },
                        Style::default().fg(self.palette.signal),
                    ),
                    Span::styled(
                        value,
                        Style::default()
                            .fg(if index == self.switch_focus {
                                self.palette.paper
                            } else {
                                self.palette.active
                            })
                            .add_modifier(if index == self.switch_focus {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ])
            })
            .chain(std::iter::once(Line::raw("")))
            .chain(std::iter::once(Line::styled(
                format!(
                    "Quality gates    {} repository-native command(s)",
                    self.goal.acceptance_checks.len()
                ),
                Style::default().fg(self.palette.paper),
            )))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel(
                    " SWITCHBOARD / DEFINE THE LINE ",
                    self.palette.signal,
                    true,
                ))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_proposal(&self, frame: &mut Frame, area: Rect) {
        let Some(plan) = &self.plan else {
            return;
        };
        let skill_hint = plan
            .skills
            .get(self.selected_skill)
            .map_or("none", |skill| skill.name.as_str());
        let title = format!(
            " BLUEPRINT / {:?} / e edit · a provider · m model · p prompt · d deps · x agent · s skill ({skill_hint}) · g args · r required · Enter policies ",
            self.proposal_section,
        );
        let items = match self.proposal_section {
            ProposalSection::Agents => plan
                .agents
                .iter()
                .map(|agent| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                format!("{}  ", agent.id),
                                Style::default().fg(self.palette.active),
                            ),
                            Span::styled(
                                &agent.role,
                                Style::default()
                                    .fg(self.palette.paper)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::styled(
                            format!(
                                "  {} / model {} / skills {}",
                                agent.adapter,
                                agent.model.as_deref().unwrap_or("provider default"),
                                if agent.skills.is_empty() {
                                    "none".into()
                                } else {
                                    agent.skills.join(", ")
                                }
                            ),
                            Style::default().fg(self.palette.signal),
                        ),
                    ])
                })
                .collect(),
            ProposalSection::Tasks => plan
                .tasks
                .iter()
                .map(|task| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                format!("{}  ", task.id),
                                Style::default().fg(self.palette.active),
                            ),
                            Span::raw(format!("{:?} -> {}", task.kind, task.assigned_agent)),
                        ]),
                        Line::styled(
                            format!(
                                "  deps [{}] / {}",
                                task.dependencies.join(", "),
                                task.objective
                            ),
                            Style::default().fg(self.palette.paper),
                        ),
                    ])
                })
                .collect(),
            ProposalSection::Checks => {
                if plan.validation.commands.is_empty() {
                    vec![ListItem::new("[EMPTY] Add a quality gate with +")]
                } else {
                    plan.validation
                        .commands
                        .iter()
                        .map(|command| {
                            ListItem::new(format!(
                                "{} {:?} / {}s / {}",
                                command.executable,
                                command.args,
                                command.timeout_secs,
                                if command.required {
                                    "required"
                                } else {
                                    "optional"
                                }
                            ))
                        })
                        .collect()
                }
            }
        };
        let selected = match self.proposal_section {
            ProposalSection::Agents => self.selected_agent,
            ProposalSection::Tasks => self.selected_task,
            ProposalSection::Checks => self.selected_check,
        };
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(&title, self.palette.signal, true))
                .highlight_symbol(if self.ascii { ">> " } else { "▸▸ " })
                .highlight_style(
                    Style::default()
                        .bg(self.palette.rail)
                        .fg(self.palette.paper),
                ),
            area,
            &mut state,
        );
        if self.editing {
            let popup = centered_rect(76, 6, area);
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(if self.editing_check_args {
                    format!(
                        "Edit a JSON string array, for example [\"test\",\"--locked\"]\n{}",
                        self.check_args_buffer
                    )
                } else {
                    "Editing current value · Enter saves · Esc keeps current text".into()
                })
                .block(panel(" EDIT BLUEPRINT ", self.palette.signal, true))
                .style(
                    Style::default()
                        .bg(self.palette.basalt)
                        .fg(self.palette.paper),
                ),
                popup,
            );
        }
    }

    fn render_policy(&mut self, frame: &mut Frame, area: Rect) {
        let workspace = self
            .goal
            .workspace_policy
            .map_or("[REQUIRED]".into(), |policy| policy.to_string());
        let autonomy = self
            .goal
            .autonomy_policy
            .map_or("[REQUIRED]".into(), |policy| policy.to_string());
        let ready = self.goal.workspace_policy.is_some()
            && self.goal.autonomy_policy.is_some()
            && self.policy_error().is_none();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(3)])
            .split(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No process starts until both controls are explicitly selected.", Style::default().fg(self.palette.paper)),
                Line::raw(""),
                Line::from(vec![Span::styled("[w] WORKSPACE  ", Style::default().fg(self.palette.active)), Span::styled(workspace, Style::default().fg(self.palette.signal).add_modifier(Modifier::BOLD))]),
                Line::from(vec![Span::styled("[a] AUTONOMY  ", Style::default().fg(self.palette.active)), Span::styled(autonomy, Style::default().fg(self.palette.signal).add_modifier(Modifier::BOLD))]),
                Line::from(vec![Span::styled("[!] BYPASS     ", Style::default().fg(self.palette.fault)), Span::raw(if self.dangerous_confirmed { "CONFIRMED for this run" } else { "not confirmed" })]),
                Line::raw(""),
                Line::styled(
                    if self.goal.workspace_policy == Some(WorkspacePolicy::SharedWorkspace) {
                        "[WARN] Shared agents can collide; baseline changes remain but attribution and rollback are limited."
                    } else if self.repository.dirty {
                        "[WARN] Isolated lines start from committed HEAD; uncommitted changes are excluded."
                    } else {
                        "[OK] Isolated output remains on a dedicated integration branch and worktree."
                    },
                    Style::default().fg(if self.goal.workspace_policy == Some(WorkspacePolicy::SharedWorkspace) || self.repository.dirty { self.palette.signal } else { self.palette.success }),
                ),
            ])
            .block(panel(" POLICY GATE / TWO-KEY CONTROL ", self.palette.fault, true))
            .wrap(Wrap { trim: true }),
            chunks[0],
        );
        self.launch_area = chunks[1];
        frame.render_widget(
            Paragraph::new(if ready {
                "[ LAUNCH FACTORY ]  Enter or click"
            } else {
                "[ LOCKED ]  Select compatible controls"
            })
            .centered()
            .style(
                Style::default()
                    .fg(if ready {
                        self.palette.basalt
                    } else {
                        self.palette.paper
                    })
                    .bg(if ready {
                        self.palette.success
                    } else {
                        self.palette.rail
                    })
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if ready {
                        self.palette.success
                    } else {
                        self.palette.rail
                    })),
            ),
            chunks[1],
        );
    }

    fn render_run(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(if area.width >= 100 {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        let tasks = self
            .plan
            .as_ref()
            .into_iter()
            .flat_map(|plan| &plan.tasks)
            .map(|task| {
                let state = self
                    .task_states
                    .get(&task.id)
                    .copied()
                    .unwrap_or(AgentState::Queued);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("[{}] ", state_label(state)),
                        Style::default().fg(state_color(self.palette, state)),
                    ),
                    Span::styled(&task.id, Style::default().fg(self.palette.paper)),
                    Span::styled(
                        format!(" -> {}", task.assigned_agent),
                        Style::default().fg(self.palette.active),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(tasks).block(panel(
                &format!(
                    " LIVE TASK RAIL / {} ",
                    format_duration(
                        self.run_started
                            .map_or(Duration::ZERO, |start| start.elapsed())
                    )
                ),
                self.palette.active,
                true,
            )),
            chunks[0],
        );
        let logs = self
            .logs
            .iter()
            .rev()
            .take(usize::from(chunks[1].height.saturating_sub(2)))
            .rev()
            .map(|line| Line::raw(line.clone()))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(logs)
                .block(panel(" NORMALIZED EVENT FEED ", self.palette.rail, false))
                .wrap(Wrap { trim: true }),
            chunks[1],
        );
    }

    fn render_result(&self, frame: &mut Frame, area: Rect) {
        let Some(outcome) = &self.outcome else {
            return;
        };
        let success = outcome.state == RunState::Complete;
        let mut lines = vec![
            Line::styled(
                format!(
                    "[{}] {:?}",
                    if success { "PASS" } else { "STOP" },
                    outcome.state
                ),
                Style::default()
                    .fg(if success {
                        self.palette.success
                    } else {
                        self.palette.fault
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(format!("Completed tasks  {}", outcome.completed_tasks)),
            Line::raw(format!("Quality checks   {}", outcome.checks.len())),
            Line::raw(format!(
                "Elapsed          {}",
                format_duration(self.run_elapsed.unwrap_or_default())
            )),
        ];
        if let Some(path) = &outcome.integration_path {
            lines.push(Line::raw(format!("Integration line {}", path.display())));
        }
        if let Some(reason) = &outcome.reason {
            lines.push(Line::styled(
                format!("Reason            {reason}"),
                Style::default().fg(self.palette.fault),
            ));
        }
        if let Some(summary) = &outcome.diff_summary {
            lines.push(Line::styled(
                format!("Staged diff       {}", summary.replace('\n', " · ")),
                Style::default().fg(self.palette.active),
            ));
        }
        lines.extend([
            Line::raw(""),
            Line::styled(
                if success {
                    "Inspect the staged diff in the integration worktree. TeraCode did not merge the starting branch."
                } else {
                    "Artifacts and logs were preserved. Fix the blocker or retry from the policy gate."
                },
                Style::default().fg(self.palette.paper),
            ),
            Line::raw(""),
            Line::styled("[r] retry  [b] save blueprint  [e] export  [d] delete history  [h] history  [q] quit", Style::default().fg(self.palette.active)),
            Line::styled("Transcripts may contain repository content and remain local; no telemetry is sent.", Style::default().fg(self.palette.signal)),
        ]);
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel(
                    " RESULT / LINE MANIFEST ",
                    if success {
                        self.palette.success
                    } else {
                        self.palette.fault
                    },
                    true,
                ))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_history(&self, frame: &mut Frame, area: Rect) {
        let items = self
            .history_rows
            .iter()
            .map(|run| {
                ListItem::new(format!(
                    "[{}] {}  {}",
                    run["state"].as_str().unwrap_or("unknown").to_uppercase(),
                    run["id"].as_str().unwrap_or("missing-id"),
                    run["goal"]["goal"].as_str().unwrap_or("missing goal")
                ))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default()
            .with_selected((!items.is_empty()).then_some(self.selected_history));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(
                    " LOCAL FACTORY HISTORY / ↑↓ SELECT · Enter/r RETRY · e EXPORT · d DELETE · b BACK ",
                    self.palette.active,
                    true,
                ))
                .highlight_symbol(if self.ascii { ">> " } else { "▸▸ " })
                .highlight_style(Style::default().bg(self.palette.rail).fg(self.palette.paper)),
            area,
            &mut state,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppAction {
    None,
    Launch,
    Quit,
}

pub fn run(app: &mut App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let _guard = TerminalGuard;
    let (update_tx, mut update_rx) = mpsc::unbounded_channel();

    loop {
        while let Ok(update) = update_rx.try_recv() {
            app.handle_update(update);
        }
        terminal.draw(|frame| app.render(frame))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) => app.handle_key(key),
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
                AppAction::None
            }
        };
        match action {
            AppAction::None => {}
            AppAction::Quit => break,
            AppAction::Launch => {
                if let Some((run_id, goal, plan, dangerous_confirmed, cancellation)) =
                    app.begin_run()
                {
                    let updates = update_tx.clone();
                    tokio::spawn(execute_factory(
                        run_id,
                        goal,
                        plan,
                        dangerous_confirmed,
                        updates,
                        cancellation,
                    ));
                }
            }
        }
    }
    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

fn panel(title: &str, color: Color, focused: bool) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color).add_modifier(if focused {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }))
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Min(0),
        ])
        .split(vertical[1])[1]
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Queued => "WAIT",
        AgentState::Starting => "BOOT",
        AgentState::Working => "RUN",
        AgentState::Waiting => "HOLD",
        AgentState::Reviewing => "CHECK",
        AgentState::Complete => "DONE",
        AgentState::Failed => "FAIL",
        AgentState::Cancelled => "CANCEL",
        AgentState::Interrupted => "INT",
    }
}

fn state_color(palette: Palette, state: AgentState) -> Color {
    match state {
        AgentState::Complete => palette.success,
        AgentState::Failed | AgentState::Cancelled | AgentState::Interrupted => palette.fault,
        AgentState::Working | AgentState::Reviewing => palette.active,
        AgentState::Queued | AgentState::Starting | AgentState::Waiting => palette.signal,
    }
}

fn format_event(task_id: &str, event: &AgentEvent) -> String {
    match event {
        AgentEvent::AssistantText { text } => format!("[{task_id}] {text}"),
        AgentEvent::ToolActivity { tool, detail } => {
            format!(
                "[{task_id}] tool {tool} {}",
                detail.as_deref().unwrap_or_default()
            )
        }
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cost_usd,
        } => format!(
            "[{task_id}] usage in:{} out:{} cost:{}",
            input_tokens.map_or("unknown".into(), |value| value.to_string()),
            output_tokens.map_or("unknown".into(), |value| value.to_string()),
            cost_usd.map_or("unknown".into(), |value| format!("${value:.4}"))
        ),
        AgentEvent::Warning { message } | AgentEvent::Failed { message, .. } => {
            format!("[{task_id}] {message}")
        }
        AgentEvent::Completed { summary, .. } => {
            format!("[{task_id}] {}", summary.as_deref().unwrap_or("completed"))
        }
        AgentEvent::Diagnostic { source, raw } => format!("[{task_id}] {source}: {raw}"),
        _ => format!("[{task_id}] {event:?}"),
    }
}

fn discover_acceptance_checks(repository: &std::path::Path) -> Vec<AcceptanceCommand> {
    if repository.join("Cargo.toml").exists() {
        vec![
            AcceptanceCommand {
                executable: "cargo".into(),
                args: vec!["fmt".into(), "--all".into(), "--check".into()],
                timeout_secs: 120,
                required: true,
            },
            AcceptanceCommand {
                executable: "cargo".into(),
                args: vec!["test".into(), "--workspace".into()],
                timeout_secs: 600,
                required: true,
            },
        ]
    } else if repository.join("package.json").exists() {
        vec![AcceptanceCommand {
            executable: "npm".into(),
            args: vec!["test".into()],
            timeout_secs: 600,
            required: true,
        }]
    } else if repository.join("go.mod").exists() {
        vec![AcceptanceCommand {
            executable: "go".into(),
            args: vec!["test".into(), "./...".into()],
            timeout_secs: 600,
            required: true,
        }]
    } else if repository.join("pyproject.toml").exists() {
        vec![AcceptanceCommand {
            executable: "python3".into(),
            args: vec!["-m".into(), "pytest".into()],
            timeout_secs: 600,
            required: true,
        }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};
    use tempfile::tempdir;
    use teracode_core::AdapterCapabilities;

    use super::*;

    fn test_app() -> App {
        let directory = tempdir().unwrap();
        let repository_path = directory.keep();
        let history = HistoryStore::open(repository_path.join("history.db")).unwrap();
        App::new(
            RepositoryStatus {
                path: repository_path,
                is_git: true,
                head: Some("abc".into()),
                branch: Some("main".into()),
                dirty: false,
                changed_paths: Vec::new(),
            },
            vec![ProbeResult {
                adapter: AdapterKind::Codex,
                executable: "codex".into(),
                installed: true,
                version: Some("codex 1.0".into()),
                readiness: ProbeReadiness::Unknown,
                capabilities: AdapterCapabilities {
                    structured_output: true,
                    resume: true,
                    model_selection: true,
                    read_only: true,
                    workspace_write: true,
                    full_access: true,
                },
                diagnostic: None,
            }],
            SkillIndex::default(),
            history,
            TeraCodeConfig::default(),
            true,
        )
    }

    #[test]
    fn renders_at_compact_and_normal_terminal_sizes() {
        for (width, height) in [(80, 24), (120, 36)] {
            let mut app = test_app();
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("TERACODE"));
            assert!(rendered.contains("DOCTOR"));
        }
    }

    #[test]
    fn keyboard_flow_requires_both_policies() {
        let mut app = test_app();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for character in "Build a library".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.stage, Stage::Proposal);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.stage, Stage::Policy);
        assert!(app.begin_run().is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.begin_run().is_some());
    }
}
