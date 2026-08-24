# EAS (Exchange ActiveSync) Client Guidance

> **Spike-time draft.** This skeleton was drafted from the trait-shape spike (Plan B
> Task 3, 2026-08-24) before any `engine_provider` work on `provider-eas` landed. The
> protocol facts and per-verb verdicts below are read from the imported crate and are
> stable; everything marked *fork decision* or *P2* is unimplemented until Plan C/P2
> says otherwise. Update this file as the adapter lands — it is intended to become
> authoritative for `provider-eas`, peer of `imap-smtp.md` / `graph.md`.

This document covers the **Exchange ActiveSync 16.1 (negotiated down to 12.0) mail +
calendar + contacts provider** — the `provider-eas` crate imported from the Kylins
client at `f7db44d` ("pre-engineering": a standalone protocol client with no
`engine_provider` dependency yet). Read it alongside `providers.md` (the Provider
Contract), `store-and-sync.md` (scopes/cursors/apply), `tls.md` (trust policy), and
the spike findings in the Kylins repo
(`docs/superpowers/research/p0-eas-trait-spike.md`), which carries the full evidence
table behind every verdict here.

## The crate

- **`provider-eas`** — a hand-rolled EAS client over `reqwest` POSTs of WBXML
  documents, imported verbatim from Kylins (`crates/provider-eas/src/`, 34 files).
  Layers: `wbxml/` (codec: parser/serializer, code pages, token tables),
  `commands/` (per-command request builders + response parsers), `client.rs` (the
  transport: headers, retry layers, per-command typed methods), `status.rs` (status
  classification), `provision.rs`, `auth.rs`, `autodiscover.rs`, `types.rs`
  (wire-shaped request/result structs), plus `calendar.rs` / `calendar_write.rs` /
  `contacts.rs` (class-typed conversion) and `meeting_uid.rs` / `multipart.rs`.
- It is **not yet an adapter**: nothing in it implements `Provider`,
  `ContactsProvider`, or `Watch`. The spike's job was to decide whether it *can*
  without engine API changes. Answer: **one required engine change (new
  `SyncScope` variants), everything else maps within the existing trait surface.**

## Protocol overview

- **EAS 16.1** ([MS-ASHTTP] + family), negotiated down per server via the `OPTIONS`
  round: the server advertises `MS-ASProtocolVersions` / `MS-ASProtocolCommands`
  headers, and `pick_protocol_version` chooses the highest mutually known
  (`client.rs`, `options()` / `pick_protocol_version`). Wire format is WBXML
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
| `OPTIONS` | `client.rs` | version/command negotiation at connect |
| `Provision` | `provision.rs`, `client.rs` retry layer | policy handshake; policy key rides `X-MS-PolicyKey` on every command |
| `Settings` (DeviceInformation / UserInformation / OOF / device password) | `client.rs` | bootstrap facts, OOF (no engine verb yet) |
| `FolderSync` / `FolderCreate` / `FolderUpdate` / `FolderDelete` | `client.rs`, `commands/folder_sync.rs`, `commands/folder_ops.rs` | container sync (`sync_mailboxes`) + folder writes |
| `Sync` (down) | `client.rs::sync`, `commands/sync.rs` | per-collection item sync, classes Email/Calendar/Contacts (`stream_email`, `sync_events`, `sync_contacts`) |
| `Sync` (up: Commands Add/Change/Delete) | `client.rs::sync_changes`, `commands/sync.rs` | client-side mutations (flags, calendar/contact writes) |
| `SendMail` / `SmartForward` / `SmartReply` | `client.rs`, `commands/send.rs` | submission (`submit_email`); SmartForward degrades to SendMail on rejection |
| `MoveItems` | `client.rs::move_items` | per-message move (`MailEdit::MoveTo`) |
| `ItemOperations` (Fetch / EmptyFolderContents / Move-conversation) | `client.rs`, `commands/item_operations.rs` | body/MIME/attachment fetch (`fetch_message_source`), destructive extras (no engine verbs) |
| `MeetingResponse` | `client.rs`, `commands/meeting.rs` | invitation answer (`rsvp_event`) |
| `Ping` | `client.rs`, `commands/ping.rs` | push (`Watch`) |
| `GetItemEstimate` / `Search` / `ResolveRecipients` / `ValidateCert` | `client.rs` | counts, GAL/mailbox search, cert validation (no engine verbs yet) |

## Capability / quirk notes

- **OPTIONS version negotiation** happens at account setup; the chosen version is
  then a property of the client (it rides `MS-ASProtocolVersion` on every request).
  `ConnectionInfo` reports TLS/HTTP versions only; the EAS protocol version stays
  adapter-internal (hosts do not branch on it — `providers.md`).
- **Provision/policy**: servers may demand the two-phase Provision handshake at any
  time (HTTP 449, or in-body Common/FolderSync status 142–144). The transport
  retries once after re-provisioning (`client.rs::send_command_ex`); `RemoteWipe`
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
    key-threaded automatically (`client.rs::sync_changes`).
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

## TLS decision record (spike ruling; Task 8 implements)

The crate currently builds its own `reqwest::Client` with
`danger_accept_invalid_certs` from `EasConfig`. **Ruling: adopt the engine's
`engine-tls` + `reqwest`/rustls path** — construct from the host's
`TlsClientConfig` exactly like the Graph/JMAP/CalDAV transports, and take
`engine_http::send_retrying` for 429 waits if the retry posture fits.

The historical motivation for native-tls in the Kylins client — **enterprise root
CAs and intercepting proxies** — is *not* lost: `TlsPolicy::bundled_and_system()`
picks up OS/enterprise roots with zero configuration, `TlsPolicy::pinned` trusts
exactly a private CA, and `TlsPolicy::PlatformVerifier` delegates wholly to the OS
verifier (MDM-installed roots on Android). The debug-only
`accept_invalid_certs` escape becomes "pin the lab CA as a custom root", which is
stronger and non-footgunny. No engine change is needed — this is an
adapter-construction change plus a config migration.

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

- Adopt `engine-tls`/`TlsClientConfig` (Task 8) and drop `accept_invalid_certs`.
- `Options>Range` pagination for ItemOperations Fetch (large attachments).
- Retain contact `Picture` bytes (currently presence-only) for
  `fetch_contact_photo`.
- Keyword-vocabulary policy for `MailEdit::SetKeywords` (express `read` +
  `flagged`; refuse the rest, IMAP-`PERMANENTFLAGS` style).
- `MailEdit::Delete` policy: refuse vs degrade-to-Trash-move (Kylins degrades
  today); whichever is chosen, document it in this file.
- Heartbeat persistence across restarts (constructor parameter or re-tune per
  session; the trait offers no persistence seam, and none is needed).
