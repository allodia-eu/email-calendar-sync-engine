# EAS (Exchange ActiveSync) Client Guidance

> **Protocol client landed; adapter pending.** The per-verb verdicts below come
> from the trait-shape spike (Plan B Task 3, 2026-08-24) and are stable. The
> relocation series has since brought the crate to engine quality — edition 2024,
> workspace lints, module split under the 500-line cap, the engine-tls transport,
> normalized live gating — but nothing in it implements `engine_provider` traits
> yet: everything marked *fork decision* or *P2* stays unimplemented until Plan
> C/P2 says otherwise. Update this file as the adapter lands — it is intended to
> become authoritative for `provider-eas`, peer of `imap-smtp.md` / `graph.md`.

This document covers the **Exchange ActiveSync 16.1 (negotiated down to 12.0) mail +
calendar + contacts provider** — the `provider-eas` crate imported from the Kylins
client (upstream commit `0dc611d`, engine import commit `f7db44d`) and since
retrofitted to engine standards: a standalone protocol client with no
`engine_provider` dependency yet. Read it alongside `providers.md` (the Provider
Contract), `store-and-sync.md` (scopes/cursors/apply), `tls.md` (trust policy), and
the spike findings in the Kylins repo
(`docs/superpowers/research/p0-eas-trait-spike.md`), which carries the full evidence
table behind every verdict here.

## The crate

- **`provider-eas`** — a hand-rolled EAS client over `reqwest` POSTs of WBXML
  documents, imported from Kylins and split to the engine's 500-line cap.
  Layers: `wbxml/` (codec: parser/serializer, code pages, token tables),
  `commands/` (per-command request builders + response parsers), `client/` (the
  transport: headers, retry layers, per-command typed methods), `status.rs`
  (status classification), `provision.rs`, `auth.rs`, `autodiscover/`, `types/`
  (wire-shaped request/result structs), plus `calendar/` / `calendar_write/` /
  `contacts/` (class-typed conversion) and `meeting_uid.rs` / `multipart.rs`.
- It is **not yet an adapter**: nothing in it implements `Provider`,
  `ContactsProvider`, or `Watch`. The spike's job was to decide whether it *can*
  without engine API changes. Answer: **one required engine change (new
  `SyncScope` variants), everything else maps within the existing trait surface.**

## Protocol overview

- **EAS 16.1** ([MS-ASHTTP] + family), negotiated down per server via the `OPTIONS`
  round: the server advertises `MS-ASProtocolVersions` / `MS-ASProtocolCommands`
  headers, and `pick_protocol_version` chooses the highest mutually known
  (`client/options.rs`, `options()` / `pick_protocol_version`). Wire format is WBXML
  (binary XML with per-namespace code pages) for every command body except
  SendMail's `<Mime>`, which rides as a WBXML OPAQUE blob so binary MIME survives.
- Every request is `POST <endpoint>?Cmd=<Command>&User=<user>&DeviceId=<id>`
  with `MS-ASProtocolVersion`, auth (Basic or OAuth bearer via `EasAuth`), and the
  device identity headers. HTTP 200 with an **empty body is the success shape**
  for the SendMail family; for Sync it means "no changes" and must echo the
  request's sync key.
- **Auth**: Basic (`DOMAIN\user`) or OAuth (`auth.rs`, token refresh on 401 is one
  of the transport retry layers). **Autodiscover** (`autodiscover.rs`) resolves the
  endpoint; an HTTP 451 `X-MS-Location` redirect is followed in-transport
  (hop-capped) and the adopted URL surfaces for the host to persist.

## Command surface the crate covers

