# Stalwart Protocol Test Harness

This document is authoritative for the Stalwart Docker harness: the deterministic
fixture that validates both modern (JMAP) and legacy (IMAP/SMTP/CalDAV) protocol
paths in local and CI tests. It is build-order step 3 (`north-star.md`) and the
concrete realization of the **Stalwart Test Spine** in `providers.md`. Read it
before touching anything under `docker/stalwart/` or `crates/stalwart-harness/`.

The harness is **test infrastructure, not product code**. It seeds and probes a
real server; it does not contain provider clients. The JMAP client is step 4 and
the IMAP/SMTP/CalDAV clients are step 5 — they consume this fixture, they are not
part of it.

> A **second CalDAV fixture**, SabreDAV, lives in `docker/sabredav/` (see
> `caldav.md`). It validates `provider-caldav` against a different real
> implementation than Stalwart (two-step RFC 6764 discovery, the
> `http://sabre.io/ns/sync/N` sync-token form) and **reuses this harness's shared
> calendar seed** (`seed/calendar/`), so one dataset validates both servers. The
> determinism, gating-by-env-var, and excluded-from-offline-coverage conventions
> below apply to it too.
>
> The two are **not interchangeable as evidence**, and `caldav.md` ("Which server
> proves what") is authoritative for the split. Two facts about *this* harness drive
> it: Stalwart **reserializes** the iCalendar it stores (it keeps every property but
> re-folds lines and reorders `RRULE` parts), so a write test may never assert that the
> server hands back the bytes it was sent; and Alice **owns every calendar she can
> see**, so this fixture cannot produce a collection she may not write — the read-only
> `DAV:current-user-privilege-set` case is SabreDAV's to prove, and it seeds a shared
> calendar for exactly that.

## What it is

- A Stalwart server in Docker, **pinned by image digest**, brought up by
  `docker/stalwart/docker-compose.yml` as a single self-contained service.
- A **self-bootstrapping entrypoint** (`docker/stalwart/entrypoint.sh`) that
  drives Stalwart's first-run setup non-interactively through its management API
  (Stalwart v0.16 has no declarative config file — see below), then seeds the
  shared dataset.
- A **deterministic curl seeder** (`docker/stalwart/seed.sh` + `seed/`) loading
  one shared dataset every protocol sees.
- A Rust test-support crate (`crates/stalwart-harness`) with environment-based
  discovery, a readiness poller, and a **gated connectivity smoke suite**.
- A **separate Linux CI job** (`stalwart` in `.github/workflows/ci.yml`) that
  brings the service up, waits for the post-seed readiness marker, and runs the
  smoke suite. It is the only job that sets `STALWART_*`, so it is the only one
  whose Stalwart tests execute.

## How it bootstraps (Stalwart v0.16)

Stalwart **v0.16 replaced the old declarative-TOML configuration with a
registry/bootstrap model**: the `--config` file is a small JSON pointer to the
data store, and everything else (listeners, directory, principals) lives in the
store, configured through a JMAP management API. A fresh server boots into
"bootstrap mode" (a recovery HTTP listener on 8080) and expects setup to be
completed via that API, after which it must restart to come up as a full server.
The older TOML approach the secondary docs describe applies to ≤0.15 only.

`entrypoint.sh` drives this deterministically, as PID 1 of the single container:

1. Start the server. A fixed fallback admin (`STALWART_RECOVERY_ADMIN`) gives the
   management API a known credential from first boot — no random password, no
   wizard.
2. If in bootstrap mode, `x:Bootstrap/set` completes setup with an internal
   directory and **no ACME / auto-TLS request** (so boot stays offline and
   self-contained), then the server is restarted into a full server.
3. Create the test accounts with `x:Account/set` (idempotent: existing accounts
   are skipped). The default domain `test.local` is auto-created at bootstrap;
   its id is looked up at runtime.
4. Seed mail + calendars + contacts (see below), write a readiness marker, and run the
   server in the foreground.

Re-running against an already-bootstrapped data volume skips steps 2–3 and
re-seeds idempotently. A clean slate (`docker compose down -v`) re-bootstraps.

## Settled decisions

These were confirmed before/with implementation; do not relitigate without cause.

- **Image:** `stalwartlabs/stalwart` (the `stalwartlabs/mail-server` repo is the
  legacy name), pinned by **multi-arch sha256 digest** in the compose file, not a
  moving tag. The HTTP listener serves JMAP, CalDAV/CardDAV (WebDAV), and the
  management API together. Bump deliberately: re-resolve the digest and update the
  comment that records the version.

  **A bump is its own PR.** The pin decides what *every* live test in this repo is
  evidence about, so it must not ride along inside a change that is really about
  something else — a provider fix and a server change landing together leave neither
  attributable. Currently pinned: **v0.16.15**. What a bump owes:

  1. Re-resolve the **index** digest (`docker buildx imagetools inspect
     stalwartlabs/stalwart:<tag>` → the top-level `Digest:`, not a per-platform one, or
     the other architecture stops resolving).
  2. Run the whole gated suite on the new pin — `scripts/ci/stalwart-live.sh down && …
     all` — from a wiped volume, since the entrypoint bootstraps differently on a warm
     store.
  3. **Reconcile every claim that named a version.** Behaviour the docs pin to "what
     Stalwart does" is only true of a vintage; grep for the old version string. This is
     how v0.16.14's `ifInState` enforcement was caught (see `jmap.md` — it changed the
     evidence but not the decision, because the engine sends no precondition).
  4. Say what the bump did **not** fix, so the next agent does not re-run the same
     investigation hoping. It did not fix the JMAP RSVP scheduling gap (issue #93).
- **Transport:** **plaintext HTTP on 8080** (JMAP + CalDAV + management) and
  **plaintext SMTP on 25**; **IMAP is implicit-TLS on 993**. Stalwart **supports**
  STARTTLS on the standard IMAP (143) and submission (587) ports, but its
  [security guidance](https://stalw.art/docs/install/security/) recommends implicit
  TLS (993/465) and treats 143/587 as non-essential ("should generally be disabled"),
  so the harness's fresh bootstrap does **not** create those listeners — it comes up
  with 993/465 only. To exercise the engine's STARTTLS paths (which real, older servers
  do require), the **entrypoint provisions** (via the `x:NetworkListener/set` registry
  method) an **IMAP STARTTLS listener on 143** and an **SMTP submission STARTTLS listener
  on 587** (`useTls` + `tlsImplicit:false`), both host-mapped and surfaced as
  `STALWART_IMAP_STARTTLS_ADDR` / `STALWART_SMTP_STARTTLS_ADDR`; a newly created listener
  needs a server restart to bind, which the entrypoint does on a fresh bootstrap. The
  smoke suite and the IMAP seeder accept the server's **self-signed test certificate
  explicitly** (rustls with a probe-only no-verify verifier; `curl -k`). This never
  touches a host trust store. Host ports are loopback-only and high-numbered.
- **Seeding mechanism:** **Stalwart-native via `curl`** in the entrypoint — it
  talks to the server over its management API (accounts) and the real wire
  protocols (IMAP `APPEND`/`STORE`/`COPY`/`MOVE` for mail, CalDAV `PUT` for
  calendars, CardDAV `PUT` for contacts). No Rust provider client and no extra pinned binary; the image ships
  `curl` + `/bin/sh`.
- **Where it lives:** `docker/stalwart/` for compose + entrypoint + seed; the Rust
  readiness/probe helpers and the smoke suite in `crates/stalwart-harness`. Tests
  discover the server through `STALWART_*` environment variables.
- **Gating:** **env-var presence + skip** (see below). Chosen over a cargo
  feature so the offline `cargo test --workspace` needs no special flags.
- **Scope:** the established seed covers mail + calendar + contacts. The contact
  set contains an international person and a group, is written over CardDAV,
  and is read through both CardDAV and JMAP Contacts by a gated normalization
  parity test. Offline RFC-shaped fixtures remain the fast conformance layer;
  do not treat an offline fake alone as server-acceptance evidence.

## The gating contract

Every Stalwart-touching test starts by calling `Harness::from_env()`, which
returns `Some` only when `STALWART_HTTP_ADDR` is set, and `None` otherwise. On
`None` the test prints a skip line and returns. Consequences:

- `cargo test --workspace` with **no Docker** runs the whole suite as no-ops and
  stays green. This is the **offline gate** and it is non-negotiable.
- The `stalwart` CI job sets `STALWART_*`, so there — and only there — the smoke
  tests actually connect.
- Locally, export the `STALWART_*` variables (see `docker/stalwart/README.md`)
  after `docker compose up` to exercise them.

The crate's *pure* logic (base64, HTTP response parsing, the IMAP probe driven
over a canned stream, count parsing) is unit-tested and runs offline; only the
live socket probes are gated. The coverage job excludes `crates/stalwart-harness/`
from its metric, because those probes are exercised by the `stalwart` job, not
the offline run. The IMAP probe is generic over a `Read + Write` stream so the
TLS dependency (`rustls`) stays a test-only dev-dependency, out of the library
and the cross-compile build.

