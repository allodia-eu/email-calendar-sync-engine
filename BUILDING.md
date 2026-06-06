# Building

This is a Cargo workspace. Today it contains one crate,
[`engine-core`](crates/engine-core/) — the pure, I/O-free, async-free domain
model. More crates land per the build order in
[`docs/agent-guidance/north-star.md`](docs/agent-guidance/north-star.md).

## Prerequisites

- **Rust** (stable, **1.96+**, edition 2024) via [rustup](https://rustup.rs/).
  The repository pins `rust-version = "1.96"`.
- For linting and formatting, the `rustfmt` and `clippy` components:

  ```sh
  rustup component add rustfmt clippy
  ```

## Common tasks

```sh
# Build everything.
cargo build --workspace --all-features

# Run the tests (unit + the conformance suites under crates/engine-core/tests/).
cargo test --workspace --all-features

# Open the API docs.
cargo doc --workspace --all-features --no-deps --open
```

## Verification (what CI enforces)

These four checks are mandatory before a change lands (see
[`AGENTS.md`](AGENTS.md)); CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml))
runs them on every push and pull request:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

Warnings are errors: the workspace forbids `unsafe`, requires docs on public
items, and runs clippy at the `pedantic` level.

## Code coverage

Install the coverage tool once:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
```

Then:

```sh
# Human-readable per-file summary.
cargo llvm-cov --workspace --all-features --summary-only

# An lcov report (e.g. for Codecov or an editor gutter).
cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info

# The hard gate used in CI.
cargo llvm-cov --workspace --all-features --fail-under-lines 99
```

> **Note on the threshold.** The lcov/cobertura exports report **100%** line
> coverage. llvm-cov's *native* line metric reads a fraction under that because
> it attributes region misses inside macro expansions and generic
> monomorphizations to source lines that the export formats count as covered —
> a tooling artifact, not untested logic. CI therefore gates the native metric
> at `--fail-under-lines 99` and treats the lcov export as the real signal.

## Layout

```text
.
├── Cargo.toml                 # virtual workspace + shared lints/deps
├── crates/
│   └── engine-core/           # domain model (implemented)
│       ├── src/               # one responsibility per file, < 500 lines each
│       └── tests/             # conformance suites
├── docs/agent-guidance/       # architecture and modeling specs
└── .github/workflows/ci.yml   # the verification pipeline
```
