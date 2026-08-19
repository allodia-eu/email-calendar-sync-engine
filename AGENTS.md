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
- `docs/agent-guidance/google.md` before touching the Google (Gmail + Google Calendar) client (`provider-google`, the Gmail/Calendar sync paths, or the OAuth/capture tool under `tools/google-oauth/`).
- `docs/agent-guidance/tls.md` before touching TLS trust (`engine-tls`, a provider's transport/`connect` construction, or a host's certificate-trust wiring).
- `docs/agent-guidance/store-and-sync.md` before touching the store trait, sync orchestration, or the outbox.
- `docs/agent-guidance/search.md` before touching the query AST/DSL, the search executor, or projection→index rows.
- `docs/agent-guidance/search-coverage.md` before touching search result completeness or provider-search fallback.
- `docs/agent-guidance/mime.md` before touching MIME body extraction (`engine-mime`, the `MessageBody` type, or message-body fetch/caching in `engine-provider`/`engine-sync`/`store-sqlite`).
- `docs/agent-guidance/threading.md` before touching email threading (`Message.thread_id`, the `Thread` model, the message-id graph, the assignment inside the apply, or `Engine::rebuild_thread_index`).
- `docs/agent-guidance/calendar-semantics.md` before touching timezone handling, recurrence, or scheduling (iTIP/iMIP).
- `docs/agent-guidance/stalwart-harness.md` before touching the Stalwart Docker harness, the seed fixtures, or the protocol smoke tests (`docker/stalwart/`, `crates/stalwart-harness`).
- `docs/agent-guidance/engine-api.md` before touching the host facade (`engine-api`) or the bindings/reference-host seams (UniFFI, C ABI, CLI host).

## Hard Rules

- Files must stay under 500 lines. Split by responsibility before crossing that limit. This is
  CI-enforced by [`scripts/ci/check-file-length.sh`](scripts/ci/check-file-length.sh) (rustfmt and
  clippy have no per-file length lint), which runs in CI and locally from the repo root.
- **Identifiers in fixtures and docs use reserved names — never a real domain.** Addresses,
  hostnames and calendar ids in tests, fixtures and documentation must sit under a name reserved
  for the purpose: `example.com` / `.net` / `.org` (RFC 2606 §3), anything under `.test`,
  `.example`, `.invalid` or `.localhost` (RFC 2606 §2), or the harness's `test.local` (RFC 6762).
  CI-enforced by
  [`scripts/ci/check-fixture-identifiers.sh`](scripts/ci/check-fixture-identifiers.sh).

  This is a rule about **live debugging**, which is when it gets broken. The best fixtures are the
  *observed bytes* a real server returned — that is why they catch what invented ones miss — and
  those bytes carry whoever the account belonged to. Pasting them in is one keystroke; a public
  repository remembers them permanently, because a force-push moves a ref and leaves the old commit
  served by SHA. Anonymise **as you write the fixture**, keeping the byte *shape* (a fold that
  splits a parameter name must still split it) and replacing only the identifiers.

  A real domain that is structurally required — a provider's own identifier format, like Google's
  `<opaque>@google.com` iCalUID — belongs in `exempt()` in that script, with its reason.
- Prefer small, testable modules over broad abstractions.
- Do not add speculative features, knobs, or provider shortcuts.
- Do not refactor unrelated code. Mention unrelated issues in the final answer instead.
- Do not write provider-specific assumptions into generic types unless a primary spec or provider doc proves they are universal. A symptom found on one provider does **not** scope the fix to that provider — see "Provider-neutral by default" below, which is a required step, not a preference.
- Lock identity, sync, store, search, and recurrence invariants in tests before writing implementation code.
- Keep public Rust APIs idiomatic by defaulting to the Rust API Guidelines: <https://rust-lang.github.io/api-guidelines/about.html>.
- Use newtypes for identities and protocol-specific references. Do not pass raw strings where a type can prevent mixing account ids, provider ids, mailboxes, events, or cursors.
- Avoid `unsafe`. If unavoidable, isolate it, document `# Safety`, and add tests around the safe boundary.
- **`cargo clean` is a symptom, not a maintenance task.** If you are cleaning to free disk, something
  is configured wrong — find it instead, because every clean throws away the cache that makes the
  next build fast. The settings that keep `target/` small live in `[profile.dev]` in the root
  [`Cargo.toml`](Cargo.toml) (measured: 11 GB → 3.7 GB, 4m51s → 3m18s); the reasoning is in
  [`docs/agent-guidance/rust.md`](docs/agent-guidance/rust.md) → "Build time and disk". Read it
  before changing a profile, and never put a build fix in the workflow file — a fix that lives in
  CI is a fix nobody who builds locally gets.

