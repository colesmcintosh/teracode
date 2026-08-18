use std::{path::Path, time::Instant};

use thiserror::Error;
use tokio::{process::Command, time::Duration};

use crate::{AcceptanceCommand, CheckResult};

const MAX_CHECK_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("cannot start acceptance command {command}: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
}

pub async fn run_acceptance_checks(
    working_directory: &Path,
    commands: &[AcceptanceCommand],
) -> Result<Vec<CheckResult>, ValidationError> {
    let mut results = Vec::with_capacity(commands.len());
    for command in commands {
        let started = Instant::now();
        let child = Command::new(&command.executable)
            .args(&command.args)
            .current_dir(working_directory)
            .kill_on_drop(true)
            .output();
        let result = match tokio::time::timeout(Duration::from_secs(command.timeout_secs), child)
            .await
        {
            Ok(Ok(output)) => {
                let mut combined = output.stdout;
                if !combined.is_empty() && !output.stderr.is_empty() {
                    combined.push(b'\n');
                }
                combined.extend_from_slice(&output.stderr);
                combined.truncate(MAX_CHECK_OUTPUT_BYTES);
                CheckResult {
                    command: command.clone(),
                    passed: output.status.success(),
                    exit_code: output.status.code(),
                    output: String::from_utf8_lossy(&combined).into_owned(),
                    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                }
            }
            Ok(Err(source)) => {
                return Err(ValidationError::Spawn {
                    command: command.executable.clone(),
                    source,
                });
            }
            Err(_) => CheckResult {
                command: command.clone(),
                passed: false,
                exit_code: None,
                output: format!("timed out after {} seconds", command.timeout_secs),
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
        };
        let should_stop = result.command.required && !result.passed;
        results.push(result);
        if should_stop {
            break;
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn required_failure_stops_the_line() {
        let commands = vec![
            AcceptanceCommand {
                executable: "false".into(),
                args: Vec::new(),
                timeout_secs: 1,
                required: true,
            },
            AcceptanceCommand {
                executable: "true".into(),
                args: Vec::new(),
                timeout_secs: 1,
                required: true,
            },
        ];
        let results = run_acceptance_checks(Path::new("."), &commands)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
    }
}
