#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Instant};

use tempfile::tempdir;
use teracode_adapters::{Invocation, adapters, run_supervised};
use teracode_core::{AdapterKind, AgentEvent};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

fn script(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn codex_adapter() -> Box<dyn teracode_adapters::AgentAdapter> {
    adapters()
        .into_iter()
        .find(|adapter| adapter.kind() == AdapterKind::Codex)
        .unwrap()
}

#[tokio::test]
async fn separates_stderr_and_parses_stdout_incrementally() {
    let directory = tempdir().unwrap();
    let executable = directory.path().join("fake-agent");
    script(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"fake\"}'\nprintf '%s\\n' '{\"type\":\"turn.completed\",\"summary\":\"done\"}'\nprintf '%s\\n' 'API_KEY=private' >&2\n",
    );
    let output = run_supervised(
        codex_adapter().as_ref(),
        &Invocation {
            program: executable.to_string_lossy().into_owned(),
            args: Vec::new(),
            current_dir: directory.path().to_path_buf(),
        },
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    assert!(output.success());
    assert!(
        output
            .events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::Completed { .. }))
    );
    assert_eq!(output.stderr, ["API_KEY=[REDACTED]"]);
}

#[tokio::test]
async fn cancellation_escalates_and_reaps_the_process_group() {
    let directory = tempdir().unwrap();
    let executable = directory.path().join("stubborn-agent");
    script(
        &executable,
        "#!/bin/sh\ntrap '' INT\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
    );
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    let output = run_supervised(
        codex_adapter().as_ref(),
        &Invocation {
            program: executable.to_string_lossy().into_owned(),
            args: Vec::new(),
            current_dir: directory.path().to_path_buf(),
        },
        cancellation,
        None,
    )
    .await
    .unwrap();
    assert!(output.cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
}
