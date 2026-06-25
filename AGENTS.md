# Agent Instructions

These instructions are mandatory for every coding agent in this repository. Think before coding, keep changes simple, edit surgically, and define verifiable success criteria before implementation.

## North Star

Build a standalone Rust PIM engine for mail/calendar sync, search, indexing, and writes. Native apps and server integrations are host adapters; they must not leak product-specific shortcuts into `engine-core`.

Read before relevant work:
- `docs/agent-guidance/north-star.md` for the product/architecture north star.
- `docs/agent-guidance/rust.md` before editing Rust.
- `docs/agent-guidance/modeling.md` before touching domain models.
- `docs/agent-guidance/providers.md` before touching protocol/provider code.
- `docs/agent-guidance/jmap.md` before touching the JMAP client (`engine-provider`, `provider-jmap`, `engine-sync`).
- `docs/agent-guidance/imap-smtp.md` before touching the IMAP/SMTP client (`provider-imap`, and the submission paths in `engine-provider`/`engine-sync`).
- `docs/agent-guidance/caldav.md` before touching the CalDAV calendar client (`provider-caldav`, the calendar sync path in `engine-provider`/`engine-sync`, or the SabreDAV fixture under `docker/sabredav/`).
- `docs/agent-guidance/graph.md` before touching the Microsoft Graph mail client (`provider-graph`, the Graph mail sync path, or the OAuth/capture tool under `tools/graph-oauth/`).
- `docs/agent-guidance/store-and-sync.md` before touching the store trait, sync orchestration, or the outbox.
- `docs/agent-guidance/search.md` before touching the query AST/DSL, the search executor, or projection→index rows.
- `docs/agent-guidance/search-coverage.md` before touching search result completeness or provider-search fallback.
- `docs/agent-guidance/calendar-semantics.md` before touching timezone handling, recurrence, or scheduling (iTIP/iMIP).
- `docs/agent-guidance/stalwart-harness.md` before touching the Stalwart Docker harness, the seed fixtures, or the protocol smoke tests (`docker/stalwart/`, `crates/stalwart-harness`).
- `docs/agent-guidance/engine-api.md` before touching the host facade (`engine-api`) or the bindings/reference-host seams (UniFFI, C ABI, CLI host).

## Hard Rules

- Files must stay under 500 lines. Split by responsibility before crossing that limit.
- Prefer small, testable modules over broad abstractions.
- Do not add speculative features, knobs, or provider shortcuts.
- Do not refactor unrelated code. Mention unrelated issues in the final answer instead.
- Do not write provider-specific assumptions into generic types unless a primary spec or provider doc proves they are universal.
- Lock identity, sync, store, search, and recurrence invariants in tests before writing implementation code.
- Keep public Rust APIs idiomatic by defaulting to the Rust API Guidelines: <https://rust-lang.github.io/api-guidelines/about.html>.
- Use newtypes for identities and protocol-specific references. Do not pass raw strings where a type can prevent mixing account ids, provider ids, mailboxes, events, or cursors.
- Avoid `unsafe`. If unavoidable, isolate it, document `# Safety`, and add tests around the safe boundary.

## Documentation Currency

`docs/agent-guidance/` is the durable baseline, not a one-time sketch. A large or architectural change MUST update the affected guidance docs in the same change, so code and docs never drift:

- When a change alters a decision, a trait or type signature, a crate's responsibility, an invariant, or where something lives, reconcile every guidance doc that states otherwise (north-star, store-and-sync, modeling, providers, search-coverage, calendar-semantics).
- If a new decision supersedes a doc's wording, rewrite the wording and record the rationale when it is non-obvious. Treat the docs as authoritative for the next agent: code and docs disagreeing is a bug to fix, not a discrepancy to leave.
- In the final summary, list which guidance docs you updated and why — or state explicitly that none needed changes.

## Test-Driven Workflow

- For Rust behavior changes, write or update tests before implementation.
- Aim for 100% meaningful coverage on Rust engine/model/search/sync logic. If a line is not worth testing, question whether it belongs.
- Every bug fix needs a failing test first.
- Every provider behavior needs a fixture or integration test tied to primary docs or an observed provider transcript.

## Required Verification

Before handing off Rust changes, run:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

When coverage tooling is available, also run:

```sh
cargo llvm-cov --workspace --all-features --fail-under-lines 100
```

If a command cannot run, say exactly why and what remains unverified.