## Provider-neutral by default

A bug reported against one provider is a claim about **one provider**. It is not the scope of the
fix. Treating it as the scope is how a neutral engine grows five different answers to the same
question, and how a host ends up branching on which provider it is talking to — which is the
engine leaking, and the thing `engine-core` exists to prevent.

**Before designing a response to any provider-specific symptom, survey the same surface across
every adapter that has it** — `provider-jmap`, `provider-imap`, `provider-caldav` (+ CardDAV),
`provider-graph`, `provider-google` — and write the result down as a table. Do this *before*
proposing a fix, not after: the survey routinely changes what the fix is.

Then **state the result in the final summary**: which providers share the gap, and whether the fix
landed in the neutral layer or in an adapter. If it landed in an adapter, say why the others
genuinely differ. "I checked and they differ" is a fine answer; not having checked is not.

The bar is highest for anything a **host can see**. A neutral verb, a capability, an error class,
or a type crossing `engine-api` must describe the *problem*, not the provider that happened to
surface it. If a host has to ask "which provider is this?" to handle a result correctly, the
engine has pushed its job outward.

**This is not hypothetical — it happened, and it cost real work.** Issue #93 arrived as "a JMAP
RSVP schedules no iTIP `REPLY`". The investigation stayed JMAP-shaped for several rounds and
produced two issues scoped to JMAP (an `engine-api` export, and a host-side check-then-write in
the app) before anyone looked sideways. The eventual survey took minutes and found the framing was
wrong twice over:

- conflict handling was **detection-only on every provider** — CalDAV, Graph and Google all refuse
  a stale write correctly via `If-Match`, and *still* told the host only that *something*
  conflicted, never what;
- **mail writes carried no guard concept at all** on any of the four, so `with_mail_writes()` takes
  no `WriteGuard` and every mail mutation is silent last-writer-wins.

Both JMAP-scoped issues were closed or superseded by #99, a neutral four-outcome write result that
serves every provider. The narrow framing cost two issues and a round of rework; the survey that
dissolved it was one `grep`.

**The tell:** if a fix names a provider in its type, its capability, its error, or its issue title,
stop and check whether the other providers have the same hole.

## Documentation Currency

`docs/agent-guidance/` is the durable baseline, not a one-time sketch. A large or architectural change MUST update the affected guidance docs in the same change, so code and docs never drift:

- When a change alters a decision, a trait or type signature, a crate's responsibility, an invariant, or where something lives, reconcile every guidance doc that states otherwise (north-star, store-and-sync, modeling, providers, search-coverage, calendar-semantics).
- If a new decision supersedes a doc's wording, rewrite the wording and record the rationale when it is non-obvious. Treat the docs as authoritative for the next agent: code and docs disagreeing is a bug to fix, not a discrepancy to leave.
- In the final summary, list which guidance docs you updated and why — or state explicitly that none needed changes.
- If the change came from a provider-specific symptom, the final summary also carries the cross-provider survey required by "Provider-neutral by default".

## Test-Driven Workflow

