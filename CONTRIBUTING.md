# Contributing to TeraCode

Thanks for helping improve TeraCode. Changes should keep the orchestrator local-first, provider-neutral, explicit about dangerous permissions, and safe to exercise without paid model calls in default CI.

## Development setup

Install Rust 1.85 or newer, then run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
```

Keep changes focused and include tests for observable behavior. Default tests must use fixtures, temporary repositories, or fake executables. Never add a test that invokes a paid provider unless it is ignored and gated by `TERACODE_LIVE_TESTS=1`.

## Adapter changes

An adapter contribution should include:

- a documented executable and structured-output invocation;
- an explicit capability declaration and policy mapping;
- no inferred approval or sandbox bypass;
- resume command construction when the provider supports it;
- JSONL fixtures for success, failure, usage, unknown fields, malformed lines, and session IDs;
- command-construction, normalization, redaction, and policy tests.

Prompts must remain individual process arguments. Do not use shell interpolation, read provider credential files, install skills, or rewrite provider-global configuration.

## Pull requests

Describe the user-visible outcome, safety implications, and checks run. Update nearby documentation when behavior or provider compatibility changes. Small, reviewable pull requests are preferred.

The repository currently has no open-source license grant. Licensing is being kept as a separate, explicit project decision.
