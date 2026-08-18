use std::{path::PathBuf, process::Stdio, time::Duration};

use async_trait::async_trait;
use serde_json::Value;
use teracode_core::{AdapterCapabilities, AdapterKind, AgentEvent, AgentState, AutonomyPolicy};
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct InvocationContext {
    pub prompt: String,
    pub current_dir: PathBuf,
    pub model: Option<String>,
    pub autonomy: AutonomyPolicy,
    pub dangerous_confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeReadiness {
    Ready,
    Unknown,
    Unavailable,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProbeResult {
    pub adapter: AdapterKind,
    pub executable: String,
    pub installed: bool,
    pub version: Option<String>,
    pub readiness: ProbeReadiness,
    pub capabilities: AdapterCapabilities,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdapterError {
    #[error("{adapter} does not support the {policy} autonomy policy")]
    UnsupportedPolicy {
        adapter: AdapterKind,
        policy: AutonomyPolicy,
    },
    #[error("{adapter} requires explicit confirmation for {policy}")]
    DangerousConfirmationRequired {
        adapter: AdapterKind,
        policy: AutonomyPolicy,
    },
    #[error("{0} does not support session resume")]
    ResumeUnsupported(AdapterKind),
}

#[async_trait]
pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AdapterKind;
    fn executable(&self) -> &'static str;
    fn capabilities(&self, version: Option<&str>) -> AdapterCapabilities;
    async fn probe(&self) -> ProbeResult;
    fn build_invocation(&self, context: &InvocationContext) -> Result<Invocation, AdapterError>;
    fn resume_invocation(
        &self,
        context: &InvocationContext,
        session_id: &str,
    ) -> Result<Invocation, AdapterError>;
    fn parse_event(&self, line: &str) -> AgentEvent {
        parse_normalized_event(self.kind(), line)
    }
    fn map_policy(
        &self,
        policy: AutonomyPolicy,
        dangerous_confirmed: bool,
    ) -> Result<Vec<String>, AdapterError>;
}

#[derive(Debug)]
struct BuiltInAdapter {
    kind: AdapterKind,
}

pub fn adapters() -> Vec<Box<dyn AgentAdapter>> {
    AdapterKind::ALL
        .into_iter()
        .map(|kind| Box::new(BuiltInAdapter { kind }) as Box<dyn AgentAdapter>)
        .collect()
}

#[async_trait]
impl AgentAdapter for BuiltInAdapter {
    fn kind(&self) -> AdapterKind {
        self.kind
    }

    fn executable(&self) -> &'static str {
        self.kind.executable()
    }

    fn capabilities(&self, _version: Option<&str>) -> AdapterCapabilities {
        match self.kind {
            AdapterKind::Claude | AdapterKind::Codex | AdapterKind::Grok | AdapterKind::Droid => {
                AdapterCapabilities {
                    structured_output: true,
                    resume: true,
                    model_selection: true,
                    read_only: true,
                    workspace_write: true,
                    full_access: true,
                }
            }
            AdapterKind::OpenCode => AdapterCapabilities {
                structured_output: true,
                resume: true,
                model_selection: true,
                read_only: true,
                workspace_write: true,
                full_access: false,
            },
            AdapterKind::Cursor => AdapterCapabilities {
                structured_output: true,
                resume: true,
                model_selection: true,
                read_only: false,
                workspace_write: true,
                full_access: true,
            },
        }
    }

    async fn probe(&self) -> ProbeResult {
        let mut command = Command::new(self.executable());
        match self.kind {
            AdapterKind::Grok => {
                command.arg("version");
            }
            _ => {
                command.arg("--version");
            }
        }
        command.stdin(Stdio::null());
        let result = tokio::time::timeout(Duration::from_secs(3), command.output()).await;
        match result {
            Ok(Ok(output)) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let version = if stdout.trim().is_empty() {
                    stderr.trim().to_owned()
                } else {
                    stdout.trim().to_owned()
                };
                let version = (!version.is_empty()).then_some(version);
                if let Some(diagnostic) = version
                    .as_deref()
                    .and_then(|version| executable_identity_error(self.kind, version))
                {
                    return ProbeResult {
                        adapter: self.kind,
                        executable: self.executable().into(),
                        installed: false,
                        version,
                        readiness: ProbeReadiness::Unavailable,
                        capabilities: self.capabilities(None),
                        diagnostic: Some(diagnostic),
                    };
                }
                ProbeResult {
                    adapter: self.kind,
                    executable: self.executable().into(),
                    installed: true,
                    version: version.clone(),
                    readiness: ProbeReadiness::Unknown,
                    capabilities: self.capabilities(version.as_deref()),
                    diagnostic: Some(
                        "Executable responded; authentication is checked only when a run starts."
                            .into(),
                    ),
                }
            }
            Ok(Ok(output)) => ProbeResult {
                adapter: self.kind,
                executable: self.executable().into(),
                installed: true,
                version: None,
                readiness: ProbeReadiness::Unavailable,
                capabilities: self.capabilities(None),
                diagnostic: Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
            },
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => ProbeResult {
                adapter: self.kind,
                executable: self.executable().into(),
                installed: false,
                version: None,
                readiness: ProbeReadiness::Unavailable,
                capabilities: self.capabilities(None),
                diagnostic: Some("Executable was not found on PATH.".into()),
            },
            Ok(Err(error)) => ProbeResult {
                adapter: self.kind,
                executable: self.executable().into(),
                installed: true,
                version: None,
                readiness: ProbeReadiness::Unavailable,
                capabilities: self.capabilities(None),
                diagnostic: Some(error.to_string()),
            },
            Err(_) => ProbeResult {
                adapter: self.kind,
                executable: self.executable().into(),
                installed: true,
                version: None,
                readiness: ProbeReadiness::Unavailable,
                capabilities: self.capabilities(None),
                diagnostic: Some("Version probe timed out after 3 seconds.".into()),
            },
        }
    }

    fn build_invocation(&self, context: &InvocationContext) -> Result<Invocation, AdapterError> {
        let mut args = match self.kind {
            AdapterKind::Claude => vec![
                "--print".into(),
                "--verbose".into(),
                "--output-format".into(),
                "stream-json".into(),
            ],
            AdapterKind::Codex => vec![
                "exec".into(),
                "--json".into(),
                "--color".into(),
                "never".into(),
            ],
            AdapterKind::OpenCode => {
                vec!["run".into(), "--format".into(), "json".into()]
            }
            AdapterKind::Cursor => vec![
                "--print".into(),
                "--output-format".into(),
                "stream-json".into(),
            ],
            AdapterKind::Grok => vec![
                "--no-auto-update".into(),
                "--output-format".into(),
                "streaming-json".into(),
                "--no-subagents".into(),
            ],
            AdapterKind::Droid => vec![
                "exec".into(),
                "--output-format".into(),
                "stream-json".into(),
            ],
        };
        args.extend(self.map_policy(context.autonomy, context.dangerous_confirmed)?);
        if let Some(model) = &context.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if self.kind == AdapterKind::Grok {
            args.push("--single".into());
        }
        args.push(context.prompt.clone());
        Ok(Invocation {
            program: self.executable().into(),
            args,
            current_dir: context.current_dir.clone(),
        })
    }

    fn resume_invocation(
        &self,
        context: &InvocationContext,
        session_id: &str,
    ) -> Result<Invocation, AdapterError> {
        if !self.capabilities(None).resume {
            return Err(AdapterError::ResumeUnsupported(self.kind));
        }
        let mut invocation = self.build_invocation(context)?;
        let prompt = invocation
            .args
            .pop()
            .expect("new invocations end in a prompt");
        match self.kind {
            AdapterKind::Claude | AdapterKind::Cursor => {
                invocation.args.push("--resume".into());
                invocation.args.push(session_id.into());
            }
            AdapterKind::Codex => {
                invocation.args.push("resume".into());
                invocation.args.push(session_id.into());
            }
            AdapterKind::OpenCode => {
                invocation.args.push("--session".into());
                invocation.args.push(session_id.into());
            }
            AdapterKind::Grok => {
                if invocation.args.last().is_some_and(|arg| arg == "--single") {
                    invocation.args.pop();
                }
                invocation.args.push("--resume".into());
                invocation.args.push(session_id.into());
                invocation.args.push("--single".into());
            }
            AdapterKind::Droid => {
                invocation.args.push("--session-id".into());
                invocation.args.push(session_id.into());
            }
        }
        invocation.args.push(prompt);
        Ok(invocation)
    }

    fn map_policy(
        &self,
        policy: AutonomyPolicy,
        dangerous_confirmed: bool,
    ) -> Result<Vec<String>, AdapterError> {
        if !self.capabilities(None).supports(policy) {
            return Err(AdapterError::UnsupportedPolicy {
                adapter: self.kind,
                policy,
            });
        }
        if policy.is_dangerous() && !dangerous_confirmed {
            return Err(AdapterError::DangerousConfirmationRequired {
                adapter: self.kind,
                policy,
            });
        }
        let flags = match (self.kind, policy) {
            (AdapterKind::Claude, AutonomyPolicy::ReadOnly) => {
                vec!["--permission-mode".into(), "plan".into()]
            }
            (AdapterKind::Claude, AutonomyPolicy::WorkspaceWrite) => {
                vec!["--permission-mode".into(), "acceptEdits".into()]
            }
            (AdapterKind::Claude, AutonomyPolicy::FullAccess) => {
                vec!["--dangerously-skip-permissions".into()]
            }
            (AdapterKind::Codex, AutonomyPolicy::ReadOnly) => vec![
                "--sandbox".into(),
                "read-only".into(),
                "--ask-for-approval".into(),
                "never".into(),
            ],
            (AdapterKind::Codex, AutonomyPolicy::WorkspaceWrite) => vec![
                "--sandbox".into(),
                "workspace-write".into(),
                "--ask-for-approval".into(),
                "never".into(),
            ],
            (AdapterKind::Codex, AutonomyPolicy::FullAccess) => {
                vec!["--dangerously-bypass-approvals-and-sandbox".into()]
            }
            (AdapterKind::OpenCode, AutonomyPolicy::ReadOnly) => {
                vec!["--agent".into(), "plan".into()]
            }
            (AdapterKind::OpenCode, AutonomyPolicy::WorkspaceWrite) => vec!["--auto".into()],
            (AdapterKind::Cursor, AutonomyPolicy::WorkspaceWrite)
            | (AdapterKind::Droid, AutonomyPolicy::ReadOnly) => Vec::new(),
            (AdapterKind::Cursor, AutonomyPolicy::FullAccess) => vec!["--force".into()],
            (AdapterKind::Grok, AutonomyPolicy::ReadOnly) => vec![
                "--permission-mode".into(),
                "dontAsk".into(),
                "--deny".into(),
                "Edit".into(),
                "--deny".into(),
                "Bash".into(),
                "--sandbox".into(),
                "strict".into(),
            ],
            (AdapterKind::Grok, AutonomyPolicy::WorkspaceWrite) => {
                vec![
                    "--permission-mode".into(),
                    "acceptEdits".into(),
                    "--sandbox".into(),
                    "workspace".into(),
                ]
            }
            (AdapterKind::Grok, AutonomyPolicy::FullAccess) => {
                vec!["--always-approve".into()]
            }
            (AdapterKind::Droid, AutonomyPolicy::WorkspaceWrite) => {
                vec!["--auto".into(), "medium".into()]
            }
            (AdapterKind::Droid, AutonomyPolicy::FullAccess) => {
                vec!["--skip-permissions-unsafe".into()]
            }
            _ => {
                return Err(AdapterError::UnsupportedPolicy {
                    adapter: self.kind,
                    policy,
                });
            }
        };
        Ok(flags)
    }
}

