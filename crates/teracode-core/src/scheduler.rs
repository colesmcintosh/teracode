use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    sync::Semaphore,
    task::JoinSet,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{AgentEvent, TaskNode, TeamPlan};

#[derive(Debug, Clone)]
pub struct TaskExecution {
    pub task_id: String,
    pub attempts: u8,
    pub summary: String,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error("maximum parallelism must be at least one")]
    InvalidParallelism,
    #[error("task {task_id} failed after {attempts} attempt(s): {message}")]
    TaskFailed {
        task_id: String,
        attempts: u8,
        message: String,
    },
    #[error("the task graph cannot make progress")]
    Deadlock,
    #[error("run cancelled")]
    Cancelled,
    #[error("task process panicked: {0}")]
    Join(String),
}

#[async_trait]
pub trait TaskRunner: Send + Sync + 'static {
    async fn run_task(&self, task: TaskNode, attempt: u8) -> Result<TaskExecution, String>;
}

pub async fn execute_plan<R: TaskRunner>(
    plan: &TeamPlan,
    max_parallel: usize,
    runner: Arc<R>,
    cancellation: CancellationToken,
) -> Result<Vec<TaskExecution>, ScheduleError> {
    if max_parallel == 0 {
        return Err(ScheduleError::InvalidParallelism);
    }
    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut completed = HashSet::new();
    let mut running_tasks = HashSet::new();
    let mut busy_agents = HashSet::new();
    let mut attempts = std::collections::HashMap::<String, u8>::new();
    let mut results = Vec::new();
    let mut joins = JoinSet::new();

    while completed.len() < plan.tasks.len() {
        if cancellation.is_cancelled() {
            stop_running_tasks(&mut joins).await;
            return Err(ScheduleError::Cancelled);
        }

        for task in &plan.tasks {
            if completed.contains(&task.id)
                || running_tasks.contains(&task.id)
                || busy_agents.contains(&task.assigned_agent)
                || !task
                    .dependencies
                    .iter()
                    .all(|dependency| completed.contains(dependency))
            {
                continue;
            }
            let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                break;
            };
            let task = task.clone();
            let task_id = task.id.clone();
            let agent_id = task.assigned_agent.clone();
            let attempt = attempts.entry(task_id.clone()).or_insert(0);
            *attempt += 1;
            let current_attempt = *attempt;
            let runner = Arc::clone(&runner);
            running_tasks.insert(task_id.clone());
            busy_agents.insert(agent_id.clone());
            joins.spawn(async move {
                let _permit = permit;
                let result = runner.run_task(task, current_attempt).await;
                (task_id, agent_id, current_attempt, result)
            });
        }

        if joins.is_empty() {
            return Err(ScheduleError::Deadlock);
        }

        let joined = tokio::select! {
            () = cancellation.cancelled() => {
                stop_running_tasks(&mut joins).await;
                return Err(ScheduleError::Cancelled);
            }
            joined = joins.join_next() => joined,
        };
        let Some(joined) = joined else {
            return Err(ScheduleError::Deadlock);
        };
        let (task_id, agent_id, attempt, result) =
            joined.map_err(|error| ScheduleError::Join(error.to_string()))?;
        running_tasks.remove(&task_id);
        busy_agents.remove(&agent_id);
        match result {
            Ok(mut execution) => {
                execution.attempts = attempt;
                completed.insert(task_id);
                results.push(execution);
            }
            Err(message) => {
                let retry_limit = plan
                    .tasks
                    .iter()
                    .find(|task| task.id == task_id)
                    .map_or(0, |task| task.retry_limit);
                if attempt > retry_limit {
                    joins.abort_all();
                    return Err(ScheduleError::TaskFailed {
                        task_id,
                        attempts: attempt,
                        message,
                    });
                }
            }
        }
    }
    Ok(results)
}

async fn stop_running_tasks(
    joins: &mut JoinSet<(String, String, u8, Result<TaskExecution, String>)>,
) {
    let graceful = async { while joins.join_next().await.is_some() {} };
    if timeout(Duration::from_secs(5), graceful).await.is_err() {
        joins.abort_all();
        while joins.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::time::{Duration, sleep};

    use super::*;
    use crate::{
        AdapterCapabilities, AdapterKind, AgentProfile, TEAM_PLAN_SCHEMA_VERSION, TaskKind,
        ValidationStrategy,
    };

    struct MeasuringRunner {
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    struct RetryRunner {
        attempts: AtomicUsize,
    }

    #[async_trait]
    impl TaskRunner for RetryRunner {
        async fn run_task(&self, task: TaskNode, attempt: u8) -> Result<TaskExecution, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                return Err("transient".into());
            }
            Ok(TaskExecution {
                task_id: task.id,
                attempts: attempt,
                summary: "recovered".into(),
                events: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl TaskRunner for MeasuringRunner {
        async fn run_task(&self, task: TaskNode, _: u8) -> Result<TaskExecution, String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(TaskExecution {
                task_id: task.id,
                attempts: 0,
                summary: "done".into(),
                events: Vec::new(),
            })
        }
    }

    fn plan() -> TeamPlan {
        let capabilities = AdapterCapabilities {
            structured_output: true,
            resume: true,
            model_selection: true,
            read_only: true,
            workspace_write: true,
            full_access: false,
        };
        TeamPlan {
            schema_version: TEAM_PLAN_SCHEMA_VERSION,
            name: "test".into(),
            agents: (0..3)
                .map(|index| AgentProfile {
                    id: format!("agent-{index}"),
                    role: "builder".into(),
                    adapter: AdapterKind::Codex,
                    model: None,
                    prompt: String::new(),
                    skills: Vec::new(),
                    capabilities: capabilities.clone(),
                })
                .collect(),
            tasks: (0..3)
                .map(|index| TaskNode {
                    id: format!("task-{index}"),
                    kind: if index == 2 {
                        TaskKind::Integration
                    } else {
                        TaskKind::Implementation
                    },
                    dependencies: Vec::new(),
                    assigned_agent: format!("agent-{index}"),
                    objective: String::new(),
                    expected_artifacts: Vec::new(),
                    write_scope: Vec::new(),
                    retry_limit: 0,
                })
                .collect(),
            skills: Vec::new(),
            validation: ValidationStrategy {
                reviewer_task: "task-1".into(),
                commands: Vec::new(),
                max_rework_passes: 2,
            },
            final_integration_task: "task-2".into(),
        }
    }

    #[tokio::test]
    async fn enforces_parallel_limit() {
        let runner = Arc::new(MeasuringRunner {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let results = execute_plan(&plan(), 2, Arc::clone(&runner), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(runner.peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retries_up_to_the_task_limit() {
        let mut plan = plan();
        plan.tasks.truncate(1);
        plan.tasks[0].kind = TaskKind::Integration;
        plan.tasks[0].retry_limit = 1;
        plan.validation.reviewer_task = plan.tasks[0].id.clone();
        plan.final_integration_task = plan.tasks[0].id.clone();
        let runner = Arc::new(RetryRunner {
            attempts: AtomicUsize::new(0),
        });

        let results = execute_plan(&plan, 1, Arc::clone(&runner), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(runner.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(results[0].attempts, 2);
    }
}