- For Rust behavior changes, write or update tests before implementation.
- Aim for 100% meaningful coverage on Rust engine/model/search/sync logic. If a line is not worth testing, question whether it belongs.
- Every bug fix needs a failing test first.
- Every provider behavior needs a fixture or integration test tied to primary docs or an observed provider transcript.
- The offline provider fakes (`MockStream`, the JMAP fake executor, the Graph fixture-replay
  server) reply with canned bytes **regardless of the request they receive**, so an offline-green
  suite cannot catch a wrong *command/request shape* (a malformed `UID FETCH` item list, a bad JMAP
  method, a wrong CalDAV `REPORT` body). Any change to the bytes a provider sends must be validated
  against a real server (the `stalwart-live` skill / `scripts/ci/stalwart-live.sh`) or a captured
  transcript — and where practical, tighten the offline fake to assert the shape it was sent.

## Test against the real server — non-negotiable

**Do not ship a feature or a fix to a provider until it has run against that provider's actual
server.** This is a library other people's mail depends on; "the unit tests pass" is not evidence
that a request Google, Microsoft, or an IMAP server will accept was ever sent. There is a real
server available for **every** protocol this repo speaks, so there is no case where this is
impractical:

| Protocol | Real server | How |
|---|---|---|
| JMAP · IMAP · CalDAV · CardDAV | the Dockerized **Stalwart** harness (`docker/stalwart`) | `scripts/ci/stalwart-live.sh` / the `stalwart-live` skill; `crates/provider-{jmap,imap}/tests/live_*.rs` |
| CalDAV (a second implementation) | the **SabreDAV** fixture (`docker/sabredav`) | `crates/provider-caldav/tests/live_sabredav.rs` |
| Gmail · Google Calendar · Google People | a **throwaway Google test account** | `tools/google-oauth` mints the token; `crates/provider-google/tests/live_*.rs` |
| Microsoft Graph | a **test Microsoft account** | `tools/graph-oauth` mints the token; `crates/provider-graph/tests/live_*.rs` |

The live tests are env-gated so the offline suite stays green without credentials — which makes them
easy to forget. Forgetting is the failure mode this rule exists to prevent:

- **The fakes cannot fail on a wrong request.** They answer canned bytes whatever you send (above).
  A green offline run says the *response parsing* is right and says nothing at all about the request.
- **Real servers reject things no spec reading predicts.** Gmail 400s an unknown label id; it
  rewrites the caller's `Message-ID` on send; `messages.insert` needs the `/upload/` endpoint. Every
  one of those was found by calling the server and would have shipped otherwise.
- **Coverage does not help here.** A line can be 100% covered by a test whose fake would have
  accepted any bytes at all.

So, concretely, before a provider change is done:

1. Run that provider's `live_*` tests, and **add one** when the change introduces a request shape or
   a server behaviour no existing live test exercises.