| Command | Where | Purpose (engine verb it feeds) |
| --- | --- | --- |
| `OPTIONS` | `client/options.rs` | version/command negotiation at connect |
| `Provision` | `provision.rs`, `client/` retry layers | policy handshake; policy key rides `X-MS-PolicyKey` on every command |
| `Settings` (DeviceInformation / UserInformation / OOF / device password) | `client/settings.rs` | bootstrap facts, OOF (no engine verb yet) |
| `FolderSync` / `FolderCreate` / `FolderUpdate` / `FolderDelete` | `client/sync.rs`, `commands/folder_sync.rs`, `commands/folder_ops.rs` | container sync (`sync_mailboxes`) + folder writes |
| `Sync` (down) | `client/sync.rs::sync`, `commands/sync/` | per-collection item sync, classes Email/Calendar/Contacts (`stream_email`, `sync_events`, `sync_contacts`) |
| `Sync` (up: Commands Add/Change/Delete) | `client/sync.rs::sync_changes`, `commands/sync/` | client-side mutations (flags, calendar/contact writes) |
| `SendMail` / `SmartForward` / `SmartReply` | `client/compose.rs`, `commands/send.rs` | submission (`submit_email`); SmartForward degrades to SendMail on rejection |
| `MoveItems` | `client/items.rs::move_items` | per-message move (`MailEdit::MoveTo`) |
| `ItemOperations` (Fetch / EmptyFolderContents / Move-conversation) | `client/items.rs`, `commands/item_operations/` | body/MIME/attachment fetch (`fetch_message_source`), destructive extras (no engine verbs) |
| `MeetingResponse` | `client/items.rs`, `commands/meeting.rs` | invitation answer (`rsvp_event`) |
| `Ping` | `client/items.rs`, `commands/ping.rs` | push (`Watch`) |
| `GetItemEstimate` / `Search` / `ResolveRecipients` / `ValidateCert` | `client/items.rs` / `client/settings.rs` | counts, GAL/mailbox search, cert validation (no engine verbs yet) |

## Capability / quirk notes

- **OPTIONS version negotiation** happens at account setup; the chosen version is
  then a property of the client (it rides `MS-ASProtocolVersion` on every request).
  `ConnectionInfo` reports TLS/HTTP versions only; the EAS protocol version stays
  adapter-internal (hosts do not branch on it — `providers.md`).
- **Provision/policy**: servers may demand the two-phase Provision handshake at any
  time (HTTP 449, or in-body Common/FolderSync status 142–144). The transport
  retries once after re-provisioning (`client/transport.rs::send_command_ex`); `RemoteWipe`
  is surfaced as a permanent error, never executed.
- **Status classification** (`status.rs`): one `RecoveryAction` classifier per
  command family. Engine mapping: `ResetSyncKey` (Sync 3, FolderSync 9) →
  `ProviderError::needs_resync`; `RetryTransient` (Sync 5/16, HTTP 429/5xx) →
  `retryable`/`rate_limited`; `SurfaceAuth` → `authentication`; `RunFolderSync`
  (Sync 12, Ping 7) → needs-resync-or-internal-FolderSync; `RetryProvision` /
  `RefreshToken` / `FollowRedirect` are handled inside the transport and never
  surface. This is a clean 1:1 with `FailureClass`.
- **Sync key mechanics** (the load-bearing quirk set):
  - A collection sync key is an opaque server cursor; `"0"` bootstraps. The
    bootstrap round itself returns **no items** on some servers (Exchange 15.2) —
    the drain loop must follow the rotated key once (Kylins
    `eas_source/window.rs::should_follow_empty_bootstrap`).
  - Sync key invalid (status 3) = the EAS `cannotCalculateChanges`; FolderSync 9
    is the same for the hierarchy. Restart from `"0"` as a reconcile snapshot.
  - Each Sync round rotates the key, and `MoreAvailable` says whether more rounds
    remain — so **every round is a safe checkpoint** (EAS is more resumable
    mid-pass than JMAP/Graph, not less).
  - `WindowSize` (items per round) follows the Android ladder 10→512
    (`next_window_size`); ≤200 upsync commands per request are chunked and
    key-threaded automatically (`client/sync.rs::sync_changes`).
  - `FilterType` (days-back ladder: 1/3/5/7/14/30/45/90/180) is the only server
    window; a `SyncWindow::since` date maps to the smallest covering rung and the
    engine's `SyncWindow::admits` tightens the delta on apply — the composition
    `window.rs` already prescribes.
