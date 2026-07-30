# Every recipe is a thin wrapper around one cargo command, so nothing here is
# required to work on monitrs. `just --list` shows them; the body of each shows
# exactly what it runs.

_default:
    @just --list

# cargo fmt --all
fmt:
    cargo fmt --all

# cargo fmt --all -- --check
fmt-check:
    cargo fmt --all -- --check

# cargo check --workspace --all-targets
check:
    cargo check --workspace --all-targets

# cargo clippy --workspace --all-targets --all-features -- -D warnings
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# cargo test --workspace --all-features
test:
    cargo test --workspace --all-features

# cargo test --workspace --all-features -- --ignored
# Platform smoke tests. Ignored by default so `just test` stays hermetic.
test-platform:
    cargo test --workspace --all-features -- --ignored --test-threads=1

# cargo doc --workspace --no-deps
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# cargo deny check
deny:
    cargo deny check

# Type-check the Linux code paths from any host.
#
# Not optional: the `cfg(target_os = "linux")` branches — the /proc readers and the
# kill(2) sink — are invisible to a macOS build, and CI is a slow place to discover
# that. Requires `rustup target add x86_64-unknown-linux-gnu`.
check-linux:
    cargo check --workspace --all-features --target x86_64-unknown-linux-gnu

# Parse every GitHub workflow file. Requires PyYAML.
#
# Worth running before every push: an unparseable workflow fails with no line
# number and only after the push, reported merely as "a workflow file issue".
check-workflows:
    python3 .github/scripts/check_workflows.py

# The three verifications that need a real terminal or a real machine.
#
# None of them can run in CI: two drive the release binary on a pty and one measures
# this machine's CPU, so their answers are about the machine you run them on. Build
# first — they use `target/release/monitrs`.
verify-by-hand:
    cargo build --release -p monitrs
    python3 scripts/verify-terminal-restoration.py
    python3 scripts/verify-renice.py
    python3 scripts/measure-overhead.py

# Everything CI runs, in CI order.
ci: check-workflows fmt-check check check-linux clippy test doc deny
    @echo "all quality gates passed"

# cargo run -p monitrs
run *ARGS:
    cargo run -p monitrs -- {{ARGS}}

# cargo run -p monitrs --release
run-release *ARGS:
    cargo run -p monitrs --release -- {{ARGS}}

# cargo insta review
snapshots:
    cargo insta review

# cargo insta accept
snapshots-accept:
    cargo insta accept

# cargo bench --workspace
bench:
    cargo bench --workspace

# A quicker benchmark pass, as used for docs/benchmarks.md.
bench-quick:
    cargo bench -p monitrs --bench pipeline -- --warm-up-time 1 --measurement-time 3 --sample-size 20

# Verify the workspace resolves exactly one crossterm major version (§13).
crossterm-version:
    cargo tree -p crossterm --workspace

# Confirm the declared MSRV is real. Requires the 1.95.0 toolchain installed.
msrv:
    RUSTUP_TOOLCHAIN=1.95.0 cargo check --workspace --all-targets --all-features
