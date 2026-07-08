---
name: stalwart-live
description: Validate provider/protocol changes against a REAL server via the Dockerized Stalwart harness. Use when a change touches IMAP/JMAP/SMTP/CalDAV command or request shape (a new FETCH/SEARCH argument, a JMAP method call, a CalDAV REPORT body), or when the offline suite is green but you need to confirm the wire protocol — the offline provider mocks serve canned bytes and do NOT validate what was sent, so a wrong command shape passes offline yet breaks live. Wraps scripts/ci/stalwart-live.sh, the same script CI runs.
---

# Stalwart live protocol harness

The `provider-*` offline test fakes (`MockStream`, the JMAP fake executor, the
Graph fixture-replay server) reply with canned bytes **regardless of the request
they receive**. So an offline-green suite can still send a malformed command —
e.g. an unparenthesized `UID FETCH … UID FLAGS ENVELOPE …` that a lenient server
silently truncates to just `UID`. Only a real server catches that. This skill is
how you run one.

## When to reach for it

- You changed the bytes a provider sends: a FETCH/SEARCH item list, a JMAP
  request, a CalDAV `REPORT`/`PROPFIND` body, an SMTP command.
- The offline gate is green but the change is protocol-shaped and unverified.
- You are debugging a failure the CI "Stalwart protocol harness" job reported
  and want to reproduce it locally.

If the change is pure engine/model/store logic with no wire effect, the offline
gate is enough — skip this.

## How to run it

Everything goes through `scripts/ci/stalwart-live.sh` (Docker must be running).
The endpoints and throwaway credentials are baked into the script, so you set no
env vars.

```sh
scripts/ci/stalwart-live.sh all      # up + seed → smoke → jmap+imap+caldav (full local pass)
```

Or step by step while iterating:

```sh
scripts/ci/stalwart-live.sh up                              # start + self-seed (compose --wait)
scripts/ci/stalwart-live.sh test provider-imap              # one crate's live suite
scripts/ci/stalwart-live.sh test provider-imap live_imap_saves_a_draft   # a single test
scripts/ci/stalwart-live.sh logs                            # dump server logs after a failure
scripts/ci/stalwart-live.sh down                            # stop + wipe volumes
```

The harness stays up between runs, so leave it running while you iterate and
`down` when finished. `up` is idempotent (re-seeds from empty volumes only on a
fresh `down -v`).

## Diagnosing a live-only failure

When a test passes offline but fails here, the response is usually the clue: dump
what the server actually returned. A quick `eprintln!` of the raw line the parser
reads (and, if needed, the exact command sent) pinpoints a command/response
mismatch fast — that is how the missing-parens `UID FETCH` bug was found. Remove
the debugging before committing.

## After a fix

1. Re-run the affected `test <crate>` (or `all`) until green here.
2. If the offline mock could have caught the bug, tighten it — e.g. assert the
   command *shape* the provider sends, so the class of bug is guarded offline too.
3. Run the normal offline gate (see `AGENTS.md` → Required Verification) before
   pushing; the live job is gated and runs in CI, but the offline gate must pass
   locally first.
