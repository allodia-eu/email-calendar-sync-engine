# Rust Guidance

This repo treats the Rust API Guidelines as the default review standard:
<https://rust-lang.github.io/api-guidelines/about.html>

Use the checklist during API review:
<https://rust-lang.github.io/api-guidelines/checklist.html>

## API Design

- Follow Rust casing and naming conventions: modules/functions/methods in `snake_case`, types/traits in `UpperCamelCase`, acronyms like `Jmap`, `Imap`, `Uuid`.
- Use `as_`, `to_`, and `into_` according to conversion cost and ownership.
- Getter methods should usually be named after the field, not `get_*`.
- Public types implement useful common traits where correct: `Debug`, `Clone`, `Eq`, `PartialEq`, `Ord`, `PartialOrd`, `Hash`, `Default`, `Serialize`, `Deserialize`.
- Public errors must be meaningful. Prefer structured error enums with clear variants over stringly errors.
- Public structs should normally have private fields plus constructors/builders that preserve invariants.
- Use sealed traits when downstream implementations would constrain future evolution.
- Expose intermediate results when it avoids duplicate expensive parsing, normalization, or network work.

## Type Safety

- Newtype all ids and provider references: `AccountId`, `MessageId`, `EventId`, `MailboxId`, `CalendarId`, `ProviderKey`, `SyncStateId`.
- Do not use boolean parameters for behavior choices. Use enums or dedicated option types.
- Use `bitflags` only for true bitset flags. Do not force free-form provider keywords into fixed enums.
- Preserve provider-native data in explicit raw types, such as `RawMime`, `RawIcal`, and `RawJsCalendar`.
- Avoid `Option<T>` when the absence has multiple meanings; use an enum with named states.

## Documentation

- Every public module has crate/module docs explaining scope and invariants.
- Public fallible functions document `# Errors`.
- Public panic paths document `# Panics`.
- Unsafe functions or unsafe trait impls document `# Safety`.
- Rustdoc examples should use `?` rather than `unwrap`.

## Toolchain

One pinned Rust version builds this repo everywhere: the channel in `rust-toolchain.toml`. rustup
honours it for every cargo invocation inside the checkout, and CI parses the same file, so a
version bump is a one-line edit there (in its own PR — see AGENTS.md) and never a YAML edit. The
sole exception is `cargo fmt`, which runs on the pinned nightly because `rustfmt.toml` uses
nightly-only options. `rust-version` in the root `Cargo.toml` is a separate thing: the MSRV floor
the crates promise, not what CI builds with.

## Build time and disk

`cargo clean` is a symptom, not a maintenance task. If you are cleaning to free disk, something is
configured wrong — find it instead. A clean throws away the cache that separates a 30-second
rebuild from a five-minute one, so a workflow that needs one regularly pays that toll over and over.

The default is the trap. rustc emits full debug info (`debug = 2`) for **everything**, dependencies
included, and nothing in a stock workspace turns it down. Measured on a Surface Pro X (SQ1, 8
cores, arm64), `cargo test --workspace --all-features --no-run` from an empty target dir:

|                                      | before | after |
|--------------------------------------|-------:|------:|
| Cold build                           |  4m51s | **3m18s** |
| `target/` after that build           |  11 GB | **3.7 GB** |
| Rebuild after touching `engine-core` |    64s | **29s** |

Of that 11 GB baseline, 4.4 GB was PDBs under `debug/deps` — six test binaries carried one over
120 MB apiece — and another 4.4 GB was `debug/incremental`, most of it the same information again.
Running the whole gate (clippy + build + test + doc are separate unit graphs) now lands at 4.8 GB,
with 1.0 GB of PDBs.

The fix is the `[profile.dev]` block in the root [`Cargo.toml`](../../Cargo.toml) — our crates at
`line-tables-only`, dependencies at `debug = 0`. The rationale is written there; the short version
is that a panic backtrace needs a file and a line (which `line-tables-only` keeps) and a debugger
needs the rest (which nobody here uses on `rustls` or bundled SQLite). Read that comment before
changing it, and note that the other common reading of "optimize the dependencies" —
`[profile.dev.package."*"] opt-level = 3` — makes builds *slower*, not faster.