## The accounts

| Account | Holds | Used for |
| --- | --- | --- |
| `alice@test.local` | **the whole shared seed** (mail, calendar, contacts) | every read/sync/write suite; several assert its exact mailbox and calendar counts |
| `bob@test.local` | nothing | scratch: the SMTP recipient in the submission tests, and the **organizer** in the scheduling suite |
| `carol@test.local` | nothing | scratch: the **attendee** in the scheduling suite |

`Harness::scratch` exposes the two scratch accounts as a pair. They exist because
some server behaviour cannot be observed without writing to a mailbox: RFC 6638
auto-scheduling has the server **mail** the attendee an invitation *and* the
organizer the reply, and that mail cannot be switched off. Running such an
exchange through Alice would push her INBOX permanently over the exact count the
mail suites assert, so the whole two-party exchange happens between Bob and Carol,
where nothing counts what is delivered. Prefer a scratch account over relaxing an
assertion on the seeded one.

## Rate limiters are disarmed on purpose

The entrypoint raises Stalwart's **inbound rate limiters** at bootstrap and logs
`relaxed inbound rate limiters`. This is not incidental tidying — without it the
scheduling suite is not re-runnable, and it fails in the most misleading way
available.

Stalwart ships two enabled by default, both visible via
`x:MtaInboundThrottle/get`:

