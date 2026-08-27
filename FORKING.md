# Fork maintenance

This repository is [kylins-client](https://github.com/kenizhou/kylins-client)'s fork of
[allodia-eu/email-calendar-sync-engine](https://github.com/allodia-eu/email-calendar-sync-engine).
We cannot submit PRs upstream, so the fork carries our engine changes as a small,
disciplined patch series on top of `upstream/main`, rebased periodically.

## Remotes

- `origin` — this fork (the only push target).
- `upstream` — allodia-eu (fetch only; never push).

## Patch series

| Patch | Why |
|---|---|
| FTS tokenizer option — creation-time `porter unicode61` / `trigram` choice, `meta.fts_tokenizer` recording, mismatch refusal, CJK substring acceptance pins (8 commits + 1 upstream-adaptation commit) | kylins needs CJK substring search; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §4 |
| EAS provider relocation — import of the Kylins Exchange ActiveSync protocol client at provenance `0dc611d` plus an engine-quality retrofit (edition 2024, workspace lints, 500-line module split, transport on `engine-tls`, env-gated live suite, offline mock-HTTP transport harness, guidance docs) (15 commits) | kylins P0: the engine needs an EAS provider; spec: kylins-client `docs/superpowers/specs/2026-08-23-p0-engine-preparation-design.md` §3. Protocol client only — the `Provider` trait impl follows in the next series (Plan C) |

Every fork-only change must appear here with its motivation, or it will surprise
the next rebase.

## Rebase procedure

1. `git fetch upstream`
2. Rebase the patch series onto `upstream/main` (`git rebase upstream/main` on
   the branch holding it, or on fork `main` if the series lives there).
   Conflicts are expected where upstream edits files we restructured — resolve
   by keeping our shape and folding in upstream's new content (see the
   "Adapt an upstream test to the tokenizer-taking migrate" commit for the
   shape of adapting new upstream call sites to changed fork signatures).
3. Run the full verification gate from `AGENTS.md` before pushing to `origin`.
4. Update the patch-series table above if the series changed.

Cadence: whenever upstream lands something we want (the engine moves fast —
blob GC, delta bounds, occurrence edits all arrived within weeks); at minimum
monthly. Force-pushing `origin/main` after a rebase is expected in a fork;
upstream history is never rewritten.

## Standards

All engine-repo discipline still applies here (CI gate, 500-line cap, fixture
identifier rules, real-server evidence). Cheap rebases are the return on that
investment, and a clean series stays upstreamable as one PR if the situation
ever changes.

## Line endings on Windows checkouts

`core.autocrlf` must be `input` (or `false`) in this clone: the pinned nightly
rustfmt enforces LF, and an `autocrlf=true` checkout fails `cargo fmt --check`
repo-wide. Two fixture files legitimately contain CRLF bytes
(`crates/engine-api/tests/fixtures/stalwart-invitation.eml`,
`crates/provider-caldav/tests/fixtures/sync-initial.xml`) — do not run
`git add --renormalize` across them.
