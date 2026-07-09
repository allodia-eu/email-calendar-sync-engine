# Building

This is a Cargo workspace containing the PIM sync engine crates, provider adapters, store, search, and test harnesses. The workspace is organized in [`crates/`](crates/) and driven by the virtual manifest in [`Cargo.toml`](Cargo.toml). See [`docs/agent-guidance/north-star.md`](docs/agent-guidance/north-star.md) for the build order and architecture.

## Prerequisites

- **Rust** (stable, **1.96+**, edition 2024) via [rustup](https://rustup.rs/).
  The repository pins `rust-version = "1.96"`.
- For linting and formatting, the `rustfmt` and `clippy` components:

  ```sh
  rustup component add rustfmt clippy
  ```

  The workspace [`rustfmt.toml`](rustfmt.toml) uses nightly-only options, so **formatting
  runs on nightly** (`cargo +nightly fmt`); CI pins the nightly for reproducibility. Everything
  else (clippy, build, test, docs) runs on stable.
- For coverage, `llvm-tools-preview` and `cargo-llvm-cov`:

  ```sh
  rustup component add llvm-tools-preview
  cargo install cargo-llvm-cov --locked
  ```

## Common tasks

```sh
# Build everything.
cargo build --workspace --all-features

# Run the offline test suite (unit + integration tests across all crates).
cargo test --workspace --all-features

# Open the API docs.
cargo doc --workspace --all-features --no-deps --open
```

Some tests are gated on a live Stalwart Docker harness (JMAP, IMAP, SMTP, CalDAV). See [`docs/agent-guidance/stalwart-harness.md`](docs/agent-guidance/stalwart-harness.md) and `scripts/ci/stalwart-live.sh` for how to run them locally.

## Verification (what CI enforces)

These checks are mandatory before a change lands (see [`AGENTS.md`](AGENTS.md)); CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs them on every push and pull request. Run them with warnings-as-errors to match CI exactly:

```sh
export RUSTFLAGS="-D warnings" RUSTDOCFLAGS="-D warnings"

scripts/ci/check-file-length.sh
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

Warnings are errors: the workspace forbids `unsafe`, requires docs on public
items, and runs clippy at the `pedantic` level. Every tracked `*.rs` file must
also stay under 500 lines — [`scripts/ci/check-file-length.sh`](scripts/ci/check-file-length.sh)
enforces that (rustfmt and clippy have no per-file length lint).

## Code coverage

The single source of truth for the coverage floor is [`codecov.yml`](codecov.yml). Read the floor from there and run the same offline metric CI uses:

```sh
# Human-readable per-file summary.
cargo llvm-cov --workspace --all-features --summary-only

# An lcov report (e.g. for Codecov or an editor gutter).
cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info

# The hard gate used in CI (excludes live/harness tests).
cargo llvm-cov --no-report --workspace --all-features
threshold="$(yq '.coverage.status.project.default.target' codecov.yml | tr -d '%')"
cargo llvm-cov report --fail-under-lines "$threshold" \
  --ignore-filename-regex 'stalwart-harness/|provider-[a-z]+/tests/'
```

> **Note on the threshold.** The lcov/cobertura exports report **100%** line
> coverage. llvm-cov's *native* line metric reads a fraction under that because
> it attributes region misses inside macro expansions and generic
> monomorphizations to source lines that the export formats count as covered —
> a tooling artifact, not untested logic. CI therefore reads the native metric
> floor from `codecov.yml` and treats the lcov export as the real signal.

## Layout

```text
.
├── Cargo.toml                    # virtual workspace + shared lints/deps
├── crates/
│   ├── engine-api/               # Host-facing facade
│   ├── engine-core/              # Domain model, ids, pure logic
│   ├── engine-sync/              # Sync orchestration and outbox
│   ├── engine-provider/          # Provider trait and shared contracts
│   ├── engine-store/             # Store trait
│   ├── store-sqlite/             # SQLite implementation
│   ├── engine-search/            # Query DSL and executor
│   ├── engine-recurrence/        # Recurrence expansion
│   ├── engine-mime/              # MIME / RFC 5322 body extraction
│   ├── engine-tls/               # Shared TLS trust policy
│   ├── provider-jmap/            # JMAP adapter
│   ├── provider-imap/            # IMAP + SMTP adapter
│   ├── provider-caldav/          # CalDAV adapter
│   ├── provider-graph/           # Microsoft Graph adapter
│   ├── engine-cli/               # Headless debugging / fixture harness
│   └── stalwart-harness/         # Docker-based protocol test harness
├── docs/
│   ├── providers.md              # User-facing provider guide
│   └── agent-guidance/           # Architecture and modeling specs
├── scripts/ci/                   # CI helpers (file-length, live harness)
└── .github/workflows/ci.yml      # The verification pipeline
```