| Throttle | Key | Default rate |
| --- | --- | --- |
| Sender IP | `remoteIp` | 5 / second |
| Sender address to recipient | `senderDomain` + `rcpt` | **25 / hour** |

The second one is the biter. RFC 6638 auto-scheduling makes the server **mail both
parties** of every invitation, so the CalDAV scheduling suite spends ~6 messages
per run and used to stop working after about four. Past the cap Stalwart abandons
the **entire** iTIP delivery — the attendee's calendar copy included — while still
answering the organizer's `PUT` with `201`, and logs nothing. Every scheduling test
then times out on a delivery that never arrives, which reads exactly like a code
regression.

A rate limiter also makes a result depend on *how many times the suite has already
run*, which is the direct opposite of the determinism this fixture exists for. So
the entrypoint sets both to effectively unlimited (`1000000/1s`). Verified: eight
consecutive scheduling runs, where the fourth used to fail.

These are **store-backed settings**, not file config: v0.16's `config.json` holds
only the `DataStore` object, and everything else lives in the database behind the
JMAP management API (`x:`-prefixed objects — `configuration/declarative-deployments`
in the Stalwart docs). The object name is `MtaInboundThrottle`; guessing at
`x:Setting`/`x:RateLimit` gets `unknownMethod`, which is easy to mistake for "not
configurable". The full object registry is the Stalwart docs' `ref/object` index —
consult it rather than probing.

## The seed dataset — and the invariant each fixture protects

One shared dataset, loaded into `alice@test.local`. Fixtures carry content the
harness controls; **nothing asserts on server-assigned ids** (see determinism).
Because Stalwart does not match a `SEARCH HEADER Message-ID`, the seeder targets
messages by **append order** (it clears INBOX, then appends in a fixed order, so
sequence numbers are deterministic) rather than by searching.

### Mail (`seed/mail/`, via IMAP into Alice)

| Fixture                  | Placement                    | Invariant it protects |
| ------------------------ | ---------------------------- | --------------------- |
| `01-plain.eml`           | INBOX, **COPY**ed to Archive | An IMAP copy in another folder is a distinct provider object — two memberships, one content. |
| `02-dup-msgid-a/b.eml`   | INBOX (shared `Message-ID`)  | `Message-ID` is a threading/reconciliation hint, **not** identity: two distinct stored objects (both carry "Duplicate" in their subject for the smoke search). |
| `03-no-msgid.eml`        | INBOX (no `Message-ID`)      | Missing `Message-ID` → locally-derived threading; still a distinct object. |
| `04-attachment.eml`      | INBOX (multipart/mixed)      | File-attachment path (vs item/reference/inline kinds). |
| `05-flagged.eml`         | INBOX, `\Flagged` + keyword  | JMAP keywords ↔ IMAP flags. Stalwart marks owner-appended mail `\Seen`, so the distinctive markers are `\Flagged` and the custom keyword `harness`. |
| `06-thread-root/reply`   | INBOX (`In-Reply-To`/`References`) | Threading by references, independent of subject. |
| `07-moved.eml`           | INBOX → **MOVE**d to Projects | A moved message keeps a single membership (contrast the copy). |

