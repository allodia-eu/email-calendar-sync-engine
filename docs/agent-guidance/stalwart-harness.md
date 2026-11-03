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
> calendar seed** (`seed/calendar/`), so one dataset validates both servers. A third,
> **Nextcloud** (`docker/nextcloud/`), imports the same seed. The determinism,
> gating-by-env-var, and excluded-from-offline-coverage conventions below apply to both.
>
> The two are **not interchangeable as evidence**, and `caldav.md` ("Which server
> proves what") is authoritative for the split. Two facts about *this* harness drive
> it: Stalwart **reserializes** the iCalendar it stores (it keeps every property but
> re-folds lines and reorders `RRULE` parts), so a write test may never assert that the
> server hands back the bytes it was sent; and Alice **owns every calendar she can
> see**, so this fixture cannot produce a collection she may not write — the read-only
> `DAV:current-user-privilege-set` case is SabreDAV's to prove, and it seeds a shared
> calendar for exactly that.
>
> That second fact is about **calendars**, and stays true. It is no longer true of her
> *mail*: the shared-mailbox fixture below gives her one store she holds every right on and
> one she may only read, which is what makes the read-only `MailboxAccess` path provable here.

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

## How it bootstraps (Stalwart v0.16 onward, v1 included)

Stalwart **v0.16 replaced the old declarative-TOML configuration with a
registry/bootstrap model**: the `--config` file is a small JSON pointer to the
data store, and everything else (listeners, directory, principals) lives in the
store, configured through a JMAP management API. A fresh server boots into
"bootstrap mode" (a recovery HTTP listener on 8080) and expects setup to be
completed via that API, after which it must restart to come up as a full server.
The older TOML approach the secondary docs describe applies to ≤0.15 only. v1 keeps
the model and the method names and renames fields (its `SCHEMA_CHANGES.md` in
`stalwartlabs/stalwart-migrate` lists every one); two of them are fields the entrypoint
touches — see "Gotchas".

`entrypoint.sh` drives this deterministically, as PID 1 of the single container:

1. Start the server. A fixed fallback admin (`STALWART_RECOVERY_ADMIN`) gives the
   management API a known credential from first boot — no random password, no
   wizard.
2. If in bootstrap mode, `x:Bootstrap/set` completes setup with an internal
   directory and **no ACME / auto-TLS request** (so boot stays offline and
   self-contained), then the server is restarted into a full server. Bootstrap mode
   is recognised by `x:Bootstrap/get` *listing* the singleton (`"id":"singleton"`);
   a full server answers it under `notFound`. On v1 the bootstrap also creates a real
   `admin@test.local` account with a random password, which the harness never uses —
   every management call authenticates as the recovery admin.
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
  attributable. Currently pinned: **v1.0.2**. What a bump owes:

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
     investigation hoping. Be equally careful about what you attribute to the server in the
     first place: the "JMAP RSVP scheduling gap" this line used to cite (#93) was never
     Stalwart's — `provider-jmap` had simply never sent `sendSchedulingMessages`, and two
     version bumps were taken partly in the hope of fixing a defect that did not exist
     (#102).

  **The v0.16.24 → v1.0.2 bump** (through v0.16.25–27 and v1.0.0–1). v1 is a major release
  with a new database layout and a renamed settings schema; neither touches the harness beyond
  two field renames, because CI always bootstraps from an empty volume (no migration). Every
  release note was read, and the JMAP Calendars and JSCalendar drafts v1 moved to (-33 and
  jscalendarbis-22, from -28 and -19) were diffed in full against what `provider-jmap` sends
  and reads. With the changes below, all 1455 tests in the gated Stalwart suites (smoke,
  `provider-jmap`, `provider-imap`, `provider-caldav`, `dav-cli`) passed from a wiped volume,
  and a warm restart re-seeded without re-bootstrapping. What changed for this repo:

  - **The entrypoint**: `tlsImplicit` → `implicitTls` in the listener it creates, and
    `serverHostname` → `defaultHostname`, which it never sends but its bootstrap-mode check
    grepped for (see "Gotchas" — that one failed silently). The check now asks for the
    singleton by id and stops on any answer that is neither the object nor `notFound`.
  - **A fully read-only share now says so on its account.** Bob's share reports
    `isReadOnly: true` and lists no `submission`, so a client bound to it offers neither
    writes nor sending; the all-writable group share is unchanged. The two tests built as
    tripwires for exactly this (`smoke.rs`, `live_shared.rs`) fired, and now pin both
    directions; `session_shared_accounts.json` was re-captured. The per-folder rights design
    stands: a share mixing writable and read-only folders still says `false` (`modeling.md`).
  - **JSCalendar `version` and `organizerCalendarAddress`.** v1 returns `"version": "2.0"` on
    every event and now infers `organizerCalendarAddress` on a create that names an owner
    participant (`jmap.md`). The adapter already sent both, so nothing it does changed.
  - **Limits it now states**: `maxMailboxesPerEmail` 100, `maxCalendarsPerEvent` 10,
    `maxAddressBooksPerCard` 10 (all `null` before). Nothing the engine does approaches them.

  Read and found not to touch a request the engine sends: the slimmer `*/set` create
  responses (v1.0.1; the adapter reads only `id`), `onSuccessUpdateEmail` by submission id
  (v0.16.26; the adapter uses a creation reference), inline-attachment placement in
  `Email/set` (v1.0.2; the adapter sends its own `multipart/related`), the `EmailDelivery`
  and `EmailPush` fixes (the watcher reads `Email` state), `SEARCH RETURN MIN/MAX`, the
  untagged `NO` to a rejected app-password login (v0.16.25; no live test logs in with a wrong
  credential), and the iTIP `PRIORITY`/`SEQUENCE` fixes (the CalDAV RSVP edits the stored
  object in place). New in v1 and not used by the engine: JMAP and IMAP `METADATA`, indexing
  of attachment contents (a `text` search may now match more; the engine passes the query
  through), OAuth scopes, OpenPGP/S/MIME encryption.

  **Found while testing, and not caused by the bump** — each reproduced on v0.16.24 with the
  same procedure, so neither was fixed in the bump; both were fixed straight after it:

  - Stalwart answers a **DAV** request over its concurrency cap with `400` and JMAP's
    `urn:ietf:params:jmap:error:limit` / `maxConcurrentRequests` body (5 of 144 `PROPFIND`s
    in 24-wide bursts on v0.16.24; on v1.0.2, 2 of 16 simultaneous `PUT`s, and 34 of 48).
    `provider-caldav` classed that `400` `Permanent`, so `live_caldav`'s 22 concurrent tests,
    each its own client, met it now and then as a flake. It now waits the refusal out
    (`DavThrottles`; `live_concurrency_refusal.rs` proves it), and `http-throttling.md` states
    the ceiling (#332).
  - `/healthz/live` counts against `Http.rateLimitAnonymous` (100/minute on both pins), and
    every live test polls it through `Harness::wait_until_ready`. Back-to-back runs of one
    suite exhausted it and the poll timed out after 30 s ("harness ready: Timeout(30s)") — 4 of
    5 consecutive `live_caldav` runs on v0.16.24. A single CI run stays under it. The
    entrypoint now raises the limit ("And so is the anonymous HTTP limit", below; #333).

  **The v0.16.21 → v0.16.24 bump.** The whole gated suite passed unchanged from a wiped
  volume. One fix lands on a request the engine sends: before v0.16.24, an `Email/set` whose
  patch carries a full `mailboxIds` equal to the current membership together with a keyword
  change left the message at IMAP UID 0. That is the JMAP report patch whenever the message
  already sits in the destination; reproduced on v0.16.22 and written up under `jmap.md` →
  reporting. No live test covers it, because the suite never reports a message already in its
  destination. Nothing else in v0.16.22–v0.16.24 touches a request the engine sends: it uses
  no `PushSubscription`, no `Email/import`, and reads neither `useDefaultAlerts` nor
  `baseEventId`.

  **The v0.16.15 → v0.16.21 bump, recorded so it is not re-investigated.** All 754 gated
  tests passed unchanged on both pins, so nothing the engine sends today behaves
  differently. Two observed changes near this repo, neither of which the suite can see:

  - `Calendar/get` with `properties` omitted now returns `isVisible`,
    `includeInAvailability`, `shareWith` and the two `defaultAlerts*` maps as well
    (`AddressBook/get` gains `shareWith`) — the v0.16.21 "return every property" fix. The
    neutral `Calendar` already has an `is_visible` field that **no** provider populates, so
    this is newly-available data rather than a defect; tracked separately, because filling it
    is a cross-provider question, not a JMAP one.
  - A `CalendarEvent/set` carrying JSCalendar `version` went from `invalidProperties` to
    accepted-and-silently-dropped. The "do not send it" conclusion is unchanged (`jmap.md`).

  What it did **not** change: JMAP still exposes no per-object write precondition, still has
  no JSCalendar version negotiation, and still cannot send an iMIP scheduling message from
  the adapter (#105). The one thing it made *worse* for a host is written up under
  `jmap.md` → scheduling: asking for scheduling on an account that cannot schedule is now a
  hard `forbidden` instead of a silent no-op.
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
| `bob@test.local` | nothing | scratch: the SMTP recipient in the submission tests, the **organizer** in the scheduling suite, and the **grantor** of the read-only share below |
| `carol@test.local` | nothing | scratch: the **attendee** in the scheduling suite |
| `support@test.local` | `12-shared.eml` in its INBOX | a **group** principal — a mailbox with no credentials of its own, which Alice belongs to |

`Harness::scratch` exposes the two scratch accounts as a pair. They exist because
some server behaviour cannot be observed without writing to a mailbox: RFC 6638
auto-scheduling has the server **mail** the attendee an invitation *and* the
organizer the reply, and that mail cannot be switched off. Running such an
exchange through Alice would push her INBOX permanently over the exact count the
mail suites assert, so the whole two-party exchange happens between Bob and Carol,
where nothing counts what is delivered. Prefer a scratch account over relaxing an
assertion on the seeded one.

### The shared-mailbox fixture: two shares, deliberately unequal

Alice can open two stores besides her own, and they differ in **rights**, because that
contrast is the point:

- **`support@test.local`** is a `Group` principal (`x:Account/set` with `"@type":"Group"`, and
  Alice added through `memberGroupIds` on *her* principal) — the vendor-neutral analogue of a
  Microsoft 365 shared mailbox. Alice holds **every** right on its folders (`rliteswkxpa`), and
  its INBOX holds one seeded message found in none of hers.
- **Bob grants Alice `lr`** on his INBOX, with an IMAP `SETACL` in `seed.sh` — nobody but its
  owner can. Alice holds **read** alone. Bob's INBOX takes the scheduling suite's mail, so it is
  asserted on only for its **rights**, never its contents.

Both appear in Alice's JMAP session as `accounts` entries with `isPersonal: false` (and give her
JMAP account a second `Identity`, the group's, listed **first**), and in her IMAP `LIST` under
the `Shared Folders` prefix her `NAMESPACE` puts in RFC 2342's *Other Users'* position. The
finding the fixture pins, in both directions: since v1 Stalwart reports `isReadOnly: true` for
Bob's account, whose one mailbox grants read alone, and `false` for the group's (v0.16 said
`false` for **both**, so nothing above the folder told them apart). v1 also lists no
`submission` on Bob's account. The flag is still no substitute for per-folder rights: a share
mixing a writable and a read-only folder says `false` (observed by hand on v1.0.2; no fixture
carries that shape). The smoke suite and `provider-jmap`'s `live_shared.rs` assert both values,
so if Stalwart changes its mind again the rationale in `modeling.md` is revisited rather than
silently outliving its evidence.

`MYRIGHTS` on the `\Noselect` containers (`Shared Folders`, `Shared Folders/support@test.local`)
answers `NO Mailbox does not exist.` — they are path components, not mailboxes — and pipelined
`MYRIGHTS` complete out of order (`imap-smtp.md`). The Dovecot harness carries the same two
shares with the same owners, so the IMAP contract runs against both (`docker/dovecot/README.md`).

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

### And so is the anonymous HTTP limit

The entrypoint also raises `x:Http`'s **`rateLimitAnonymous`** (100 requests/minute per client
IP as shipped) and logs `relaxed the anonymous HTTP rate limit`. Every live run spends it, in two
ways measured on v1.0.2:

- the readiness poll — `Harness::wait_until_ready`, one `GET /healthz/live` per test;
- the first request of every DAV connection, which `provider-caldav` sends without the
  credential until the server asks for it (`CredentialScope`, `caldav.md`). Stalwart answers it
  `401`, and that counts: 60 of them left 40 of the 100. Authenticated requests do not.

One `live_caldav` run fits in the budget; back-to-back runs did not, and once it ran out every
poll drew `429` (`Retry-After` up to a minute) until the wait timed out with
`harness ready: Timeout(30s)` — 4 of 5 consecutive runs on v0.16.24 (#333). Polling once per test
binary would only have moved the failure to the DAV connects, so the fix is the limit. Verified:
five back-to-back `live_caldav` runs, where the fourth used to fail. `rateLimitAuthenticated`
(1000/minute) is left as shipped. The smoke test `anonymous_requests_are_not_rationed` sends 150
unauthenticated requests and fails on a harness started before the change, so a stale container
says so rather than flaking.

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
| `08-jmap-state.eml`      | `JmapState` (**APPEND**ed)   | Its own object to move between `JmapState` and `JmapStateArchive` — the shape an `Email/changes` `updated` id takes. |
| `09-jmap-sent.eml`       | `JmapSent` (**APPEND**ed)    | Its own object, and a recipient address used nowhere else in the seed, so the sent-observation assertion cannot be satisfied by another fixture. |
| `10-report-junk.eml`     | `Reported` (**APPEND**ed)    | The junk/not-junk report tests' own message, on both the JMAP and IMAP side. |
| `11-report-phishing.eml` | `Reported` (**APPEND**ed)    | A **second** message so the phishing test starts from one carrying neither `$junk` nor `$phishing`. |
| `12-shared.eml`          | the **`support@` group mailbox's** INBOX | A shared store's sync returns *its* mail and not the credential's: this message is in none of Alice's folders. The group account collects a Sent copy per send-as-group run, so it is asserted on by `Message-ID`, never by count. |

Folders `Archive` and `Projects` exercise non-INBOX mailboxes, and `Archive/Nested` plus
`Archive/Nested/Deeper` make the list a **tree**: two levels, because one proves only that a
name was split at its delimiter while the name a folder pane shows is the last segment of the
whole path. Both are empty and both are declared on the Dovecot harness too, so the same
assertion runs on every configured server. Beyond them, every **mutating** live test gets a
dedicated mailbox, so nothing it does can disturb a count-asserted folder (`live_imap.rs`
asserts eight messages in INBOX):

| Mailbox | Contents | The test that owns it |
| --- | --- | --- |
| `QResync` | three **COPY**s of INBOX fixtures | the CONDSTORE/QRESYNC delta test — it re-flags one and expunges another |
| `Idle` | one **COPY** | the IMAP `IDLE` push test, which flag-toggles it on a second connection (`imap-smtp.md`) |
| `JmapState` + `JmapStateArchive` | `08-jmap-state.eml` | the JMAP state-delta test, which moves it between the two |
| `JmapSent` | `09-jmap-sent.eml` | the sent-observation test, which moves it into the real Sent collection and back |
| `Reported` | `10-report-junk.eml`, `11-report-phishing.eml` | the JMAP **and** IMAP report suites — one message each |

Two rules that produced this shape, both learned by breaking them:

- **APPEND, never COPY, when the test will *move* the message.** JMAP models a message in
  several mailboxes as one `Email` with several `mailboxIds`, so an IMAP `COPY` inside one
  account adds a mailbox to the existing object rather than minting a second one. Moving
  "the copy" replaces the shared baseline's `mailboxIds` and drags it out of INBOX and
  Archive.
- **One message per test, not one per suite.** Cargo runs the tests in a binary
  concurrently and neither CI job passes `--test-threads=1`, so two tests sharing a
  message read each other's writes. `Reported` holds two for exactly this reason — the
  report suite's phishing test failed on the runner, and only on the runner, while both
  tests passed locally under `--test-threads=1`.

### Calendar (`seed/calendar/`, via CalDAV into Alice's `default` and `work` calendars)

| Fixture                  | Invariant it protects |
| ------------------------ | --------------------- |
| `one-off.ics`            | A single zoned event with an embedded `VTIMEZONE`; every event materializes occurrences. |
| `recurring-weekly.ics`   | `RRULE` + `EXDATE` (excluded instance) + `RECURRENCE-ID` (a moved instance): recurrence with exceptions/overrides. |
| `meeting-attendees.ics`  | `ORGANIZER` + `ATTENDEE` with `ROLE`/`PARTSTAT`/`RSVP`: participants (and the inbound iTIP/iMIP target later). Shaped as a Google invitation: `X-GOOGLE-CONFERENCE` and Google's meeting block in the description, read as one detected conference and notes without the block. |
| `virtual-location.ics`   | RFC 7986 `CONFERENCE`: a join link and two dial-in numbers, one with a PIN after its pause, read as one conference. |
| `all-day.ics`            | `DTSTART;VALUE=DATE`: zoneless, no DST, never zone-attached. |
| `floating.ics`           | Floating wall-clock time (no `TZID`, no `Z`): resolves through the observer's zone. |

A **second calendar**, `work` (display name *Work*), is created with `MKCALENDAR` and holds
`seed/calendar/work/`. Every account a calendar suite reads therefore has more than one
calendar, so one provider serving them all, each under a scope of its own, is exercised on
every run rather than assumed. SabreDAV and Nextcloud seed the same pair.

| Fixture                  | Invariant it protects |
| ------------------------ | --------------------- |
| `work/planning.ics`      | A zoned event in the second calendar: it syncs under that calendar's scope and names that calendar, never the first. |
| `work/offsite.ics`       | An all-day event in the second calendar, so the pair is not one shape twice. |

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
  fixed absolute 2026 dates so occurrence expansion is stable — with one deliberate
  class of exception. **Every scheduling suite** dates its invitation a fixed offset from
  *today* (`invitation_date`, in both `provider-caldav/tests/scheduling/mod.rs` and
  `provider-jmap/tests/scheduling/mod.rs`), because auto-scheduling is the server choosing
  to deliver an iTIP message and Stalwart does not deliver one for a meeting that has
  already finished. A fixed date there buys no determinism; it sets a timer. It has now
  fired twice: the CalDAV suite passed every run until 11:00 on its own `DTSTART` day and
  then failed eight tests on every run after, and the JMAP suite — written before that fix
  and never swept into it — did the same two days later with all four of its scenarios.
  Both read like a broken server. Fix a date when the *client* is what interprets it;
  derive it from today when the *server's* behaviour turns on whether it has passed, and
  when you fix one suite's fixture, grep the others for the same shape.
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
- **v1 renamed two fields the entrypoint uses, and they fail differently.**
  `NetworkListener.tlsImplicit` became `implicitTls`, and the old name is refused
  loudly (`invalidPatch`, "Invalid key for object"). `Bootstrap.serverHostname` became
  `defaultHostname`, and that one failed *silently*: the bootstrap-mode check grepped for
  the field's name, so a fresh v1 server read as already bootstrapped, setup was skipped,
  and the entrypoint died later on "could not resolve default domain id". The check now
  matches the singleton's id, which no rename can move. Singleton settings objects
  (`x:Http`, `x:Jmap`, …) answer `ids: null` with an empty list (observed on v1.0.2);
  ask for `ids: ["singleton"]`.
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
