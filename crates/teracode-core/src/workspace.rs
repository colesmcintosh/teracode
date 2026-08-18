use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::WorkspacePolicy;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryStatus {
    pub path: PathBuf,
    pub is_git: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    pub changed_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSet {
    pub policy: WorkspacePolicy,
    pub base_commit: Option<String>,
    pub starting_branch: Option<String>,
    pub integration_path: PathBuf,
    pub integration_branch: Option<String>,
    pub agent_paths: HashMap<String, PathBuf>,
    pub baseline_paths: Vec<PathBuf>,
    pub warning: Option<String>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("repository path does not exist or is not a directory: {0}")]
    InvalidRepository(PathBuf),
    #[error("workspace policy {0} requires a Git repository")]
    GitRequired(WorkspacePolicy),
    #[error("Git command failed: git {args}: {stderr}")]
    Git { args: String, stderr: String },
    #[error("cannot create workspace directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("agent worktrees contain overlapping edits: {0}")]
    OverlappingEdits(String),
    #[error("cannot write a binary patch to Git: {0}")]
    PatchIo(#[from] std::io::Error),
    #[error("unknown agent worktree: {0}")]
    UnknownAgent(String),
}

#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    repository: PathBuf,
    worktree_root: PathBuf,
    run_id: Uuid,
}

impl WorkspaceManager {
    pub fn new(repository: PathBuf, worktree_root: PathBuf, run_id: Uuid) -> Self {
        Self {
            repository,
            worktree_root,
            run_id,
        }
    }

    pub fn inspect(repository: &Path) -> Result<RepositoryStatus, WorkspaceError> {
        if !repository.is_dir() {
            return Err(WorkspaceError::InvalidRepository(repository.to_path_buf()));
        }
        let is_git = git_optional(repository, &["rev-parse", "--is-inside-work-tree"])
            .is_some_and(|value| value.trim() == "true");
        if !is_git {
            return Ok(RepositoryStatus {
                path: repository.to_path_buf(),
                is_git: false,
                head: None,
                branch: None,
                dirty: false,
                changed_paths: Vec::new(),
            });
        }
        let head =
            git_optional(repository, &["rev-parse", "HEAD"]).map(|value| value.trim().to_owned());
        let branch = git_optional(repository, &["branch", "--show-current"])
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let status = git(repository, &["status", "--porcelain=v1", "-z"])?;
        let changed_paths = parse_porcelain_paths(status.as_bytes());
        Ok(RepositoryStatus {
            path: repository.to_path_buf(),
            is_git,
            head,
            branch,
            dirty: !changed_paths.is_empty(),
            changed_paths,
        })
    }