- **MoveItems' status table is inverted**: 3 is success (with `DstMsgId`), 1 is
  invalid source. A move re-keys the message (new ServerId), exactly like an IMAP
  move re-keys by UID — `MailEditReceipt::message_key` keeps the source key and
  the destination reconciles on the next sync.
- **ClientId dedup**: SendMail's `<ClientId>` (≤40 chars, enforced — Exchange 15.2
  rejects over-cap with in-body status 103) is derived from the draft's
  `Message-ID` so a lost-response retry does not double-send.

## TLS decision record (landed — P0-b Task 8)

The EAS transport builds on the engine's unified TLS policy, exactly like the
Graph/JMAP/CalDAV transports: `EasClient::new(config, &tls)` takes the host's
`engine_tls::TlsClientConfig` and constructs its `reqwest::Client` via
`tls.reqwest_builder()` (workspace `reqwest` 0.13 + preconfigured ring-backed
rustls), finished with the EAS-specific non-TLS settings only (the client-wide
120 s timeout and the User-Agent). The crate's crate-local
`reqwest = 0.12 + native-tls` declaration is gone, and `EasConfig`'s
`accept_invalid_certs` field with it — a per-provider invalid-cert escape is
not expressible on the shared-policy path by design.

**What a host passes in**: one `TlsClientConfig` per account, built once from
its `TlsPolicy` via `engine_tls::client_config(&policy)?` and shared (a cheap
`Arc` clone) with the account's other providers — the same wiring as
`docs/agent-guidance/tls.md`'s "How each provider consumes it". The
autodiscover flow's `http` client is the caller's `reqwest::Client` and must
come from the same `reqwest_builder()`.

The historical motivation for native-tls in the Kylins client — **enterprise
root CAs and intercepting proxies** — remains expressible through the policy's
opt-ins (`crates/engine-tls/src/policy.rs`): `TlsPolicy::bundled_and_system()`
picks up OS/enterprise roots with zero configuration, `TlsPolicy::pinned`
trusts exactly a private CA, and `TlsPolicy::PlatformVerifier` delegates
wholly to the OS verifier (MDM-installed roots on Android). The old
`accept_invalid_certs` lab-server escape becomes "pin the lab CA as a custom
root"; the crate's own gated live tests use the test-builds-only
`TlsClientConfig::dangerous_accept_any()` (`EAS_LIVE_INSECURE`), which
never compiles into a production build.

