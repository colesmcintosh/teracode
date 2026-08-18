use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use teracode_adapters::{AgentAdapter, InvocationContext, ProcessEvent, adapters, run_supervised};
use teracode_core::{
    AgentEvent, AgentProfile, AgentState, AutonomyPolicy, CheckResult, GoalSpec, RunState,
    ScheduleError, TaskExecution, TaskKind, TaskNode, TaskRunner, TeamPlan, WorkspaceManager,
    WorkspacePolicy, WorkspaceSet, execute_plan, run_acceptance_checks,
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug)]
pub enum FactoryUpdate {
    Status(String),
    TaskState {
        task_id: String,
        agent_id: String,
        state: AgentState,
    },
    Event {
        task_id: String,
        agent_id: String,
        event: AgentEvent,
    },
    Check(CheckResult),
    Workspace {
        integration_path: PathBuf,
        warning: Option<String>,
    },
    Finished(FactoryOutcome),
}

#[derive(Debug, Clone)]
pub struct FactoryOutcome {
    pub state: RunState,
    pub reason: Option<String>,
    pub integration_path: Option<PathBuf>,
    pub completed_tasks: usize,
    pub checks: Vec<CheckResult>,
    pub diff_summary: Option<String>,
}

pub async fn execute_factory(
    run_id: Uuid,
    goal: GoalSpec,
    plan: TeamPlan,
    dangerous_confirmed: bool,
    updates: mpsc::UnboundedSender<FactoryUpdate>,
    cancellation: CancellationToken,
) {
    let outcome = execute_factory_inner(
        run_id,
        goal,
        plan,
        dangerous_confirmed,
        updates.clone(),
        cancellation,
    )
    .await;
    let _ = updates.send(FactoryUpdate::Finished(outcome));
}