    pub fn prepare(
        &self,
        policy: WorkspacePolicy,
        agent_ids: &[String],
    ) -> Result<WorkspaceSet, WorkspaceError> {
        let status = Self::inspect(&self.repository)?;
        if !status.is_git && policy != WorkspacePolicy::SharedWorkspace {
            return Err(WorkspaceError::GitRequired(policy));
        }
        if policy == WorkspacePolicy::SharedWorkspace {
            return Ok(WorkspaceSet {
                policy,
                base_commit: status.head,
                starting_branch: status.branch,
                integration_path: self.repository.clone(),
                integration_branch: None,
                agent_paths: agent_ids
                    .iter()
                    .map(|id| (id.clone(), self.repository.clone()))
                    .collect(),
                baseline_paths: status.changed_paths,
                warning: Some(
                    "Agents share the selected directory; attribution and rollback are limited."
                        .into(),
                ),
            });
        }

        let base = status
            .head
            .clone()
            .ok_or(WorkspaceError::GitRequired(policy))?;
        let run_root = self.worktree_root.join(self.run_id.to_string());
        fs::create_dir_all(&run_root).map_err(|source| WorkspaceError::CreateDirectory {
            path: run_root.clone(),
            source,
        })?;
        let short_id = &self.run_id.simple().to_string()[..12];
        let integration_path = run_root.join("integration");
        let integration_branch = format!("teracode/{short_id}/integration");
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "-b",
                &integration_branch,
                &integration_path.to_string_lossy(),
                &base,
            ],
        )?;

        let mut agent_paths = HashMap::new();
        if policy == WorkspacePolicy::WorktreePerAgent {
            for (index, agent_id) in agent_ids.iter().enumerate() {
                let path = run_root.join(format!("cell-{}", index + 1));
                let branch = format!("teracode/{short_id}/cell-{}", index + 1);
                git(
                    &self.repository,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        &branch,
                        &path.to_string_lossy(),
                        &base,
                    ],
                )?;
                agent_paths.insert(agent_id.clone(), path);
            }
        } else {
            for agent_id in agent_ids {
                agent_paths.insert(agent_id.clone(), integration_path.clone());
            }
        }

        Ok(WorkspaceSet {
            policy,
            base_commit: Some(base),
            starting_branch: status.branch,
            integration_path,
            integration_branch: Some(integration_branch),
            agent_paths,
            baseline_paths: status.changed_paths,
            warning: status.dirty.then(|| {
                "Isolated worktrees use committed HEAD; uncommitted source changes are excluded."
                    .into()
            }),
        })
    }

    pub fn assemble(&self, workspaces: &WorkspaceSet) -> Result<(), WorkspaceError> {
        if workspaces.policy != WorkspacePolicy::WorktreePerAgent {
            return Ok(());
        }
        let base = workspaces
            .base_commit
            .as_deref()
            .ok_or(WorkspaceError::GitRequired(workspaces.policy))?;
        let mut ownership = HashMap::<PathBuf, String>::new();
        let mut patches = Vec::new();
        let mut agents: Vec<_> = workspaces.agent_paths.iter().collect();
        agents.sort_by(|left, right| left.0.cmp(right.0));
        for (agent_id, path) in agents {
            let paths = changed_paths(path, base)?;
            let overlaps: Vec<_> = paths
                .iter()
                .filter_map(|changed| {
                    ownership
                        .get(changed)
                        .map(|owner| format!("{} ({owner}, {agent_id})", changed.display()))
                })
                .collect();
            if !overlaps.is_empty() {
                return Err(WorkspaceError::OverlappingEdits(overlaps.join(", ")));
            }
            for changed in paths {
                ownership.insert(changed, agent_id.clone());
            }
            git(path, &["add", "-N", "--", "."])?;
            let patch = git_bytes(path, &["diff", "--binary", "--full-index", base, "--"])?;
            if !patch.is_empty() {
                patches.push(patch);
            }
        }
        for patch in patches {
            git_with_stdin(
                &workspaces.integration_path,
                &["apply", "--index", "--3way", "-"],
                &patch,
            )?;
        }
        Ok(())
    }

    pub fn stage_integration(&self, workspaces: &WorkspaceSet) -> Result<(), WorkspaceError> {
        if workspaces.policy != WorkspacePolicy::SharedWorkspace {
            git(&workspaces.integration_path, &["add", "--all", "--", "."])?;
        }
        Ok(())
    }

    pub fn integration_diff_summary(
        &self,
        workspaces: &WorkspaceSet,
    ) -> Result<Option<String>, WorkspaceError> {
        if !Self::inspect(&workspaces.integration_path)?.is_git {
            return Ok(None);
        }
        let summary = if workspaces.policy == WorkspacePolicy::SharedWorkspace {
            git(
                &workspaces.integration_path,
                &["diff", "--stat", "HEAD", "--"],
            )?
        } else {
            git(
                &workspaces.integration_path,
                &["diff", "--cached", "--stat", "--"],
            )?
        };
        let summary = summary.trim();
        Ok((!summary.is_empty()).then(|| summary.to_owned()))
    }

    pub fn path_for_agent<'a>(
        &self,
        workspaces: &'a WorkspaceSet,
        agent_id: &str,
    ) -> Result<&'a Path, WorkspaceError> {
        workspaces
            .agent_paths
            .get(agent_id)
            .map(PathBuf::as_path)
            .ok_or_else(|| WorkspaceError::UnknownAgent(agent_id.to_owned()))
    }
}