fn executable_identity_error(adapter: AdapterKind, version: &str) -> Option<String> {
    let version = version.to_ascii_lowercase();
    let conflicting_product = match adapter {
        AdapterKind::Cursor if version.contains("grok") => Some("Grok Build"),
        AdapterKind::Cursor if version.contains("claude") => Some("Claude Code"),
        AdapterKind::Cursor if version.contains("codex") => Some("Codex"),
        AdapterKind::Cursor if version.contains("opencode") => Some("OpenCode"),
        AdapterKind::Cursor if version.contains("droid") => Some("Factory Droid"),
        _ => None,
    }?;
    Some(format!(
        "{} resolved to {conflicting_product}, not {}.",
        adapter.executable(),
        adapter
    ))
}

pub fn parse_normalized_event(adapter: AdapterKind, line: &str) -> AgentEvent {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return AgentEvent::Diagnostic {
            source: adapter.to_string(),
            raw: crate::process::redact_text(line),
        };
    };
    let event_type = string_at(&value, &["type"]).unwrap_or_default();
    let subtype = string_at(&value, &["subtype"]).unwrap_or_default();
    if matches!(event_type.as_str(), "error" | "turn.failed")
        || subtype.contains("error")
        || value.get("is_error").and_then(Value::as_bool) == Some(true)
    {
        return AgentEvent::Failed {
            message: first_string(&value, &[&["error", "message"], &["message"], &["result"]])
                .unwrap_or_else(|| "provider reported a failure".into()),
            retryable: false,
        };
    }
    if matches!(
        event_type.as_str(),
        "result" | "turn.completed" | "complete" | "completed"
    ) {
        return AgentEvent::Completed {
            session_id: first_string(
                &value,
                &[
                    &["session_id"],
                    &["sessionId"],
                    &["thread_id"],
                    &["threadId"],
                ],
            ),
            summary: first_string(&value, &[&["result"], &["summary"], &["message"]]),
        };
    }
    if matches!(
        event_type.as_str(),
        "system" | "thread.started" | "turn.started"
    ) {
        return AgentEvent::Lifecycle {
            state: if event_type == "turn.started" {
                AgentState::Working
            } else {
                AgentState::Starting
            },
            message: first_string(&value, &[&["message"], &["session_id"], &["thread_id"]]),
        };
    }
    if event_type.contains("warning") || subtype.contains("warning") {
        return AgentEvent::Warning {
            message: first_string(&value, &[&["message"], &["warning"], &["detail"]])
                .unwrap_or_else(|| "provider emitted a warning".into()),
        };
    }
    if event_type.contains("approval") || event_type.contains("permission_request") {
        return AgentEvent::Approval {
            prompt: first_string(&value, &[&["prompt"], &["message"], &["detail"]])
                .unwrap_or_else(|| "provider requested approval".into()),
        };
    }
    if event_type.contains("file")
        && let Some(path) = first_string(
            &value,
            &[&["path"], &["file_path"], &["filePath"], &["item", "path"]],
        )
    {
        return AgentEvent::FileActivity {
            path: path.into(),
            action: first_string(&value, &[&["action"], &["operation"]])
                .unwrap_or_else(|| event_type.clone()),
        };
    }
    if event_type.contains("tool")
        || value.get("tool_name").is_some()
        || value.get("toolName").is_some()
    {
        return AgentEvent::ToolActivity {
            tool: first_string(
                &value,
                &[&["tool_name"], &["toolName"], &["name"], &["item", "type"]],
            )
            .unwrap_or_else(|| "tool".into()),
            detail: first_string(&value, &[&["message"], &["input"], &["item", "command"]]),
        };
    }
    if event_type.contains("usage") || value.get("usage").is_some() {
        let usage = value.get("usage").unwrap_or(&value);
        return AgentEvent::Usage {
            input_tokens: number_at(usage, &["input_tokens"])
                .or_else(|| number_at(usage, &["inputTokens"])),
            output_tokens: number_at(usage, &["output_tokens"])
                .or_else(|| number_at(usage, &["outputTokens"])),
            cost_usd: usage
                .get("cost_usd")
                .or_else(|| usage.get("costUsd"))
                .and_then(Value::as_f64),
        };
    }
    if let Some(text) = first_string(
        &value,
        &[
            &["text"],
            &["content"],
            &["message", "content"],
            &["message", "text"],
            &["item", "text"],
            &["item", "content"],
            &["delta", "text"],
            &["result"],
        ],
    ) {
        return AgentEvent::AssistantText { text };
    }
    AgentEvent::Diagnostic {
        source: adapter.to_string(),
        raw: crate::process::redact_json(&value),
    }
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    match current {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => values
            .iter()
            .find_map(|value| value.get("text").and_then(Value::as_str).map(str::to_owned)),
        _ => None,
    }
}