async fn execute_factory_inner(
    run_id: Uuid,
    goal: GoalSpec,
    plan: TeamPlan,
    dangerous_confirmed: bool,
    updates: mpsc::UnboundedSender<FactoryUpdate>,
    cancellation: CancellationToken,
) -> FactoryOutcome {
    let worktree_root = goal
        .repository
        .parent()
        .unwrap_or(&goal.repository)
        .join(".teracode-worktrees");
    let manager = WorkspaceManager::new(goal.repository.clone(), worktree_root, run_id);
    let policy = goal
        .workspace_policy
        .expect("the policy gate guarantees a workspace policy");
    let agent_ids = plan
        .agents
        .iter()
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    let prepare_manager = manager.clone();
    let workspaces = match tokio::task::spawn_blocking(move || {
        prepare_manager.prepare(policy, &agent_ids)
    })
    .await
    {
        Ok(Ok(workspaces)) => workspaces,
        Ok(Err(error)) => {
            return failed_outcome(RunState::Blocked, error.to_string(), 0, None);
        }
        Err(error) => {
            return failed_outcome(RunState::Failed, error.to_string(), 0, None);
        }
    };
    let _ = updates.send(FactoryUpdate::Workspace {
        integration_path: workspaces.integration_path.clone(),
        warning: workspaces.warning.clone(),
    });

    let runner = Arc::new(CliTaskRunner::new(
        goal.clone(),
        &plan,
        manager.clone(),
        workspaces.clone(),
        dangerous_confirmed,
        updates.clone(),
        cancellation.clone(),
    ));
    let executions = match execute_plan(
        &plan,
        goal.max_parallel,
        Arc::clone(&runner),
        cancellation.clone(),
    )
    .await
    {
        Ok(executions) => executions,
        Err(ScheduleError::Cancelled) => {
            return failed_outcome(
                RunState::Cancelled,
                "Run cancelled; process groups were stopped and workspaces were preserved.".into(),
                0,
                Some(workspaces.integration_path),
            );
        }
        Err(error) => {
            return failed_outcome(
                RunState::Blocked,
                error.to_string(),
                0,
                Some(workspaces.integration_path),
            );
        }
    };

    let _ = updates.send(FactoryUpdate::Status(
        "QUALITY GATE / running checks".into(),
    ));
    let mut checks = match run_acceptance_checks(
        &workspaces.integration_path,
        &plan.validation.commands,
    )
    .await
    {
        Ok(checks) => checks,
        Err(error) => {
            return failed_outcome(
                RunState::Failed,
                error.to_string(),
                executions.len(),
                Some(workspaces.integration_path),
            );
        }
    };
    for check in &checks {
        let _ = updates.send(FactoryUpdate::Check(check.clone()));
    }

    let mut rework_pass = 0;
    while required_check_failed(&checks) && rework_pass < plan.validation.max_rework_passes {
        rework_pass += 1;
        let _ = updates.send(FactoryUpdate::Status(format!(
            "REWORK / targeted pass {rework_pass} of {}",
            plan.validation.max_rework_passes
        )));
        let Some(mut task) = plan
            .tasks
            .iter()
            .find(|task| task.id == plan.final_integration_task)
            .cloned()
        else {
            break;
        };
        let failures = checks
            .iter()
            .filter(|check| check.command.required && !check.passed)
            .map(|check| {
                format!(
                    "{} {:?}: {}",
                    check.command.executable, check.command.args, check.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        task.objective = format!(
            "Target only the acceptance failures below. Preserve successful work and rerun relevant checks.\n{failures}"
        );
        if let Err(error) = runner.run_task(task, rework_pass).await {
            return failed_outcome(
                RunState::Blocked,
                format!("rework failed: {error}"),
                executions.len(),
                Some(workspaces.integration_path),
            );
        }
        checks =
            match run_acceptance_checks(&workspaces.integration_path, &plan.validation.commands)
                .await
            {
                Ok(checks) => checks,
                Err(error) => {
                    return failed_outcome(
                        RunState::Failed,
                        error.to_string(),
                        executions.len(),
                        Some(workspaces.integration_path),
                    );
                }
            };
        for check in &checks {
            let _ = updates.send(FactoryUpdate::Check(check.clone()));
        }
    }

    if required_check_failed(&checks) {
        return FactoryOutcome {
            state: RunState::Blocked,
            reason: Some(format!(
                "Required acceptance checks still fail after {rework_pass} rework pass(es)."
            )),
            integration_path: Some(workspaces.integration_path),
            completed_tasks: executions.len(),
            checks,
            diff_summary: None,
        };
    }

    let stage_manager = manager.clone();
    let stage_workspaces = workspaces.clone();
    let stage_result =
        tokio::task::spawn_blocking(move || stage_manager.stage_integration(&stage_workspaces))
            .await;
    let stage_error = match stage_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(error) => Some(error.to_string()),
    };
    if let Some(error) = stage_error {
        return failed_outcome(
            RunState::Blocked,
            format!("could not stage the integration result: {error}"),
            executions.len(),
            Some(workspaces.integration_path),
        );
    }

    let summary_manager = manager;
    let summary_workspaces = workspaces.clone();
    let diff_summary = tokio::task::spawn_blocking(move || {
        summary_manager.integration_diff_summary(&summary_workspaces)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .flatten();

    FactoryOutcome {
        state: RunState::Complete,
        reason: None,
        integration_path: Some(workspaces.integration_path),
        completed_tasks: executions.len(),
        checks,
        diff_summary,
    }
}

fn failed_outcome(
    state: RunState,
    reason: String,
    completed_tasks: usize,
    integration_path: Option<PathBuf>,
) -> FactoryOutcome {
    FactoryOutcome {
        state,
        reason: Some(reason),
        integration_path,
        completed_tasks,
        checks: Vec::new(),
        diff_summary: None,
    }
}

fn required_check_failed(checks: &[CheckResult]) -> bool {
    checks
        .iter()
        .any(|check| check.command.required && !check.passed)
}

struct CliTaskRunner {
    goal: GoalSpec,
    profiles: HashMap<String, AgentProfile>,
    manager: WorkspaceManager,
    workspaces: WorkspaceSet,
    updates: mpsc::UnboundedSender<FactoryUpdate>,
    cancellation: CancellationToken,
    dangerous_confirmed: bool,
    assembled: Mutex<bool>,
    summaries: Mutex<HashMap<String, String>>,
}

impl CliTaskRunner {
    fn new(
        goal: GoalSpec,
        plan: &TeamPlan,
        manager: WorkspaceManager,
        workspaces: WorkspaceSet,
        dangerous_confirmed: bool,
        updates: mpsc::UnboundedSender<FactoryUpdate>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            goal,
            profiles: plan
                .agents
                .iter()
                .map(|profile| (profile.id.clone(), profile.clone()))
                .collect(),
            manager,
            workspaces,
            dangerous_confirmed,
            updates,
            cancellation,
            assembled: Mutex::new(false),
            summaries: Mutex::new(HashMap::new()),
        }
    }

    async fn ensure_assembled(&self) -> Result<(), String> {
        if self.workspaces.policy != WorkspacePolicy::WorktreePerAgent {
            return Ok(());
        }
        let mut assembled = self.assembled.lock().await;
        if *assembled {
            return Ok(());
        }
        let manager = self.manager.clone();
        let workspaces = self.workspaces.clone();
        tokio::task::spawn_blocking(move || manager.assemble(&workspaces))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        *assembled = true;
        Ok(())
    }

    fn task_directory(&self, task: &TaskNode) -> Result<PathBuf, String> {
        if matches!(task.kind, TaskKind::Review | TaskKind::Integration) {
            return Ok(self.workspaces.integration_path.clone());
        }
        self.manager
            .path_for_agent(&self.workspaces, &task.assigned_agent)
            .map(PathBuf::from)
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl TaskRunner for CliTaskRunner {
    async fn run_task(&self, task: TaskNode, attempt: u8) -> Result<TaskExecution, String> {
        if matches!(task.kind, TaskKind::Review | TaskKind::Integration) {
            self.ensure_assembled().await?;
        }
        let profile = self
            .profiles
            .get(&task.assigned_agent)
            .ok_or_else(|| format!("missing profile {}", task.assigned_agent))?;
        let adapter: Arc<dyn AgentAdapter> = adapters()
            .into_iter()
            .find(|adapter| adapter.kind() == profile.adapter)
            .map(Arc::from)
            .ok_or_else(|| format!("missing adapter {}", profile.adapter))?;
        let current_dir = self.task_directory(&task)?;
        let dependency_summaries = {
            let summaries = self.summaries.lock().await;
            task.dependencies
                .iter()
                .filter_map(|dependency| {
                    summaries
                        .get(dependency)
                        .map(|summary| format!("{dependency}: {summary}"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let autonomy = if matches!(task.kind, TaskKind::Analysis | TaskKind::Review)
            || (self.workspaces.policy == WorkspacePolicy::ReadOnlyThenExecutor
                && task.kind != TaskKind::Integration)
        {
            AutonomyPolicy::ReadOnly
        } else {
            self.goal
                .autonomy_policy
                .expect("the policy gate guarantees autonomy")
        };
        let prompt = format!(
            "{}\n\nAssigned production task: {}\nExpected artifacts: {}\nUpstream production summaries:\n{}\nAttempt: {}. Do not spawn subagents. Work only in the current workspace and return a concise completion summary.",
            profile.prompt,
            task.objective,
            task.expected_artifacts.join(", "),
            if dependency_summaries.is_empty() {
                "none"
            } else {
                &dependency_summaries
            },
            attempt
        );
        let invocation = adapter
            .build_invocation(&InvocationContext {
                prompt,
                current_dir,
                model: profile.model.clone(),
                autonomy,
                dangerous_confirmed: self.dangerous_confirmed,
            })
            .map_err(|error| error.to_string())?;

        let _ = self.updates.send(FactoryUpdate::TaskState {
            task_id: task.id.clone(),
            agent_id: task.assigned_agent.clone(),
            state: AgentState::Working,
        });
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ProcessEvent>();
        let cancellation = self.cancellation.clone();
        let adapter_for_run = Arc::clone(&adapter);
        let run = tokio::spawn(async move {
            run_supervised(
                adapter_for_run.as_ref(),
                &invocation,
                cancellation,
                Some(event_tx),
            )
            .await
        });
        tokio::pin!(run);
        let mut event_stream_open = true;
        let output = loop {
            tokio::select! {
                event = event_rx.recv(), if event_stream_open => {
                    match event {
                        Some(event) => {
                            let _ = self.updates.send(FactoryUpdate::Event {
                                task_id: task.id.clone(),
                                agent_id: task.assigned_agent.clone(),
                                event: event.event,
                            });
                        }
                        None => event_stream_open = false,
                    }
                }
                result = &mut run => {
                    break result.map_err(|error| error.to_string())?.map_err(|error| error.to_string())?;
                }
            }
        };
        while let Ok(event) = event_rx.try_recv() {
            let _ = self.updates.send(FactoryUpdate::Event {
                task_id: task.id.clone(),
                agent_id: task.assigned_agent.clone(),
                event: event.event,
            });
        }
        let state = if output.success() {
            AgentState::Complete
        } else if output.cancelled {
            AgentState::Cancelled
        } else {
            AgentState::Failed
        };
        let _ = self.updates.send(FactoryUpdate::TaskState {
            task_id: task.id.clone(),
            agent_id: task.assigned_agent.clone(),
            state,
        });
        if !output.success() {
            let stderr = output.stderr.join("\n");
            let reason = (!stderr.trim().is_empty()).then_some(stderr).or_else(|| {
                output
                    .events
                    .iter()
                    .rev()
                    .find_map(|event| match &event.event {
                        AgentEvent::Failed { message, .. } => Some(message.clone()),
                        _ => None,
                    })
            });
            return Err(
                reason.unwrap_or_else(|| format!("{} exited unsuccessfully", profile.adapter))
            );
        }
        let events = output
            .events
            .into_iter()
            .map(|event| event.event)
            .collect::<Vec<_>>();
        let summary = events
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::Completed { summary, .. } => summary.clone(),
                AgentEvent::AssistantText { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "Agent completed without a textual summary.".into());
        self.summaries
            .lock()
            .await
            .insert(task.id.clone(), summary.clone());
        Ok(TaskExecution {
            task_id: task.id,
            attempts: attempt,
            summary,
            events,
        })
    }
}
