use tempfile::tempdir;
use teracode_adapters::{InvocationContext, ProbeReadiness, adapters, run_supervised};
use teracode_core::AutonomyPolicy;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "may consume provider quota; requires TERACODE_LIVE_TESTS=1"]
async fn installed_providers_complete_a_read_only_smoke_prompt() {
    if std::env::var("TERACODE_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let directory = tempdir().unwrap();
    for adapter in adapters() {
        let probe = adapter.probe().await;
        if !probe.installed
            || probe.readiness == ProbeReadiness::Unavailable
            || !probe.capabilities.read_only
        {
            continue;
        }
        let invocation = adapter
            .build_invocation(&InvocationContext {
                prompt: "Reply with exactly: TERACODE_SMOKE_OK. Do not use tools.".into(),
                current_dir: directory.path().to_path_buf(),
                model: None,
                autonomy: AutonomyPolicy::ReadOnly,
                dangerous_confirmed: false,
            })
            .unwrap();
        let output = run_supervised(
            adapter.as_ref(),
            &invocation,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert!(output.success(), "{} smoke run failed", adapter.kind());
    }
}
