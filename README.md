# TeraCode

[![CI](https://github.com/colesmcintosh/teracode/actions/workflows/ci.yml/badge.svg)](https://github.com/colesmcintosh/teracode/actions/workflows/ci.yml)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-DEA584?logo=rust)](https://www.rust-lang.org/)
[![No telemetry](https://img.shields.io/badge/telemetry-none-6FB98F)](#configuration-and-local-data)

TeraCode is a local factory for building software-development factories. Inspired by field-factory thinking, it treats a development workflow as a production line that can be designed, inspected, run, and saved for reuse. A goal becomes an editable blueprint: persistent worker cells, provider/model assignments, a task DAG, selected Agent Skills, workspace controls, and executable quality gates. TeraCode then routes the line through planning, work, verification, bounded rework, and staged integration.

It is designed for developers who already install and authenticate their coding-agent CLIs. TeraCode does not embed provider SDKs, collect API keys, modify provider-global configuration, merge into the starting branch, send telemetry, or synchronize run history.

## Status

This repository contains a runnable first release for macOS and Linux. It includes the Ratatui switchyard UI, six built-in CLI adapters, deterministic fallback planning, provider-neutral contracts, bounded DAG scheduling, worktree isolation and binary patch assembly, direct acceptance checks, process-group cancellation, local SQLite history, Agent Skills discovery, and reusable factory blueprints.

Planner output validation and repair are exposed by `RecommendationEngine`; the interactive path currently uses its deterministic fallback planner so proposing a line never spends provider quota. The adapter boundary supports plugging in a structured provider planner without changing the scheduler or TUI.

## Install and run

Rust 1.85 or newer is required.

```sh
cargo install --path crates/teracode --locked
teracode doctor
teracode doctor --json
teracode --repo /path/to/repository
```

Running `teracode` opens the TUI. Its normal flow is:

1. Inspect Git and provider readiness.
2. Enter the product goal, worker-cell count, parallel-lane count, routing priority, provider preferences, and operating constraints.
3. Edit the proposed cells, assignments, models, objectives, dependencies, skills, and quality commands (including argument arrays, timeouts, and required/optional status).
4. Explicitly select workspace and autonomy controls.
5. Launch and watch the live routing rail.
6. Inspect the result or save the line as a reusable blueprint.

The interface works at 80×24, uses color-independent status labels, supports `--ascii`, and exposes the launch control to both Enter and mouse click. `Ctrl-C` during a run cancels process groups while preserving logs and workspaces; outside a run it exits.

## Safety model

Every run requires two independent choices. There are no executable defaults.

Workspace policy:

- **Worktree per agent** starts each worker from the selected committed `HEAD`, detects overlapping paths, applies binary-safe patches into a dedicated integration worktree, and stages successful output there.
- **Shared workspace** operates in the selected directory. Existing changes are preserved, but attribution, collision handling, and rollback are limited.
- **Read-only then executor** keeps parallel cells read-only and gives only the final executor a dedicated integration worktree.

Autonomy policy:

- **Read-only** maps to the provider's supported read/plan controls.
- **Workspace write** maps only to documented workspace-scoped or edit-accepting controls.
- **Full access** can expose dangerous native bypass flags and requires a second `!` confirmation for each run.

Unsupported provider/policy combinations are blocked before launch. Prompts are passed as process arguments, never interpolated into shell strings. Standard output and error remain separate, lines are capped at 1 MiB, unknown JSON becomes a redacted diagnostic event, and credential-shaped values are removed from diagnostics. TeraCode inherits the environment needed by an already-authenticated CLI but never reads provider credential files.

Dirty repositories are explicit: isolated modes use committed `HEAD` and exclude uncommitted changes; shared mode retains them and warns about limited attribution. Non-Git directories can use only shared mode. Successful isolated runs remain on a `teracode/<run>/integration` branch and worktree; TeraCode never merges it into the starting branch.

## Provider contracts

The built-ins probe the executable and version without invoking a paid model. Authentication is provider-owned and is first exercised when a production task starts.

| Adapter | Executable | Structured command | Resume | Policy notes |
|---|---|---|---|---|
| Claude Code | `claude` | `--print --verbose --output-format stream-json` | `--resume` | plan, accept-edits, or separately confirmed bypass |
| Codex | `codex` | `exec --json` | `exec … resume` | explicit sandbox and approval flags; no app server |
| OpenCode | `opencode` | `run --format json` | `--session` | plan agent for read-only; `--auto` for writes |
| Cursor | `agent` | `--print --output-format stream-json` | `--resume` | no enforceable read-only mapping; force requires confirmation |
| Grok Build | `grok` | `--single … --output-format streaming-json` | `--resume` | documented permission modes and sandbox profiles |
| Factory Droid | `droid` | `exec --output-format stream-json` | `--session-id` | read-only default, tiered auto, or confirmed unsafe bypass |

The command shapes follow the providers' published headless references: [Claude Code](https://docs.anthropic.com/en/docs/claude-code/cli-usage), [Codex](https://developers.openai.com/codex/cli/reference), [OpenCode](https://dev.opencode.ai/docs/cli/), [Cursor](https://docs.cursor.com/en/cli/reference/output-format), [Grok Build](https://docs.x.ai/build/cli/headless-scripting), and [Factory Droid](https://docs.factory.ai/droid-exec/overview). Provider releases can change; use `teracode doctor --json` to capture the installed version and advertised TeraCode capabilities when reporting an incompatibility.

Pi and Amp are intentionally not built in yet.

## Configuration and local data

TeraCode resolves platform paths with `directories::ProjectDirs("dev", "teracode", "TeraCode")`. `config.json` lives in the resulting OS configuration directory and `history.sqlite3` in the OS data directory (XDG locations on Linux and Application Support on macOS). SQLite uses WAL mode.

Configuration is optional. Missing quality, speed, or cost tiers stay unknown; TeraCode does not invent rankings or prices.

```json
{
  "provider_priority": ["codex", "claude", "open-code", "cursor", "grok", "droid"],
  "adapter_tuning": [
    {
      "adapter": "codex",
      "quality_tier": 4,
      "speed_tier": 3,
      "cost_tier": null
    }
  ],
  "retention": { "keep-latest": 250 }
}
```

History is kept forever by default. A run records its goal, plan, assignments, state transitions, normalized events, transcript content, checks, artifacts, probes, and worktree metadata. The history screen can select, export, delete, or load a prior blueprint for a clean retry. Deleting history does not delete review worktrees. On restart, unfinished runs become `Interrupted` and retain their artifacts. Adapter contracts preserve provider-specific resume invocation support for a future task-level resume flow; the current TUI deliberately offers a clean retry.

Agent Skills are indexed from repository `SKILL.md` files and the user's `.agents/skills` and `.codex/skills` directories. Selected instructions are copied only into the run-scoped prompt bundle. Native provider discovery continues unchanged; TeraCode does not install skills or rewrite global provider settings.

## Architecture and extension boundary

The Cargo workspace has three crates:

- `teracode-core`: stable domain contracts, recommendation and validation, state machines, scheduler, workspaces, checks, configuration, skills, and SQLite history.
- `teracode-adapters`: `AgentAdapter`, direct invocations, policy maps, stream normalization, redaction, probes, and process supervision.
- `teracode`: command parsing, factory execution composition, and the switchyard TUI.

To add a Rust adapter:

1. Add its `AdapterKind`, executable, capability declaration, documented invocation, resume form, and policy mapping in `teracode-adapters`.
2. Normalize only stable event fields; preserve unknown objects as diagnostics.
3. Add JSONL fixtures for init, assistant text, tools, usage, success, failure, malformed lines, stderr, unknown fields, and session IDs.
4. Add command-construction and policy tests. Never infer a bypass flag.
5. Update `teracode doctor` output and this table.

`AgentAdapter` is deliberately the future generic-adapter boundary: the scheduler sees only `TaskNode`, `AgentEvent`, and `TaskExecution`. Adding Pi, Amp, ACP, or a declarative adapter does not require scheduler or TUI changes.

## Development and verification

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
```

Default tests use recorded streams, temporary Git repositories, and fake executables; they never invoke a paid model. A deliberately ignored smoke test is available only with both an explicit environment flag and `--ignored`:

```sh
TERACODE_LIVE_TESTS=1 cargo test -p teracode-adapters --test live_providers -- --ignored
```

That command can consume provider quota. It runs installed adapters read-only in a temporary directory and depends on the user's existing authentication.

No license file is included; licensing remains an explicit project decision.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development conventions, [SECURITY.md](SECURITY.md) for private vulnerability reporting, and [CHANGELOG.md](CHANGELOG.md) for release notes.