fn git(repository: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(WorkspaceError::PatchIo)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(WorkspaceError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn git_optional(repository: &Path, args: &[&str]) -> Option<String> {
    git(repository, args).ok()
}

fn git_bytes(repository: &Path, args: &[&str]) -> Result<Vec<u8>, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(WorkspaceError::PatchIo)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(WorkspaceError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn git_with_stdin(repository: &Path, args: &[&str], input: &[u8]) -> Result<(), WorkspaceError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin was configured")
        .write_all(input)?;
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn changed_paths(repository: &Path, base: &str) -> Result<HashSet<PathBuf>, WorkspaceError> {
    let tracked = git(repository, &["diff", "--name-only", "-z", base, "--"])?;
    let untracked = git(
        repository,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    Ok(tracked
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn parse_porcelain_paths(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| entry.get(3..))
        .filter(|entry| !entry.is_empty())
        .map(|entry| PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn init_repository() -> tempfile::TempDir {
        let directory = tempdir().unwrap();
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        fs::write(directory.path().join("base.txt"), "base\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(directory.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "base"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        directory
    }

    #[test]
    fn detects_dirty_repository() {
        let repository = init_repository();
        fs::write(repository.path().join("new.txt"), "new\n").unwrap();
        let status = WorkspaceManager::inspect(repository.path()).unwrap();
        assert!(status.is_git);
        assert!(status.dirty);
        assert_eq!(status.branch.as_deref(), Some("main"));
    }

    #[test]
    fn assembles_non_overlapping_binary_safe_patches_without_switching_branch() {
        let repository = init_repository();
        let worktree_root = tempdir().unwrap();
        let manager = WorkspaceManager::new(
            repository.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            Uuid::new_v4(),
        );
        let workspaces = manager
            .prepare(
                WorkspacePolicy::WorktreePerAgent,
                &["one".into(), "two".into()],
            )
            .unwrap();
        fs::write(
            workspaces.agent_paths["one"].join("one.bin"),
            [0, 1, 2, 0xff],
        )
        .unwrap();
        fs::write(workspaces.agent_paths["two"].join("two.txt"), "two\n").unwrap();
        manager.assemble(&workspaces).unwrap();

        assert_eq!(
            git(repository.path(), &["branch", "--show-current"])
                .unwrap()
                .trim(),
            "main"
        );
        assert!(workspaces.integration_path.join("one.bin").exists());
        assert!(workspaces.integration_path.join("two.txt").exists());
        let staged = git(
            &workspaces.integration_path,
            &["diff", "--cached", "--name-only"],
        )
        .unwrap();
        assert!(staged.contains("one.bin"));
        assert!(staged.contains("two.txt"));
        assert!(
            manager
                .integration_diff_summary(&workspaces)
                .unwrap()
                .unwrap()
                .contains("2 files changed")
        );
    }

    #[test]
    fn rejects_overlapping_worker_edits() {
        let repository = init_repository();
        let worktree_root = tempdir().unwrap();
        let manager = WorkspaceManager::new(
            repository.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            Uuid::new_v4(),
        );
        let workspaces = manager
            .prepare(
                WorkspacePolicy::WorktreePerAgent,
                &["one".into(), "two".into()],
            )
            .unwrap();
        fs::write(workspaces.agent_paths["one"].join("base.txt"), "one\n").unwrap();
        fs::write(workspaces.agent_paths["two"].join("base.txt"), "two\n").unwrap();

        assert!(matches!(
            manager.assemble(&workspaces),
            Err(WorkspaceError::OverlappingEdits(_))
        ));
    }

    #[test]
    fn read_only_cells_inspect_the_committed_integration_worktree() {
        let repository = init_repository();
        fs::write(repository.path().join("uncommitted.txt"), "excluded\n").unwrap();
        let worktree_root = tempdir().unwrap();
        let manager = WorkspaceManager::new(
            repository.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            Uuid::new_v4(),
        );
        let workspaces = manager
            .prepare(WorkspacePolicy::ReadOnlyThenExecutor, &["reader".into()])
            .unwrap();

        assert_eq!(
            workspaces.agent_paths["reader"],
            workspaces.integration_path
        );
        assert!(!workspaces.integration_path.join("uncommitted.txt").exists());
    }
}