Folders `Archive` and `Projects` exercise non-INBOX mailboxes. Two further dedicated
mailboxes carry isolated copies of INBOX fixtures so a test can mutate them without
disturbing the count-asserted folders: `QResync` (three messages, for the
CONDSTORE/QRESYNC delta test) and `Idle` (one message, for the IMAP `IDLE` push test,
which flag-toggles it on a second connection — `imap-smtp.md`).

### Calendar (`seed/calendar/`, via CalDAV into Alice's default calendar)

| Fixture                  | Invariant it protects |
| ------------------------ | --------------------- |
| `one-off.ics`            | A single zoned event with an embedded `VTIMEZONE`; every event materializes occurrences. |
| `recurring-weekly.ics`   | `RRULE` + `EXDATE` (excluded instance) + `RECURRENCE-ID` (a moved instance): recurrence with exceptions/overrides. |
| `meeting-attendees.ics`  | `ORGANIZER` + `ATTENDEE` with `ROLE`/`PARTSTAT`/`RSVP`: participants (and the inbound iTIP/iMIP target later). |
| `virtual-location.ics`   | RFC 7986 `CONFERENCE` (a virtual location / conference link). |
| `all-day.ics`            | `DTSTART;VALUE=DATE`: zoneless, no DST, never zone-attached. |
| `floating.ics`           | Floating wall-clock time (no `TZID`, no `Z`): resolves through the observer's zone. |

These map directly onto the requirements in `providers.md` (Stalwart Test Spine)
and `calendar-semantics.md` (time model, recurrence subset, iTIP/iMIP).

## Determinism rules

The whole point is that fixed inputs produce identical observable state every
run. Non-negotiable:

- **Never assert on server-assigned opaque ids.** Stalwart generates JMAP object
  ids, IMAP `UIDVALIDITY`/`UID`, sync tokens, and JSCalendar uids. Assert on
  fields the harness controls — subjects, addresses, iCalendar UIDs, mailbox
  counts, append order — or capture generated ids at runtime.
- **Pin the image by digest** and fix every seed input. Calendar fixtures use
  fixed absolute 2026 dates so occurrence expansion is stable.
- **Readiness, not sleeps.** The compose healthcheck gates on a post-seed marker
  *and* HTTP liveness; `Harness::wait_until_ready` polls `/healthz/live`. Never a
  fixed sleep.
- **Idempotent seeding.** Account creation is skip-if-present; `seed.sh` clears
  its managed mailboxes before appending and CalDAV `PUT` is idempotent by URL,
  so a re-run converges. Start from a clean data volume for a guaranteed-identical
  slate (`docker compose down -v`); CI always starts fresh.
- **Secrets are throwaway** and committed on purpose — this server never holds
  real data. Do not wire it to the host trust store or real credentials.

## Running it

See `docker/stalwart/README.md` for the exact commands, host-port table, and
account list. In short: `docker compose up -d --wait` (self-bootstraps + seeds,
healthy when ready), export `STALWART_*`, and
`cargo test -p stalwart-harness --test smoke`.

## Gotchas observed against the live server

- **Bootstrap is JSON-via-API, not TOML.** A mounted `config.toml` is rejected
  (`--config` parses JSON `DataStore`). Configuration is the `x:Bootstrap/set` /
  `x:Account/set` / `x:NetworkListener/*` registry methods (the `x:` prefix marks
  registry methods; first-class objects like `Principal` are unprefixed).
- **`SEARCH HEADER Message-ID` does not match** in Stalwart; the seeder uses
  append-order sequence numbers and the smoke suite searches by `SUBJECT`.
- **The JMAP session is at `/jmap/session`** (`/.well-known/jmap` 307-redirects
  there; the probe does not follow redirects).
- **Docker Desktop (macOS) port proxy + `SO_RCVTIMEO`**: a socket read timeout was
  observed not to wake on arriving data through the userspace proxy, so the SMTP
  probe uses a blocking read (the server is health-gated first). Linux/CI uses
  direct NAT and is unaffected.

## What lands on top of this (steps 4–5)

The provider clients reuse this fixture and `crates/stalwart-harness` for
discovery + readiness, then add the deep suites: JMAP session/capability,
`Email/changes` with `Email/get` back-references, `Mailbox/changes`, state-cursor
persistence and `cannotCalculateChanges` resync, `Email/set` drafts and
`EmailSubmission/set`; IMAP `UIDVALIDITY`/`UID` identity and reset-triggered
rediscovery; CalDAV sync-token/CTag + ETag diffing and `If-Match` writes; and
iTIP/iMIP reconciliation. The seed already carries the data those suites need.
