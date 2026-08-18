## Outcome

Describe the user-visible change and why it belongs in TeraCode.

## Safety and compatibility

- [ ] Provider commands still use direct argument arrays.
- [ ] New autonomy behavior is explicit and does not infer bypass flags.
- [ ] Default tests do not invoke paid models.
- [ ] No credentials, telemetry, or provider-global configuration were added.

## Verification

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo build --workspace --release --locked`
