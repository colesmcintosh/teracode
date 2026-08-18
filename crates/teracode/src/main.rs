mod runner;
mod tui;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Serialize;
use teracode_adapters::{ProbeResult, adapters};
use teracode_core::{HistoryStore, RepositoryStatus, SkillIndex, TeraCodeConfig, WorkspaceManager};
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "teracode",
    version,
    about = "A local factory for building software-development factories"
)]
struct Cli {
    /// Repository or workspace to orchestrate.
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<PathBuf>,

    /// Force ASCII rail characters for limited terminals.
    #[arg(long, global = true)]
    ascii: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect Git and supported coding-agent CLIs without invoking a model.
    Doctor {
        /// Emit one machine-readable JSON document.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    repository: RepositoryStatus,
    adapters: Vec<ProbeResult>,
    notes: Vec<&'static str>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    let cli = Cli::parse();
    let repository = cli.repo.unwrap_or(std::env::current_dir()?);
    let repository = repository.canonicalize().unwrap_or(repository);
    let repository_status = WorkspaceManager::inspect(&repository)?;
    let probes = probe_all().await;

    if let Some(Command::Doctor { json }) = cli.command {
        let report = DoctorReport {
            repository: repository_status,
            adapters: probes,
            notes: vec![
                "Version probes never invoke a paid model.",
                "Authentication remains provider-managed and is verified when a run starts.",
                "TeraCode has no telemetry or remote history synchronization.",
            ],
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_doctor(&report);
        }
        return Ok(());
    }

    let history = HistoryStore::open_default()?;
    history.mark_unfinished_interrupted()?;
    let (config, _) = TeraCodeConfig::load_default()?;
    history.apply_retention(config.retention)?;
    let user_skill_locations = std::env::var_os("HOME").map_or_else(Vec::new, |home| {
        let home = PathBuf::from(home);
        vec![
            home.join(".agents/skills"),
            home.join(".codex/skills"),
            home.join(".claude/skills"),
            home.join(".grok/skills"),
            home.join(".factory/skills"),
            home.join(".config/opencode/skills"),
        ]
    });
    let skills = SkillIndex::discover(&repository, &user_skill_locations);
    let mut app = tui::App::new(
        repository_status,
        probes,
        skills,
        history,
        config,
        cli.ascii,
    );
    tui::run(&mut app)?;
    Ok(())
}

async fn probe_all() -> Vec<ProbeResult> {
    let mut joins = JoinSet::new();
    for adapter in adapters() {
        joins.spawn(async move { adapter.probe().await });
    }
    let mut probes = Vec::new();
    while let Some(result) = joins.join_next().await {
        if let Ok(probe) = result {
            probes.push(probe);
        }
    }
    probes.sort_by_key(|probe| {
        teracode_core::AdapterKind::ALL
            .iter()
            .position(|kind| *kind == probe.adapter)
            .unwrap_or(usize::MAX)
    });
    probes
}

fn print_doctor(report: &DoctorReport) {
    println!(
        "Repository: {} [{}]",
        report.repository.path.display(),
        if report.repository.is_git {
            if report.repository.dirty {
                "Git, dirty"
            } else {
                "Git, clean"
            }
        } else {
            "non-Git"
        }
    );
    for probe in &report.adapters {
        println!(
            "[{:<4}] {:<14} {:<24} structured={} resume={} model={} readiness={:?}",
            if probe.installed { "OK" } else { "MISS" },
            probe.adapter,
            probe.version.as_deref().unwrap_or("not installed"),
            probe.capabilities.structured_output,
            probe.capabilities.resume,
            probe.capabilities.model_selection,
            probe.readiness,
        );
    }
    for note in &report.notes {
        println!("NOTE: {note}");
    }
}