Two notes on the switched transport. (1) The shared builder advertises ALPN
`h2` then `http/1.1`, so EAS may now ride HTTP/2 where the server offers it;
the transport's explicit `Connection: keep-alive` header is illegal in h2 and
hyper strips it there (harmless; EAS semantics don't depend on it). (2) The
EAS retry layers are unchanged: 449→Provision, 401→OAuth refresh, 451→
`X-MS-Location` adoption, in-body status retries, and the transport's own
`Retry-After` parsing into `EasError::HttpStatus`. `engine_http::send_retrying`
(spike option) was judged not to fit: EAS already promotes 429/503 windows
through `HttpStatus.retry_after`, and the engine's 60 s poll loop is the retry
for transient failures — revisit only if a live deployment shows a gap.

## Live testing (env-gated)

The live suite (`tests/live_eas.rs` + the split modules under `tests/live_eas/`)
runs against the shared O365/Exchange test account,
with the same skip-when-unset convention as the Google/Graph live suites
(`GOOGLE_ACCESS_TOKEN` / `GRAPH_ACCESS_TOKEN`), the gates named `EAS_LIVE_*`.
Gating is two-layered: every test carries `#[ignore = "live Exchange account
required"]`, and each additionally no-ops when a required gate is unset, so even
an explicit `--include-ignored` run without credentials skips cleanly (one
"live gates unset" line per test, exit 0).

| Gate | Required | Covers |
| --- | --- | --- |
| `EAS_LIVE_URL` | yes | the `Microsoft-Server-ActiveSync` endpoint |
| `EAS_LIVE_USER` | yes | mailbox address (the EAS `User` query param) |
| `EAS_LIVE_PASSWORD` | yes | the account password / app password |
| `EAS_LIVE_USERNAME` | no | Basic-auth identity when it differs from the mailbox address; unset → identity = `USER` |
| `EAS_LIVE_INSECURE` | no | set to `1` to trust the self-signed on-prem test server (the test-builds-only `dangerous_accept_any`, see the TLS record above) |

The exact invocation lives in the header of `tests/live_eas.rs`. The scaffold
exercises the Basic-auth path only (OAuth accounts need `EasConfig::auth`). Two
operational rules learned live: every test uses its own `DeviceId` (concurrent
Provision phase-1 handshakes from one device identity race server-side, status
135), and EAS `FolderSync` ServerIds are per-device-partnership (never compare
them across devices).

**Fixtures**: anything learned from a live run (a wire shape, a status quirk, a
version-specific behaviour) must be captured as a **scrubbed fixture** wired into
the offline suite — observed bytes with every identifier moved to a reserved
name (`example.com`/`.net`/`.org`, `.test`, `.local`), keeping the byte shape,
per AGENTS.md "Identifiers in fixtures and docs use reserved names" and the
fixture rules the sibling providers' `tests/fixtures/` record. There is no CI
harness (no live account in CI): the suite is an occasional drift check, and it
is excluded from the offline coverage metric like the other providers' live
tests.

## Per-verb mapping table (trait → EAS), with verdicts

Verdicts: **no gap** (maps inside the current trait), **EAS-local** (works, but the
EAS side must build it), **engine change needed (fork patch)** (the trait/core must
move first). Full evidence (file:line on both sides) lives in the Kylins spike
document; this is the summary.

| Trait verb | EAS mapping | Verdict |
| --- | --- | --- |
| `connection_info` | composed post-connect (OPTIONS + Provision + first FolderSync); caps static thereafter; `http_version` from the shared client; `concurrent_fetches` from a per-server ceiling | no gap |
| `mailbox_scope` / `email_scope` | FolderSync hierarchy scope + per-folder Sync scope | **engine change (fork patch)**: new `SyncScope::EasFolderList` / `EasFolder` (+ calendar/contacts siblings), exactly as IMAP (`ImapMailboxList`/`ImapMailbox`) and Graph (`GraphFolderList`/`GraphFolder`) did |
| `sync_mailboxes` | `FolderSync`; hierarchy sync key is the cursor; status 9 → `needs_resync` → bootstrap `"0"` snapshot | no gap (JMAP `container_sync` is the code precedent) |
| `stream_email` | Sync class Email; cursor = collection sync key (`None` → `"0"`); per-round chunks are `Additive` with `advance_to` = rotated key; status 3 restarts the pass in `Reconcile` mode | no gap (`PassMode::Reconcile` matches SyncKey invalidation cleanly — JMAP `cannotCalculateChanges` recovery is the precedent) |
| `default_sync_window` / `SyncWindow` | `FilterType` day-ladder (coarse) + `admits` tighten | no gap (mapping note) |
| `fetch_message_source` | `ItemOperations` Fetch with `MIMESupport=2` + `BodyPreference` type 4 (raw MIME); multipart opt-in | no gap in contract; **EAS-local TODO**: `Options>Range` pagination/reassembly for oversized items is not implemented in the crate yet (P2; adapter-internal loop) |
| `submit_email` | `SendMail` with `save_to_sent`; empty-body success; `SubmissionReceipt` via the Graph `sent:<Message-ID>` placeholder-key precedent (EAS returns no id) | no gap |
| `file_sent_copy` | rejecting default — the server files the copy (`SaveInSentItems`), `Unfiled` never occurs | no gap |
| `edit_mail` | `SetKeywords` → Sync `Change` upsync (`read`/`starred` only — refuse other keywords); `MoveTo` → `MoveItems`; `Delete` → EAS has no per-item hard delete: either refuse or degrade to a Deleted-Items move (Kylins degrades; decide and document) | no gap (quirk decisions EAS-local) |
| `Watch` (Ping) | per-folder `EasPingWatcher`: status 2 → `Changed`, status 1 → `KeepAlive`; heartbeat self-tuning (300→900 s band, server-override) is adapter-internal and re-issued transparently | no gap for correctness; two *optional* fork records below |
| `calendar_scope`/`event_scope`, `sync_calendars`, `sync_events` | FolderSync class-discovered calendar folders; Sync class `Calendar` (`calendar_added`/`updated` + shared deletes) | rides the same fork patch for scopes; otherwise no gap |
| `create_event` / `patch_event` | Sync `Add` with ClientId (server returns ServerId — the only id-reveal point) / Sync `Change` with `Supported`-element ghosting; `WriteGuard::Absent`, empty `RevisionTokens` | no gap (expressible; P2 implements) |
| `put_event` | rejecting default — EAS's update verb is a field-level Change, not a document PUT (trait explicitly allows this) | no gap |
| `rsvp_event` | `MeetingResponse` (collection + request id of the *invite email*, `user_response`, `instance_id` for one occurrence, `send_response` ↔ notify); `RsvpControls { comment: false, suppress_notification: true }`; the invite-email reference travels in `Event`'s extended properties | no gap (expressible; P2) |
| `delete_event` | Sync `Delete`; occurrence = exception insertion via Change; already-gone = success | no gap (expressible; P2) |
| `ContactsProvider` (`sync_address_books`…`delete_contact`) | FolderSync contacts folders + Sync class Contacts; `Add`/`Change`/`Delete` with ghosting; `WriteGuard::Absent` | rides the scope fork patch; otherwise no gap (P2) |
| `fetch_contact_photo` | EAS pictures are **in-band** (`Picture` inside ApplicationData; currently dropped at parse, presence-only) — retain the bytes (P2) and serve them; `ContactPhoto` fits (fingerprint = sync-key revision) | EAS-local TODO; no engine gap |

## Fork-decision records (engine repo)

These do **not** become upstream issues — the engine is our fork; they are patches
we may make here, judged against the cross-provider survey (see the spike document
for the full table):

1. **[Required] EAS `SyncScope` variants** — add the EAS scope family to
   `engine-core` (`EasFolderList`/`EasFolder`, calendar and contacts siblings) and
   override the `*_scope` trait methods. Survey: every non-JMAP provider
   (IMAP, Graph, Gmail, CalDAV/CardDAV) added its own variants; EAS is the sixth
   and the pattern is mechanical. Estimated S (≤1 day with tests).
2. **[Optional, deferred] `Provider::watch()` accessor** — there is no trait seam
   to obtain a `Watch` today; IMAP and JMAP hand out concrete watcher types and the
   host switches on provider kind to build one. EAS would be the third such. A
   `fn watch(&self, scope) -> Option<Box<dyn Watch>>` defaulting to `None` would
   fix the switch-on-kind leak for all three; not needed for Plan C.
3. **[Optional, deferred] scope-tagged `WatchEvent`** — EAS `Ping` monitors many
   collections on one connection and names which changed; `Watch` is one-session-
   one-scope, so per-folder watchers multiply long-poll connections (correct but
   wasteful). `WatchEvent` is `#[non_exhaustive]` and its docs anticipate a
   scope-tagged variant. Defer until profiling shows the extra connections hurt.

## EAS-local work items (crate/adapter side, no engine involvement)

- ~~Adopt `engine-tls`/`TlsClientConfig` (Task 8) and drop `accept_invalid_certs`.~~ — landed (see the TLS decision record above).
- `Options>Range` pagination for ItemOperations Fetch (large attachments).
- Retain contact `Picture` bytes (currently presence-only) for
  `fetch_contact_photo`.
- Keyword-vocabulary policy for `MailEdit::SetKeywords` (express `read` +
  `flagged`; refuse the rest, IMAP-`PERMANENTFLAGS` style).
- `MailEdit::Delete` policy: refuse vs degrade-to-Trash-move (Kylins degrades
  today); whichever is chosen, document it in this file.
- Heartbeat persistence across restarts (constructor parameter or re-tune per
  session; the trait offers no persistence seam, and none is needed).