2. **Capture the real response as a fixture** (`tests/fixtures/`, scrubbed of PII per that
   directory's README) and wire it into an offline test, so the next agent inherits the observed
   truth rather than a guess.
3. **Prove the new check can fail** — revert the fix, watch the test go red, restore. A live test
   that would pass against the broken code is not a live test; it is a slow unit test.
4. **A live test that asserts an *absence* must first prove the absence is not something we
   failed to send.** A real server does not rescue you from the fake-request-shape trap above
   if you only ever send one shape — it just makes the wrong conclusion look authoritative.
   Before recording "the server does not do X", send the request that *asks* for X and show
   the difference; then pin **both** directions, so the claim cannot be produced by an adapter
   that is stuck. Skipping this cost months: #93 recorded "Stalwart schedules no iTIP `REPLY`
   from a JMAP RSVP" as server behaviour, drafted upstream bug reports, and shaped a
   capability around it — when `provider-jmap` had simply never sent
   `sendSchedulingMessages`, whose default is `false`. A cross-protocol control arm is *not*
   this proof: the CalDAV arm "worked" only because RFC 6638 auto-schedules and has no
   equivalent opt-in, so the two were never comparable. See #102.
4. If a live run genuinely cannot be done, say so explicitly in the PR and name what is unverified.
   That is a disclosure, not a default.

### When the gate is slow, it is usually not the code

The suite runs in **under 20 seconds**. Everything else a `cargo test --workspace --all-features`
costs is building 96 test binaries and *starting* them, so a slow gate is a machine problem, not a
test problem — measure before assuming otherwise:

```sh
grep -c "^test result:"   run.log   # suites finished, out of ~96
grep -oE "finished in [0-9.]+s" run.log | grep -oE "[0-9.]+" | awk '{s+=$1} END {print s" s"}'
```

Three things have each cost multiples here, none of them visible in a profiler:

- **macOS Gatekeeper, which is usually the biggest.** `syspolicyd` assesses every locally built
  executable the first time it runs — hashing the whole file — and `XProtectService` scans it too.
  96 freshly linked test binaries means 96 first-run assessments, in another process: low CPU in
  yours, constant disk reads, nothing to profile. One measured run was **35.8 minutes wall for
  19.3 seconds of tests.** The fix is Apple's developer exemption and is per-machine, which is why
  it is written here and not in a file: **System Settings → Privacy & Security → Developer Tools**,
  add the app that *hosts* the build (the terminal, or the editor you launch it from), then restart
  it. The exemption follows the responsible process, so the host covers cargo, rustc and every test
  binary under it — there is nothing to register per binary, and nothing could be, since their
  hashes change every build. It does not disable Gatekeeper; `spctl --master-disable` is not the
  answer. Same commit, same 19 crates rebuilt, before and after: **35.8 min → 7.8 min.**
- **Debug info in the profile that actually builds** — see the `[profile.test]` comment in
  [`Cargo.toml`](Cargo.toml). Setting it on `dev` alone does nothing for `cargo test`. Measured
  **368s → 149s** on `--no-run` after touching one crate.
- **Artifacts nothing reclaims — and past a point, deleting all of them is the fastest thing you
  can do.** Cargo prunes neither `target/debug/incremental` (a session dir per compilation context)
  nor superseded test executables: one day of iterating left a **34 GB** target holding **412**
  executables, on a disk at 96%. In that state the "warm cache" is not a cache, it is ballast.
  Measured, same commit, same trigger of 19 recompiled crates:

  | target | wall | user | sys |
  |---|---|---|---|
  | 34 GB, disk at 96% | **479 s** | 83 s | 239 s |
  | 3.9 GB, disk at 88% | **21.7 s** | 43 s | 43 s |

  The first spent 83 s computing and 239 s in the kernel across 479 s of wall clock — it was
  waiting, not building. A **cold** build of all 189 crates from an empty target took **57 s**, so
  the bloated warm cache was 8× slower than having no cache at all. `rm -rf target` is therefore a
  legitimate one-time reset once the dir has bloated, and the usual advice against it applies to a
  *healthy* target, not this. It regrows fast — two builds took the fresh 3.9 GB back to 7.0 GB and
  88 executables to 124 — so this is periodic, not permanent.

## Required Verification

**Run this full gate and make it pass before every `git push`** (not only at final hand-off). It
mirrors the CI **"Format, lint, build, test, docs"** job *exactly* — same commands, same
warnings-as-errors (`RUSTFLAGS`/`RUSTDOCFLAGS = -D warnings`) — so green here means green there.
Skipping `cargo fmt` (or running `--check` without fixing) has failed this job on many PRs; a
freshly hand-written line that overflows the width is the usual culprit, and CI catches it even
though the code compiles. So: **`cargo +nightly fmt --all` first, then verify.** Note that
`rustfmt` does **not** reformat inside macro bodies, so a long line inside `try_stream!` /
`async_stream!` (used across the providers) trips `error_on_line_overflow` and `fmt` will not fix
it — hand-wrap those lines yourself.

Format on **nightly**: the workspace [`rustfmt.toml`](rustfmt.toml) uses nightly-only options
(crate-granular grouped imports, `wrap_comments`, `style_edition = "2024"`,
`error_on_line_overflow`, …), so a plain stable `cargo fmt` silently ignores them and leaves the
repo unformatted. CI runs the fmt check on a **pinned** nightly (`nightly-2026-07-07`, in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)) so its output is reproducible against what
you run locally; bump that pin **and** re-run fmt when intentionally adopting a newer nightly.