fn first_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| string_at(value, path))
}

fn number_at(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(kind: AdapterKind) -> Box<dyn AgentAdapter> {
        adapters()
            .into_iter()
            .find(|adapter| adapter.kind() == kind)
            .unwrap()
    }

    #[test]
    fn codex_uses_stable_json_exec_contract() {
        let invocation = adapter(AdapterKind::Codex)
            .build_invocation(&InvocationContext {
                prompt: "fix it; $(do not execute)".into(),
                current_dir: PathBuf::from("/repo"),
                model: None,
                autonomy: AutonomyPolicy::WorkspaceWrite,
                dangerous_confirmed: false,
            })
            .unwrap();
        assert_eq!(invocation.program, "codex");
        assert_eq!(invocation.args[0..2], ["exec", "--json"]);
        assert_eq!(invocation.args.last().unwrap(), "fix it; $(do not execute)");
    }

    #[test]
    fn dangerous_flags_need_a_second_confirmation() {
        let error = adapter(AdapterKind::Claude)
            .map_policy(AutonomyPolicy::FullAccess, false)
            .unwrap_err();
        assert!(matches!(
            error,
            AdapterError::DangerousConfirmationRequired { .. }
        ));
    }

    #[test]
    fn unknown_json_is_a_diagnostic_not_an_error() {
        let event = parse_normalized_event(AdapterKind::Codex, r#"{"future":"field"}"#);
        assert!(matches!(event, AgentEvent::Diagnostic { .. }));
    }

    #[test]
    fn normalizes_file_and_approval_events() {
        let file = parse_normalized_event(
            AdapterKind::Codex,
            r#"{"type":"file.changed","path":"src/main.rs","action":"write"}"#,
        );
        assert!(matches!(file, AgentEvent::FileActivity { .. }));
        let approval = parse_normalized_event(
            AdapterKind::Claude,
            r#"{"type":"approval_request","prompt":"Allow command?"}"#,
        );
        assert!(matches!(approval, AgentEvent::Approval { .. }));
    }

    #[test]
    fn rejects_a_conflicting_cursor_executable() {
        let error = executable_identity_error(AdapterKind::Cursor, "grok 0.2.16").unwrap();
        assert!(error.contains("not Cursor"));
        assert!(executable_identity_error(AdapterKind::Cursor, "cursor-agent 1.0").is_none());
    }
}
