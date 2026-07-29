<!-- Thanks for contributing to monitrs. -->

## What this changes

<!-- One or two sentences. Link the issue if there is one. -->

## Target OS testing

A system monitor cannot be reviewed from the diff alone. Say what you actually
ran, not what you expect to work.

- [ ] Linux — version/distro:
- [ ] macOS — version/arch:
- [ ] Not applicable, because:

## Metric honesty

Tick only what applies to this change.

- [ ] No metric can now render `0` where the real state is unavailable,
      warming up, permission-denied, or unsupported.
- [ ] Stale values, if shown, are visibly marked and carry their age.
- [ ] Rates use the actual elapsed interval, not an assumed one.
- [ ] New process lookups key on `ProcessIdentity`, not a bare PID.
- [ ] Anything presented as a cause is worded as a correlation.

## Safety

- [ ] No destructive action can be triggered by a single keypress.
- [ ] Process identity is revalidated immediately before any signal.
- [ ] Process actions remain disabled while inspecting history.
- [ ] No command line or environment value is written to a log.

## Checks

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

- [ ] All three pass locally.
- [ ] New behaviour is covered by a test, or I have explained why it cannot be.
- [ ] UI changes have snapshot coverage for the states they affect.
- [ ] Docs updated (`README.md`, `docs/metrics.md`, `docs/configuration.md`) where relevant.
- [ ] `CHANGELOG.md` updated under `Unreleased`.