Build/clippy/test/docs run on the **pinned** channel in
[`rust-toolchain.toml`](rust-toolchain.toml) — the single source of truth for the Rust version.
rustup selects it for every cargo invocation inside this checkout (installing it on first use), and
CI's `toolchain` job parses the same file and installs exactly that channel, so local and CI are
byte-identical and the version is never duplicated in YAML. Bumping it is a deliberate, standalone
PR: a new release can add default-warn lints, and the workspace denies all warnings, so the bump is
where those get fixed rather than a red build on someone's unrelated change. The pin is *not* the
MSRV — that floor is `rust-version` in the root `Cargo.toml` and moves independently.

```sh
scripts/ci/check-file-length.sh       # every tracked *.rs must be <= 500 lines
scripts/ci/check-fixture-identifiers.sh   # no real domains in fixtures or docs
cargo +nightly fmt --all              # fix formatting first (nightly rustfmt.toml)
export RUSTFLAGS="-D warnings" RUSTDOCFLAGS="-D warnings"   # match CI: warnings are errors
cargo +nightly fmt --all --check      # must now be clean
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

**`--all-features` is not decoration on the test line.** A plain `cargo test --workspace` builds 32
test binaries here; with `--all-features` it builds 91. The difference is whole suites — the scale
fixture's among them — that simply do not run, and a suite that does not run reads exactly like a
suite that passed. Run the block as written.

### Coverage (catch a dip before the PR)

The enforced **line-coverage floor** and the per-diff **patch target** live in one
place — **`codecov.yml`** (`coverage.status.project.default.target` and
`…patch.default.target`). CI's coverage job reads the floor from there with `yq`, and
Codecov enforces both, so the number is defined once. Run the same check locally before
`git push` so you catch a regression before CI does. The offline metric excludes the
live/harness tests (they run in the gated `stalwart` job); the exclusion list mirrors
CI's `COVERAGE_IGNORE` (see `.github/workflows/ci.yml`):

```sh
cargo llvm-cov --no-report --workspace --all-features
threshold="$(yq '.coverage.status.project.default.target' codecov.yml | tr -d '%')"   # single source
cargo llvm-cov report --fail-under-lines "$threshold" \
  --ignore-filename-regex 'stalwart-harness/|provider-[a-z]+/tests/'
```

New/changed lines must clear the **patch** target too, so cover new code. A provider's
thin HTTP/TLS transport boundary is the one place offline coverage is hard; drive it
with a mock HTTP server / fake executor rather than leaving it to the live tests
(`provider-jmap`'s `lib_tests.rs` + `watch_tests.rs` are the pattern). To find the exact
uncovered lines: `cargo llvm-cov -p <crate> --all-features --show-missing-lines`.

If a command cannot run, say exactly why and what remains unverified.

## Chained work goes in a stack, not in parallel PRs off `main`

**A PR that depends on another sets its base to that branch, never to `main`.**

```sh
gh pr create --base <the-branch-below>
```

[`.claude/skills/gh-stack`](.claude/skills/gh-stack) wraps the `gh stack` extension, which keeps
the chain rebased and the bases pointed at each other for you.

Merge bottom-up; GitHub retargets the rest of the stack automatically as each one lands.

The failure this prevents is invisible until the first merge, which is why it is written down.
Branch B off branch A and open both against `main`, and B's diff silently contains A's commits.
Nothing complains — both PRs read fine, and CI is green on both. Then A is merged with **rebase**
or **squash**, which gives its commits new SHAs on `main`, and B is still carrying the old ones:
every PR above the one that landed conflicts at once, in changes their authors never touched. The
same branches with the right bases would have needed no intervention at all.

The tell that you are about to do this: a PR whose "files changed" includes work you know belongs
to a different review. Check that before opening, not after merging.

## Breaking changes
Don't be afraid to make breaking changes. We're in early product development and prefer
a breaking change over workarounds/patchwork if that's cleaner for the future.
Just make sure to ask the developer if it's OK for you to make that breaking change
before actually implementing it.
