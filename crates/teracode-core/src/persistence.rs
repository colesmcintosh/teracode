use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::{AgentEvent, CheckResult, GoalSpec, RunState, TeamPlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetentionPolicy {
    KeepForever,
    KeepLatest(u32),
    MaxAgeDays(u32),
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("cannot determine the local application data directory")]
    NoApplicationDirectory,
    #[error("cannot create application data directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("cannot write export {path}: {source}")]
    Export {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub struct HistoryStore {
    connection: Connection,
    path: PathBuf,
}

impl HistoryStore {
    pub fn open_default() -> Result<Self, PersistenceError> {
        let directories = ProjectDirs::from("dev", "teracode", "TeraCode")
            .ok_or(PersistenceError::NoApplicationDirectory)?;
        let data_dir = directories.data_dir();
        fs::create_dir_all(data_dir).map_err(|source| PersistenceError::CreateDirectory {
            path: data_dir.to_path_buf(),
            source,
        })?;
        Self::open(data_dir.join("history.sqlite3"))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { connection, path };
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<(), PersistenceError> {
        self.connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                goal_json TEXT NOT NULL,
                plan_json TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                integration_path TEXT,
                blocked_reason TEXT
            );
            CREATE TABLE IF NOT EXISTS state_transitions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                state TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                task_id TEXT,
                agent_id TEXT,
                event_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS acceptance_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                result_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS artifacts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                task_id TEXT,
                kind TEXT NOT NULL,
                path TEXT NOT NULL,
                metadata_json TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS adapter_probes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT REFERENCES runs(id) ON DELETE CASCADE,
                adapter TEXT NOT NULL,
                probe_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS blueprints (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                goal_json TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            PRAGMA user_version = 1;
            ",
        )?;
        Ok(())
    }

    pub fn create_run(
        &self,
        id: Uuid,
        goal: &GoalSpec,
        plan: Option<&TeamPlan>,
    ) -> Result<(), PersistenceError> {
        let now = unix_time();
        let state = serde_json::to_string(&RunState::Draft)?;
        self.connection.execute(
            "INSERT INTO runs (id, state, goal_json, plan_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                id.to_string(),
                state,
                serde_json::to_string(goal)?,
                plan.map(serde_json::to_string).transpose()?,
                now
            ],
        )?;
        self.record_transition(id, RunState::Draft)
    }

    pub fn save_plan(&self, id: Uuid, plan: &TeamPlan) -> Result<(), PersistenceError> {
        self.connection.execute(
            "UPDATE runs SET plan_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.to_string(), serde_json::to_string(plan)?, unix_time()],
        )?;
        Ok(())
    }

    pub fn set_state(
        &self,
        id: Uuid,
        state: RunState,
        reason: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "UPDATE runs SET state = ?2, blocked_reason = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                serde_json::to_string(&state)?,
                reason,
                unix_time()
            ],
        )?;
        self.record_transition(id, state)
    }

    fn record_transition(&self, id: Uuid, state: RunState) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO state_transitions (run_id, state, created_at) VALUES (?1, ?2, ?3)",
            params![id.to_string(), serde_json::to_string(&state)?, unix_time()],
        )?;
        Ok(())
    }

    pub fn record_event(
        &self,
        id: Uuid,
        task_id: Option<&str>,
        agent_id: Option<&str>,
        event: &AgentEvent,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO events (run_id, task_id, agent_id, event_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.to_string(),
                task_id,
                agent_id,
                serde_json::to_string(event)?,
                unix_time()
            ],
        )?;
        Ok(())
    }

    pub fn record_check(&self, id: Uuid, result: &CheckResult) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO acceptance_results (run_id, result_json, created_at) VALUES (?1, ?2, ?3)",
            params![id.to_string(), serde_json::to_string(result)?, unix_time()],
        )?;
        Ok(())
    }

    pub fn record_artifact(
        &self,
        id: Uuid,
        task_id: Option<&str>,
        kind: &str,
        path: &Path,
        metadata: Option<&Value>,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO artifacts (run_id, task_id, kind, path, metadata_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                task_id,
                kind,
                path.to_string_lossy(),
                metadata.map(serde_json::to_string).transpose()?,
                unix_time()
            ],
        )?;
        Ok(())
    }

    pub fn record_probe<T: Serialize>(
        &self,
        run_id: Option<Uuid>,
        adapter: &str,
        probe: &T,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO adapter_probes (run_id, adapter, probe_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id.map(|id| id.to_string()),
                adapter,
                serde_json::to_string(probe)?,
                unix_time()
            ],
        )?;
        Ok(())
    }

    pub fn set_integration_path(&self, id: Uuid, path: &Path) -> Result<(), PersistenceError> {
        self.connection.execute(
            "UPDATE runs SET integration_path = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.to_string(), path.to_string_lossy(), unix_time()],
        )?;
        Ok(())
    }

    pub fn save_blueprint(
        &self,
        blueprint: &crate::FactoryBlueprint,
    ) -> Result<(), PersistenceError> {
        self.connection.execute(
            "INSERT INTO blueprints (id, name, goal_json, plan_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                goal_json = excluded.goal_json,
                plan_json = excluded.plan_json,
                updated_at = excluded.updated_at",
            params![
                blueprint.id.to_string(),
                blueprint.name,
                serde_json::to_string(&blueprint.goal_template)?,
                serde_json::to_string(&blueprint.plan)?,
                unix_time()
            ],
        )?;
        Ok(())
    }

    pub fn list_blueprints(&self) -> Result<Vec<crate::FactoryBlueprint>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, goal_json, plan_json FROM blueprints ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, goal, plan) = row?;
            Ok(crate::FactoryBlueprint {
                id: Uuid::parse_str(&id).map_err(|error| {
                    serde_json::Error::io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error,
                    ))
                })?,
                name,
                goal_template: serde_json::from_str(&goal)?,
                plan: serde_json::from_str(&plan)?,
            })
        })
        .collect()
    }

    pub fn mark_unfinished_interrupted(&self) -> Result<usize, PersistenceError> {
        let mut statement = self.connection.prepare("SELECT id, state FROM runs")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let unfinished = rows
            .filter_map(Result::ok)
            .filter_map(|(id, state)| {
                serde_json::from_str::<RunState>(&state)
                    .ok()
                    .filter(|state| !state.is_terminal())
                    .map(|_| id)
            })
            .collect::<Vec<_>>();
        for id in &unfinished {
            self.connection.execute(
                "UPDATE runs SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![
                    id,
                    serde_json::to_string(&RunState::Interrupted)?,
                    unix_time()
                ],
            )?;
            if let Ok(id) = Uuid::parse_str(id) {
                self.record_transition(id, RunState::Interrupted)?;
            }
        }
        Ok(unfinished.len())
    }

    pub fn latest_runs(&self, limit: u32) -> Result<Vec<Value>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT id, state, goal_json, plan_json, created_at, updated_at, integration_path,
                    blocked_reason
             FROM runs ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            let goal: String = row.get(2)?;
            let plan: Option<String> = row.get(3)?;
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "state": serde_json::from_str::<Value>(&row.get::<_, String>(1)?).unwrap_or(Value::Null),
                "goal": serde_json::from_str::<Value>(&goal).unwrap_or(Value::Null),
                "plan": plan.and_then(|value| serde_json::from_str::<Value>(&value).ok()),
                "created_at": row.get::<_, i64>(4)?,
                "updated_at": row.get::<_, i64>(5)?,
                "integration_path": row.get::<_, Option<String>>(6)?,
                "blocked_reason": row.get::<_, Option<String>>(7)?,
            }))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn export_run(&self, id: Uuid, path: &Path) -> Result<(), PersistenceError> {
        let run = self
            .connection
            .query_row(
                "SELECT state, goal_json, plan_json, created_at, updated_at, integration_path,
                        blocked_reason FROM runs WHERE id = ?1",
                [id.to_string()],
                |row| {
                    Ok(json!({
                        "id": id,
                        "state": serde_json::from_str::<Value>(&row.get::<_, String>(0)?).unwrap_or(Value::Null),
                        "goal": serde_json::from_str::<Value>(&row.get::<_, String>(1)?).unwrap_or(Value::Null),
                        "plan": row.get::<_, Option<String>>(2)?.and_then(|value| serde_json::from_str::<Value>(&value).ok()),
                        "created_at": row.get::<_, i64>(3)?,
                        "updated_at": row.get::<_, i64>(4)?,
                        "integration_path": row.get::<_, Option<String>>(5)?,
                        "blocked_reason": row.get::<_, Option<String>>(6)?,
                    }))
                },
            )
            .optional()?;
        let mut export = run.unwrap_or_else(|| json!({"id": id, "missing": true}));
        let events = self.json_rows(
            "SELECT event_json FROM events WHERE run_id = ?1 ORDER BY id",
            id,
        )?;
        let checks = self.json_rows(
            "SELECT result_json FROM acceptance_results WHERE run_id = ?1 ORDER BY id",
            id,
        )?;
        export["events"] = Value::Array(events);
        export["acceptance_results"] = Value::Array(checks);
        let serialized = serde_json::to_vec_pretty(&export)?;
        fs::write(path, serialized).map_err(|source| PersistenceError::Export {
            path: path.to_path_buf(),
            source,
        })
    }

    fn json_rows(&self, query: &str, id: Uuid) -> Result<Vec<Value>, PersistenceError> {
        let mut statement = self.connection.prepare(query)?;
        let rows = statement.query_map([id.to_string()], |row| row.get::<_, String>(0))?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|value| serde_json::from_str(&value).ok())
            .collect())
    }

    pub fn delete_run(&self, id: Uuid) -> Result<bool, PersistenceError> {
        Ok(self
            .connection
            .execute("DELETE FROM runs WHERE id = ?1", [id.to_string()])?
            > 0)
    }

    pub fn apply_retention(&self, policy: RetentionPolicy) -> Result<usize, PersistenceError> {
        let removed = match policy {
            RetentionPolicy::KeepForever => 0,
            RetentionPolicy::KeepLatest(count) => self.connection.execute(
                "DELETE FROM runs WHERE id IN (
                    SELECT id FROM runs ORDER BY created_at DESC LIMIT -1 OFFSET ?1
                 )",
                [count],
            )?,
            RetentionPolicy::MaxAgeDays(days) => {
                let cutoff = unix_time() - i64::from(days) * 86_400;
                self.connection
                    .execute("DELETE FROM runs WHERE updated_at < ?1", [cutoff])?
            }
        };
        Ok(removed)
    }
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::{AutonomyPolicy, RoutingObjective, WorkspacePolicy};

    fn goal() -> GoalSpec {
        GoalSpec {
            repository: PathBuf::from("/repo"),
            goal: "test".into(),
            team_size: 1,
            max_parallel: 1,
            routing_objective: RoutingObjective::Balanced,
            constraints: Vec::new(),
            provider_preferences: Vec::new(),
            acceptance_checks: Vec::new(),
            workspace_policy: Some(WorkspacePolicy::SharedWorkspace),
            autonomy_policy: Some(AutonomyPolicy::ReadOnly),
        }
    }

    #[test]
    fn unfinished_runs_are_recovered_as_interrupted() {
        let directory = tempdir().unwrap();
        let store = HistoryStore::open(directory.path().join("history.db")).unwrap();
        let id = Uuid::new_v4();
        store.create_run(id, &goal(), None).unwrap();
        store.set_state(id, RunState::Planning, None).unwrap();
        assert_eq!(store.mark_unfinished_interrupted().unwrap(), 1);
        assert_eq!(store.latest_runs(1).unwrap()[0]["state"], "interrupted");
    }

    #[test]
    fn retention_keeps_latest_runs() {
        let directory = tempdir().unwrap();
        let store = HistoryStore::open(directory.path().join("history.db")).unwrap();
        store.create_run(Uuid::new_v4(), &goal(), None).unwrap();
        store.create_run(Uuid::new_v4(), &goal(), None).unwrap();
        assert_eq!(
            store
                .apply_retention(RetentionPolicy::KeepLatest(1))
                .unwrap(),
            1
        );
        assert_eq!(store.latest_runs(10).unwrap().len(), 1);
    }
}