Two things this deliberately does **not** break, both verified rather than assumed:

- **Coverage.** `cargo llvm-cov` is source-based: `-C instrument-coverage` embeds the region map in
  the binary and `llvm-cov` never reads DWARF. `cargo llvm-cov -p engine-core --all-features
  --summary-only` returns identical figures either side of the change.
- **Backtraces.** `line-tables-only` is exactly the level that keeps file and line in a panic.

Need a real debugger? `cargo build --profile debugger` gets full info, and lands in
`target/debugger/` so it doesn't evict the warm `debug/` cache.

If the loop still drags on Windows, two per-machine wins that must **not** be committed (they are
true of one box, not of a contributor's or a runner's): link with `lld-link.exe` via a
`.cargo/config.toml` in a parent directory of the checkout, and give worktrees a shared
`CARGO_TARGET_DIR`. Both are described in the product core's `docs/debugging.md`.

## Measuring at scale

The test suite's mailboxes hold single digits of messages, so every claim it makes about
correctness it makes about none of the costs. A read that scans the whole mailbox and a read that
seeks an index are indistinguishable at four messages; at four hundred thousand they are the
difference between an app that opens and one that does not. `crates/mailbox-fixture` is the
mailbox that tells them apart.

```sh
cargo bench -p mailbox-fixture                          # 10k messages — while iterating
ENGINE_BENCH_SCALE=100k cargo bench -p mailbox-fixture  # what CI runs
ENGINE_BENCH_SCALE=400k cargo bench -p mailbox-fixture  # the ~20 GB mailbox, opt-in (minutes)
```

Eight operations, each on a path a user waits on: the first page of the list, the deep window
behind it, completing the shown conversations, a flag-only apply, a page of a sync, a cold open,
the thread-derivation pass that runs after **every** account sync, and the list read a user takes
*while* that page is committing. Beside criterion's own statistics the run prints one table —
`n / p50 / p90 / p99 / max` — because the numbers that decide whether a mail list feels broken are
the tail ones, and a mean hides them.

That last one is there because the other seven each have the store to themselves, which is the one
condition a mail client is never in. A change that lets reads and writes overlap does not move a
single uncontended number, so without `mixed/read_under_apply` it would be unmeasurable — and an
improvement nothing can measure is one nothing can keep. Any concurrency work belongs beside a
bench that would notice it going away.

Three properties make those numbers mean something, and all three stop being true the moment
somebody "optimizes" the fixture:

- **It is deterministic.** A seed fixes the mailbox byte for byte, so a baseline captured today is
  comparable with one captured after a refactor. Two runs measuring two different mailboxes would
  report the difference between them as a regression.
- **It reaches the store through the ordinary sync path** — one provider per folder, then claim,
  project, apply, release. Inserting rows behind the engine's back would make the store look faster
  than any sync could ever be.
- **It is conversation-shaped and IMAP-shaped.** Real reference graphs across six folders, so a
  thread read is a thread read; one scope per folder, so the per-scope loop a windowed read pays
  for is actually paid. A JMAP-shaped fixture is a single `Email` scope and would hide it.

Reading the table needs one habit: **compare across sizes, not against a number you remember.** An
operation whose p50 grows tenfold from 10k to 100k is linear in mailbox size; one that does not
move is not. That ratio is the finding — the absolute milliseconds belong to whichever machine
happened to run it. The `n` column says how far to trust the p99 beside it.

A host reduces its own logged durations to this same table, against whatever mail a user actually
has. A change that improves the fixture and not the host's log has optimized something nobody
waits on.

## Linting

Code should be clean under:

```sh
cargo +nightly fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

Do not silence lints unless the suppression is narrower than the code it protects and includes a reason.

## File Shape

- Keep files below 500 lines.
- `mod.rs` files should wire modules, not hold large implementations.
- Prefer one responsibility per file: identities, recurrence, provider keys, query AST, sync cursor, etc.
- Tests may live next to code for pure model logic; larger fixture suites should live under crate-level `tests/`.

