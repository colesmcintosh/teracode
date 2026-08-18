use std::{io, process::ExitStatus, time::Duration};

use serde_json::Value;
use teracode_core::AgentEvent;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use crate::{AgentAdapter, Invocation};

const MAX_LINE_BYTES: usize = 1024 * 1024;
const INTERRUPT_GRACE: Duration = Duration::from_millis(1_500);
const TERMINATE_GRACE: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct ProcessEvent {
    pub stream: StreamKind,
    pub event: AgentEvent,
}

#[derive(Debug)]
pub struct RunOutput {
    pub status: ExitStatus,
    pub cancelled: bool,
    pub events: Vec<ProcessEvent>,
    pub stderr: Vec<String>,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.status.success() && !self.cancelled
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("cannot start {program}: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("cannot wait for agent process: {0}")]
    Wait(#[from] io::Error),
    #[error("agent process did not expose its configured {0} pipe")]
    MissingPipe(&'static str),
}

#[derive(Debug)]
struct RawLine {
    stream: StreamKind,
    bytes: Vec<u8>,
    truncated: bool,
}

pub async fn run_supervised(
    adapter: &dyn AgentAdapter,
    invocation: &Invocation,
    cancellation: CancellationToken,
    live_events: Option<mpsc::UnboundedSender<ProcessEvent>>,
) -> Result<RunOutput, SupervisorError> {
    let redactor = SecretRedactor::from_environment();
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .current_dir(&invocation.current_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|source| SupervisorError::Spawn {
        program: invocation.program.clone(),
        source,
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or(SupervisorError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(SupervisorError::MissingPipe("stderr"))?;
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    let stdout_reader = tokio::spawn(read_stream(stdout, StreamKind::Stdout, raw_tx.clone()));
    let stderr_reader = tokio::spawn(read_stream(stderr, StreamKind::Stderr, raw_tx.clone()));
    drop(raw_tx);

    let mut events = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut cancelled = false;
    let status = loop {
        tokio::select! {
            result = child.wait() => break result?,
            () = cancellation.cancelled() => {
                cancelled = true;
                break terminate_process_group(&mut child).await?;
            }
            line = raw_rx.recv() => {
                if let Some(line) = line {
                    handle_line(adapter, &redactor, &line, &mut events, &mut stderr_lines, live_events.as_ref());
                }
            }
        }
    };

    let drain = async {
        while let Some(line) = raw_rx.recv().await {
            handle_line(
                adapter,
                &redactor,
                &line,
                &mut events,
                &mut stderr_lines,
                live_events.as_ref(),
            );
        }
    };
    if tokio::time::timeout(Duration::from_secs(1), drain)
        .await
        .is_err()
    {
        stdout_reader.abort();
        stderr_reader.abort();
    }

    if !status.success() && !cancelled {
        let process_event = ProcessEvent {
            stream: StreamKind::Stderr,
            event: AgentEvent::Failed {
                message: format!(
                    "{} exited with {}",
                    adapter.kind(),
                    status
                        .code()
                        .map_or_else(|| "a signal".into(), |code| format!("status {code}"))
                ),
                retryable: true,
            },
        };
        if let Some(sender) = &live_events {
            let _ = sender.send(process_event.clone());
        }
        events.push(process_event);
    }

    Ok(RunOutput {
        status,
        cancelled,
        events,
        stderr: stderr_lines,
    })
}

async fn read_stream<R: AsyncRead + Unpin>(
    stream: R,
    kind: StreamKind,
    sender: mpsc::UnboundedSender<RawLine>,
) {
    let mut reader = BufReader::new(stream);
    while let Ok(Some((bytes, truncated))) = read_bounded_line(&mut reader).await {
        if sender
            .send(RawLine {
                stream: kind,
                bytes,
                truncated,
            })
            .is_err()
        {
            break;
        }
    }
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if output.is_empty() && !truncated {
                Ok(None)
            } else {
                Ok(Some((output, truncated)))
            };
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |position| position + 1);
        let remaining = MAX_LINE_BYTES.saturating_sub(output.len());
        let copied = remaining.min(consumed);
        output.extend_from_slice(&buffer[..copied]);
        if copied < consumed {
            truncated = true;
        }
        reader.consume(consumed);
        if newline.is_some() {
            while output.last() == Some(&b'\n') || output.last() == Some(&b'\r') {
                output.pop();
            }
            return Ok(Some((output, truncated)));
        }
    }
}

fn handle_line(
    adapter: &dyn AgentAdapter,
    redactor: &SecretRedactor,
    line: &RawLine,
    events: &mut Vec<ProcessEvent>,
    stderr_lines: &mut Vec<String>,
    live_events: Option<&mpsc::UnboundedSender<ProcessEvent>>,
) {
    let text = redactor.redact(&String::from_utf8_lossy(&line.bytes));
    let event = if line.truncated {
        AgentEvent::Warning {
            message: format!(
                "{} line exceeded {} bytes and was truncated",
                match line.stream {
                    StreamKind::Stdout => "stdout",
                    StreamKind::Stderr => "stderr",
                },
                MAX_LINE_BYTES
            ),
        }
    } else if line.stream == StreamKind::Stdout {
        adapter.parse_event(&text)
    } else {
        let redacted = redact_text(&text);
        stderr_lines.push(redacted.clone());
        AgentEvent::Diagnostic {
            source: format!("{} stderr", adapter.kind()),
            raw: redacted,
        }
    };
    let process_event = ProcessEvent {
        stream: line.stream,
        event,
    };
    if let Some(sender) = live_events {
        let _ = sender.send(process_event.clone());
    }
    events.push(process_event);
}

#[derive(Debug, Default)]
struct SecretRedactor {
    values: Vec<String>,
}

impl SecretRedactor {
    fn from_environment() -> Self {
        let mut values = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.to_string_lossy();
                let value = value.to_string_lossy();
                (sensitive_key(&key) && value.len() >= 8).then(|| value.into_owned())
            })
            .collect::<Vec<_>>();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self { values }
    }

    fn redact(&self, input: &str) -> String {
        let mut output = input.to_owned();
        for value in &self.values {
            output = output.replace(value, "[REDACTED]");
        }
        output
    }
}

#[cfg(unix)]
async fn terminate_process_group(child: &mut Child) -> io::Result<ExitStatus> {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let Some(process_id) = child.id() else {
        return child.wait().await;
    };
    let group = Pid::from_raw(-i32::try_from(process_id).unwrap_or(i32::MAX));
    let _ = kill(group, Signal::SIGINT);
    if let Ok(result) = tokio::time::timeout(INTERRUPT_GRACE, child.wait()).await {
        return result;
    }
    let _ = kill(group, Signal::SIGTERM);
    if let Ok(result) = tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
        return result;
    }
    let _ = kill(group, Signal::SIGKILL);
    child.wait().await
}

#[cfg(not(unix))]
async fn terminate_process_group(child: &mut Child) -> io::Result<ExitStatus> {
    child.kill().await?;
    child.wait().await
}

pub(crate) fn redact_json(value: &Value) -> String {
    fn visit(value: &mut Value) {
        match value {
            Value::Object(values) => {
                for (key, value) in values {
                    if sensitive_key(key) {
                        *value = Value::String("[REDACTED]".into());
                    } else {
                        visit(value);
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(visit),
            _ => {}
        }
    }
    let mut redacted = value.clone();
    visit(&mut redacted);
    redacted.to_string()
}

pub(crate) fn redact_text(input: &str) -> String {
    let mut output = input.to_owned();
    let lower = output.to_ascii_lowercase();
    if let Some(index) = lower.find("bearer ") {
        let start = index + "bearer ".len();
        let end = output[start..]
            .find(char::is_whitespace)
            .map_or(output.len(), |offset| start + offset);
        output.replace_range(start..end, "[REDACTED]");
    }
    for key in [
        "api_key",
        "apikey",
        "access_token",
        "authorization",
        "password",
        "secret",
        "token",
    ] {
        let mut search_from = 0;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(relative) = lower[search_from..].find(key) else {
                break;
            };
            let key_end = search_from + relative + key.len();
            let Some(separator_offset) = output[key_end..].find([':', '=']) else {
                break;
            };
            if separator_offset > 3 {
                search_from = key_end;
                continue;
            }
            let mut value_start = key_end + separator_offset + 1;
            while output
                .as_bytes()
                .get(value_start)
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'\'' | b'"'))
            {
                value_start += 1;
            }
            let value_end = output[value_start..]
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, ',' | '"' | '\'')
                })
                .map_or(output.len(), |offset| value_start + offset);
            if value_start < value_end {
                output.replace_range(value_start..value_end, "[REDACTED]");
                search_from = value_start + "[REDACTED]".len();
            } else {
                search_from = key_end;
            }
        }
    }
    output
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "apikey",
        "accesstoken",
        "refreshtoken",
        "token",
        "authorization",
        "password",
        "secret",
        "credential",
        "privatekey",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_text_and_nested_json_secrets() {
        assert_eq!(
            redact_text("API_KEY=example-private-value Bearer abc.def"),
            "API_KEY=[REDACTED] Bearer [REDACTED]"
        );
        let value = serde_json::json!({"nested": {"access_token": "secret-value"}, "ok": 1});
        let redacted = redact_json(&value);
        assert!(!redacted.contains("secret-value"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_exact_secret_values_even_without_a_key_label() {
        let redactor = SecretRedactor {
            values: vec!["provider-secret-value".into()],
        };
        assert_eq!(
            redactor.redact("provider said provider-secret-value"),
            "provider said [REDACTED]"
        );
    }
}
