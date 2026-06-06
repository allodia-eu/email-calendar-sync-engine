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

## Linting

Code should be clean under:

```sh
cargo fmt --check
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

