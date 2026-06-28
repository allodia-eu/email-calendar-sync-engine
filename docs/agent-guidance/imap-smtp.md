# IMAP/SMTP Client Guidance

This document is authoritative for the **IMAP (RFC 9051) read/sync + SMTP
(RFC 5321) submission provider** — the mail half of build-order step 5
(`north-star.md`). It covers the `provider-imap` crate and the IMAP/SMTP
specifics it implements against the Stalwart fixture. Read it before touching
`provider-imap` (and the submission paths in `engine-provider`/`engine-sync`),
alongside `providers.md` (the Provider Contract), `store-and-sync.md` (the
apply/lease model and `SyncScope`), `jmap.md` (the precedent it mirrors), and
`stalwart-harness.md` (the fixture).

CalDAV/CardDAV is the **other** step-5 slice and is not covered here; `caldav.md`
is authoritative for the `provider-caldav` calendar client.

## The crate

- **`provider-imap`** — a hand-rolled minimal IMAP + SMTP client over a **generic
  async stream**, implementing `engine_provider::Provider`. No third-party
  IMAP/SMTP library: the SMTP per-recipient and post-`DATA` invariants stay under
  our control, and the whole protocol is offline-testable by replaying captured
  transcripts through an in-memory stream (mirroring `provider-jmap`'s `Executor`
  seam and the harness probe). TLS is pure-Rust `tokio-rustls`, with the host
  injecting trust policy — the library bakes in no root store, so mobile hosts and
  the self-signed fixture each supply their own.
- Layers: `transport` (connect + the tagged line protocol: `LOGIN`/`CAPABILITY`/
  `ENABLE`/`SELECT [(CONDSTORE)]`/`UID FETCH [(CHANGEDSINCE … VANISHED)]`/`LIST`/
  `CREATE`/`APPEND`, literal handling), `parse` (pure response parsers,
  panic-resistant on hostile input), `mail` (normalize rows → `Message`/`Mailbox`),
  `cursor` (the per-mailbox `SyncState` — `UIDVALIDITY`/`UIDNEXT` plus an optional
  QRESYNC `HIGHESTMODSEQ` — + opaque `PageToken` encodings), `sync` (snapshot/delta
  UID-window paging), `qresync` (the QRESYNC incremental delta — flag changes +
  expunges via `CHANGEDSINCE`/`VANISHED`), `idle`/`watch` (the `IDLE` push primitives +
  the `ImapWatcher`), `smtp` (the submission conversation +
  RFC 5322 assembly), `provider` (the `Provider` impl).

## How IMAP differs from JMAP (the shape)

- **Email scope is per mailbox.** JMAP has one account-global `Email` scope; IMAP
  state is per folder (`UIDVALIDITY`/`UIDNEXT`). So an `ImapProvider` is **bound to
  a single mailbox** for email: `email_scope` names that mailbox
  (`SyncScope::ImapMailbox{account, mailbox}`), and `sync_email_page` is a
  UID-window `FETCH` over it. The folder list syncs under the new per-account
  `SyncScope::ImapMailboxList{account}` (a container scope, applied before the
  email it parents — `store-and-sync.md` referential apply order). The cross-folder
  fan-out (enumerate folders, drive each) is the later orchestrator's job.
- **Identity is synthesized**: a mail object's key is `(mailbox, UIDVALIDITY, UID)`
  encoded `imap:v{validity}:u{uid}@{mailbox}` (injective — the numeric components
  are delimited). An IMAP **copy in another folder is a distinct object** with a
  single membership — the contrast to JMAP, where the same copy is one object with
  two `mailboxIds`. `Message-ID` is a hint, never identity.
- **A UIDVALIDITY reset is a snapshot.** When the server renumbers the UID space,
  every prior key is invalid; the next pass is a snapshot (rediscovery) that
  tombstones the stale rows — the IMAP analogue of JMAP `cannotCalculateChanges`.

## IMAP specifics implemented

- **Cursor + paging.** The cursor is `(UIDVALIDITY, UIDNEXT)` encoded
  `v{validity};n{next}`, with an optional QRESYNC `HIGHESTMODSEQ` appended as
  `;m{modseq}` when the session negotiated QRESYNC (a non-QRESYNC cursor is
  byte-identical to the old format, and a pre-QRESYNC cursor with no `;m` decodes with
  `highest_modseq: None`); a foreign/garbage cursor decodes to "no cursor" → snapshot.
  Paging is **newest UIDs first, up to `limit` *messages* per page**: a page fetches
  a UID window and, if a gap (expunged UID) leaves it under-filled, **widens the
  window downward** until it has `limit` messages (or reaches the floor) — so
  `limit` is a count of messages, not a span of UID slots. Any older overshoot is
  capped off and re-fetched by the next page (whose window ends strictly below the
  lowest kept UID, so no duplication). The next boundary travels in the opaque
  `PageToken`. No `SEARCH` — windows are fetched directly, so expunged UIDs are
  simply absent (a gap), and a snapshot's accumulated `present` set is exactly the
  existing UIDs (tombstoning the rest). `limit` `0` means the whole remaining window
  in one page (the drain default).
- **Sync-depth window (optional).** A provider built with `ImapConfig::with_since(date)`
  bounds a **snapshot** to mail delivered on or after `date`: before paging, a single
  `UID SEARCH SINCE <dd-Mon-yyyy>` (`transport::uid_search_since`, parsed by
  `parse_search`, tolerating both classic `* SEARCH` and extended `* ESEARCH … ALL`)
  yields the in-window UIDs, and the snapshot starts at the **lowest** of them (older
  mail is never fetched), reporting their count as the `total` progress denominator. No
  matches yields an empty snapshot that still tombstones stale rows below the window. A
  **delta** is already bounded to new arrivals, so the window never narrows it (no
  `SEARCH` is issued). With no cutoff (the default) the whole mailbox syncs, exactly as
  before. This is how a host implements "configurable sync depth" without an
  account-wide message delta — the cutoff is a host-supplied calendar date, so this
  crate stays free of any depth/duration policy.
- **Snapshot vs delta.** First sync (no cursor) or a UIDVALIDITY mismatch →
  **snapshot** (rediscover from UID 1, carry `present`). A matching cursor → **delta**.
  On a QRESYNC session with a prior `HIGHESTMODSEQ` baseline the delta is **incremental
  and complete** — flag changes *and* expunges of already-synced messages, plus new
  arrivals, in one round trip (see **CONDSTORE/QRESYNC** below). Without QRESYNC (or on
  the first delta after an upgrade, before a modseq baseline exists) the delta is
  **new arrivals only** (UIDs at or above the cursor's `UIDNEXT`) and carries **no
  removals**, so flag/expunge changes reconcile via a periodic snapshot — the honest
  baseline `providers.md` prescribes ("CONDSTORE/QRESYNC paths are optional
  capabilities, not assumptions").
- **CONDSTORE/QRESYNC incremental delta** (RFC 7162; `qresync` module). After login the
  client issues `CAPABILITY` (capabilities are advertised only post-auth) and, when the
  server lists `QRESYNC`, `ENABLE QRESYNC` — best-effort, so a server that lists it but
  rejects `ENABLE` stays on the baseline. On a QRESYNC session the sync layer opens the
  mailbox `SELECT … (CONDSTORE)` so the response carries `[HIGHESTMODSEQ n]`, recorded
  in the cursor. A delta with a prior baseline then issues a single
  `UID FETCH 1:* (<items>) (CHANGEDSINCE <modseq> VANISHED)`: every message whose
  mod-sequence exceeds the baseline returns with **full metadata** (new arrivals *and*
  flag-only changes, so the store upserts them by their stable key), and
  `* VANISHED (EARLIER) <set>` lists the UIDs expunged since the baseline, which become
  the page's `removed` keys (the store tombstones them inline — `store-and-sync.md`
  `Delta { changed, removed }`). The set is expanded per UID and bounded by a cap so a
  hostile range cannot exhaust memory. The pass is a single page (the changed set is
  bounded to what moved since the last sync); the new baseline is the SELECT-time
  `HIGHESTMODSEQ`. So a host "refresh" that must reflect server-side flag/move/delete
  changes no longer needs `Engine::clear_mail_cursors` against a QRESYNC server — a plain
  delta sync reconciles them.
- **Normalization.** `UID FETCH (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE
  BODY.PEEK[HEADER.FIELDS (REFERENCES)])` (all peek-safe — none sets `\Seen`). The
  `References` header is not an `ENVELOPE` field, so it rides a separate peek-safe
  body-header item to feed threading (`threading.md`). Flags → keywords: `\Seen`/`\Flagged`/
  `\Answered`/`\Draft` map to their `$`-keywords; `\Deleted`/`\Recent` are
  deliberately not keywords (expunge/session model); custom keywords pass through.
  `INTERNALDATE` → a UTC instant (offset applied). `ENVELOPE` → subject, flattened
  addresses, and the `Message-ID`/`In-Reply-To` hints (the body-header item adds
  `References`) — the threading inputs; **RFC 2047 encoded-words** in
  the subject and display names are decoded (`B`/`Q`, UTF-8/ISO-8859-1/Windows-1252 —
  `ISO-8859-1` is read as its CP1252 superset so a `0x96` en-dash is `–`, not `�`, the
  browser convention — with whitespace between adjacent words dropped — `encoded_word.rs`). A quoted string
  carrying **raw UTF-8** (a `UTF8=ACCEPT` mailbox name, or an unencoded display name)
  is decoded as UTF-8, not byte-cast to Latin-1 — the quoted and `{n}`-literal paths
  agree. Folder `LIST` →
  `Mailbox` with role from the `INBOX` name or a SPECIAL-USE attribute (RFC 6154;
  note a provider may tag its Archive folder `\All`, like Gmail's "All Mail" — the
  normalizer reflects the attribute faithfully). Raw MIME is **not materialized**
  (Tier-1 metadata, like step 4).

## SMTP submission

- **`submit_email`** runs the conversation `EHLO → [AUTH] → MAIL FROM → RCPT TO* →
  DATA`, then files the sent copy. The pre-generated `Message-ID` is on the message
  so the sent copy reconciles by it.
- **Message assembly (`assemble_message`)** is hardened against header injection:
  every interpolated value (`Message-ID`, addresses, subject, display names, and the
  `In-Reply-To`/`References` threading ids) is **rejected on CR/LF/NUL** (RFC 5322
  §2.2 / RFC 5321 §2.3.8 — otherwise a poisoned draft could inject headers or split
  the command stream), and a **non-ASCII subject or display name is emitted as an
  RFC 2047 `B` encoded-word**, never raw 8-bit bytes, so headers stay 7-bit clean.
  A **`Date` header is generated locally** (RFC 5322 §3.6 requires it; for an IMAP
  `APPEND` — `save_draft` / the Sent copy — no server is in the loop to add one).
  For a reply or forward it also emits the **threading linkage** (RFC 5322 §3.6.4):
  `In-Reply-To: <id>` when `Draft.in_reply_to` is set and `References: <id1> <id2> …`
  (space-separated, each angle-bracketed) when `Draft.references` is non-empty — each
  control-char-guarded like the other ids and omitted when its field is empty, so a
  sent reply threads with its original (`threading.md`). The body is normalized so a
  bare CR/LF never reaches the wire. Plain drafts emit `text/plain`; drafts with an
  HTML alternative emit `multipart/alternative`; CID-referenced inline attachments
  wrap the body in `multipart/related`; regular attachments wrap the result in
  `multipart/mixed`. Attachment header values are CR/LF/NUL guarded, binary
  attachment bodies are base64 encoded, and non-ASCII attachment filenames use
  RFC 5987-style `filename*` / `name*` parameters. (Long encoded-words are not yet
  folded into 75-octet runs — a later refinement.)
- **Folder resolution.** The sent copy / draft is filed into the account's **real
  folder for the role**, discovered via the `\Sent`/`\Drafts` SPECIAL-USE attribute
  in a `LIST` (so a Gmail `[Gmail]/Sent Mail` or a localized name is honored), and
  only when the server advertises none does it fall back to creating the
  conventional `Sent`/`Drafts` name. This costs one `LIST` per submission (rare path).
- **Two transports.** `ImapConfig::with_smtp(addr)` is **plaintext, no auth** — for
  an MX that accepts local mail (the fixture's port 25). `with_smtp_tls(addr,
  server_name)` is **implicit TLS + `AUTH PLAIN`** (port 465) using the account
  credentials and the injected connector — what a real provider needs. AUTH is only
  attempted over the TLS transport (never in the clear). **STARTTLS (port 587) is a
  later refinement**; implicit TLS covers the common case.
- **Per-recipient acceptance/rejection** is captured from each `RCPT TO` reply (a
  `250` accept, a `550` reject). The message still goes to the accepted recipients;
  if none accept, it is a permanent rejection with no `DATA`.
- **Post-`DATA` disposition.** `2xx` → delivered; `5xx` → permanent rejection;
  `4xx` → transient (retryable — the message was not queued); any **unreadable
  acknowledgement once the message bytes are on the wire** — a dropped connection
  *or* a malformed final reply — → **ambiguous** (never a plain transport error, so
  an already-sent message is never reported as a clean failure). The
  ambiguous case becomes `ProviderError::needs_confirmation`, which
  `engine_sync::submit_mail` routes to `PendingOutcome::NeedsConfirmation` rather
  than `Failed` — so the outbox never blind-retries and risks a double-send
  (`providers.md`). This is the one cross-crate touch the slice added:
  `engine-provider`'s `ProviderError` gained `needs_confirmation`/
  `requires_confirmation`, and `engine-sync`'s outbox honors it.
- **Sent placement is best-effort.** A successful send is never failed for a
  Sent-filing hiccup. With UIDPLUS the `APPEND` returns `[APPENDUID validity uid]`
  → the receipt carries the real Sent key (the same key the next Sent sync
  synthesizes); without it the receipt key is `Message-ID`-derived and the copy
  reconciles when Sent is synced.
- **`save_draft` (no SMTP).** `ImapProvider::save_draft` files a draft into the
  account's Drafts folder (resolved by `\Drafts` SPECIAL-USE, else creating
  `Drafts`), flagged `\Draft`, via `APPEND` — so creating a mail works against any
  IMAP server even where SMTP submission cannot. Unlike Sent placement it surfaces
  an `APPEND` failure (saving the draft is the whole op). The
  `examples/imap_explore.rs` example exercises read + (opt-in) `save_draft` against
  a real provider.

## Mail mutations

- **`edit_mail`** applies a provider-neutral `MailEdit` to the bound mailbox over the
  open session (`mutate.rs`; the `Provider` impl is a thin lock-and-call). The crate
  advertises `Capabilities::mail_writes` **unconditionally** — `UID STORE`/`MOVE`/
  `EXPUNGE` need no extra config, unlike submission which is gated on a configured SMTP.
- **`SetKeywords`** → `UID STORE +FLAGS.SILENT (...)` for the `add` set and
  `-FLAGS.SILENT (...)` for the `remove` set (one command per non-empty side; both
  empty is a no-op). The keyword↔flag mapping is `keyword_to_flag`, the inverse of
  the read path's `flags_to_keywords`: `$seen`/`$flagged`/`$answered`/`$draft` →
  `\Seen`/`\Flagged`/`\Answered`/`\Draft`, every other keyword (other system
  keywords, custom keywords) → a bare IMAP keyword atom. `.SILENT` suppresses the
  per-message `FETCH` echo, so no response parsing is needed.
- **`MoveTo`** → `UID MOVE <uid> "<dest>"` (RFC 6851), an atomic server-side move.
- **`Delete`** (permanent, not a Trash move) → `UID STORE +FLAGS.SILENT (\Deleted)`
  then `UID EXPUNGE <uid>` (UIDPLUS, RFC 4315 — only the named UID is expunged, so a
  concurrent `\Deleted` elsewhere is not collaterally removed).
- **UIDVALIDITY guard.** Every edit first `SELECT`s the target key's mailbox and
  checks the returned `UIDVALIDITY` against the key's. A mismatch means the UID space
  was renumbered and every prior key is stale, so the edit is a **`Conflict`** (the
  caller re-syncs, then retries) rather than a blind write against the wrong message.
  An unparseable target key is `InvalidState` (rejected before any command).

## Body fetch (Tier-3 source)

- **`fetch_message_source`** returns a message's whole raw RFC 5322 source over the
  open session (`fetch.rs`; the `Provider` impl is a thin lock-and-call, mirroring
  `mutate.rs`). The crate advertises `Capabilities::message_source` **unconditionally**
  — every IMAP session can fetch bodies.
- **`UID FETCH <uid> (BODY.PEEK[])`** fetches the entire message (headers + every
  part) as a single `{n}` literal, which the transport inlines; `parse_fetch_body`
  pulls the literal bytes out of the framing (`BODY[] {n}\r\n<n bytes>`) — and only
  from the line whose `UID` matches the request, so a piggybacked `FETCH` for another
  UID cannot supply the wrong message's bytes. `.PEEK` does **not** set `\Seen` —
  reading a body must not silently mark it read; the host marks-read via a separate
  `edit_mail` when it chooses. Fetching the whole source (not just the text part) is
  lossless and serves the body now and HTML/attachments later from the cached raw with
  no re-fetch (`providers.md`, `store-and-sync.md`).
- **Read-only open + shared guard.** Resolution is shared with the edit path via
  `target::select_target`: parse the key, reject a `CR`/`LF` mailbox (`InvalidState`),
  open, and guard `UIDVALIDITY` (mismatch → **`Conflict`**). A body read opens the
  mailbox with **`EXAMINE`** (read-only), not `SELECT`, so it takes no write-intent
  open, leaves `\Recent` untouched, and works on a read-only folder. A `UID FETCH`
  that returns no data — the UID was expunged since the last sync — is also a
  **`Conflict`** (re-sync, then drop), not a permanent failure.

## Push (IMAP IDLE, RFC 2177)

- **A watcher, not a sync.** `ImapWatcher` (the `watch` module, built on the `idle`
  transport primitives) turns IMAP `IDLE` into the provider-neutral
  `engine_provider::Watch` stream (`providers.md`): `next()` yields a `WatchEvent` —
  `Changed` (the mailbox changed) or `KeepAlive` (a re-`IDLE` heartbeat). A
  notification carries **no data** — `IDLE` only reports *that* `* n EXISTS` /
  `* n EXPUNGE` / `* n FETCH` / `* VANISHED` happened, never *what*. So the watcher
  never applies mail; a `Changed` means only "run the mailbox's normal sync," and the
  authoritative reconciliation is the existing CONDSTORE/QRESYNC delta (one round trip).
  This is what makes push bulletproof: a coalesced burst, a spurious wake, a missed
  notification, or a dropped connection cannot corrupt the store — the next sync makes
  it correct, because syncing a scope is idempotent. The host advertises `idle` from
  the post-auth `CAPABILITY` so it can offer an "as it comes in" strategy or fall back
  to polling.
- **A dedicated connection, gated on `IDLE`.** A watcher opens its **own** connection
  (the shared `connect_session` dial), separate from the `ImapProvider` that syncs the
  mailbox — a connection in `IDLE` can only send `DONE`, so it cannot also `FETCH`.
  Construction `EXAMINE`s the mailbox **read-only** (watching never writes or resets
  `\Recent`) and fails fast with `InvalidState` if the server does not advertise `IDLE`.
  One watcher watches one mailbox, mirroring the bound-mailbox sync model; the host
  decides which (and how many) mailboxes warrant a standing connection against the
  server's connection limit (usually just INBOX).
- **The notification gap, closed three ways.** `IDLE` delivers unsolicited responses
  *only while a connection is actively idling*, so a change arriving in any other window
  is never re-sent. The watcher closes this by (1) **staying in `IDLE` continuously**
  across `Changed` events — `next()` reports a change without leaving `IDLE`, so a
  message arriving while the host syncs the previous one on its *separate* connection is
  still captured; (2) the host's prescribed loop syncing **once on start and once after
  every reconnect**; and (3) the mandatory **~28-minute keep-alive re-`IDLE`** (under
  RFC 2177's 29-minute rule), which doubles as a liveness probe and a backstop sync
  trigger — and whose pre-re-`IDLE` `DONE` drain converts a boundary change into
  `Changed` rather than swallowing it. The keep-alive interval is the one host-supplied
  knob (a protocol timer, clamped to a sane range; default 28 min, shorter on mobile to
  detect a dead link sooner), not a product policy — **scheduling and reconnect/backoff
  live in the host**, not the engine.
- **Coverage.** The `idle` primitives (continuation handling, untagged-line
  classification, `DONE` drain) are unit-tested over scripted transcripts; the watcher's
  keep-alive timing and stay-idling-across-events behavior are tested over a real
  in-memory `tokio::io::duplex` with `start_paused` (the 28-minute timer fires
  instantly, deterministically). A gated live test (`tests/live_imap_idle.rs`,
  `STALWART_IMAP_ADDR`) watches the dedicated `Idle` seed mailbox and flag-toggles it on
  a second connection, asserting the watcher surfaces `Changed`. The `imap_explore`
  example's `IMAP_IDLE` opt-in watches a real account read-only.

## Known limitations (documented, not bugs)

- **CONDSTORE/QRESYNC fallback when unsupported.** The incremental delta (above) is
  **implemented** for servers that advertise QRESYNC (RFC 7162) — the common case
  (Stalwart, Dovecot, Cyrus, Gmail). A server that advertises **neither** QRESYNC nor a
  usable baseline falls back to the new-arrivals-only delta, where flag/expunge/move
  changes to already-synced messages still reconcile via a periodic **snapshot** forced
  with `Engine::clear_mail_cursors` (the targeted, mail-only counterpart of
  `Engine::reset`). A **CONDSTORE-only** server (CONDSTORE without QRESYNC) is treated as
  the non-incremental baseline too: we gate the delta on QRESYNC because the `VANISHED`
  expunge half needs it, and a half-incremental path that detects flag changes but
  silently misses expunges would be a worse, more confusing state than the honest
  snapshot fallback. Wiring CONDSTORE-only flag deltas is a possible later refinement.
- **QRESYNC delta is a single page and not sync-depth-windowed.** The QRESYNC delta
  issues one `UID FETCH 1:* (CHANGEDSINCE … VANISHED)`: it does **not** honor the
  `limit`/paging the snapshot path uses (a bulk server-side change — "mark all read" —
  returns every changed message in one response and one transaction; per-page streaming
  of the delta is a later refinement), and it does **not** re-apply the optional
  sync-depth window (`ImapConfig::with_since`, currently provider-only and not
  host-wired), so a flag change to an *out-of-window* message can re-enter the store.
  Bounding the delta — correctly, since `VANISHED` needs `1:*` to report already-expunged
  UIDs while a window must restrict only `changed` — is deferred until `with_since` is
  host-wired. An *unsolicited* flag-only `FETCH` (no `ENVELOPE`) that the server
  interleaves mid-response is dropped, so it can never overwrite a stored message's
  metadata; the change it signals rides a later `CHANGEDSINCE`. A `* VANISHED` set larger
  than the `MAX_VANISHED` cap (2²⁰, the adversarial-allocation guard) is truncated — an
  implausible size for a real delta, but a host hitting it would need a snapshot to
  reconcile the remainder.
- **First sync after a QRESYNC upgrade re-snapshots.** A store with a **pre-QRESYNC
  cursor** (no `HIGHESTMODSEQ`) does one **snapshot** on its first QRESYNC sync rather
  than a new-arrivals delta — otherwise it would record a modseq baseline while never
  fetching the flag/expunge changes to already-synced mail that predate the session,
  hiding them from every future `CHANGEDSINCE`. The snapshot reconciles them and
  establishes the baseline; subsequent syncs are incremental.
- **No `UID MOVE` fallback.** A server lacking RFC 6851 `MOVE` is unsupported for
  moves — the `COPY` + `\Deleted` + `EXPUNGE` fallback is a later refinement.
- **`UID EXPUNGE` requires UIDPLUS** (RFC 4315). A server without it would need a
  plain `EXPUNGE` (which expunges every `\Deleted` message in the mailbox) — also a
  later refinement.
- **`SEARCH` is implemented only for the sync-depth window** (`UID SEARCH SINCE`, see
  Sync-depth window above), not yet as a general **provider-search fallback** (the
  `search-coverage.md` slice). `UID SEARCH SINCE` is parsed for both the classic
  `* SEARCH` and extended `* ESEARCH … ALL` replies; richer criteria and the
  full-text provider fallback remain a later refinement.
- **No SMTP STARTTLS** (port 587). Implicit TLS (465) + `AUTH PLAIN` is implemented; STARTTLS is a later
  refinement.
- **IDLE watches one mailbox per connection.** `NOTIFY` (RFC 5465 — watch many
  mailboxes over a single connection) is a later refinement; per-folder `IDLE` (one
  `ImapWatcher` per watched mailbox) covers the common case (usually just INBOX), as
  most servers and clients do. Binding the watch to a host facade (engine-api / UniFFI),
  with its task lifecycle and reconnect policy, is deferred to the consuming host repo —
  the engine provides the `Watch` primitive, not the scheduling.
- **Charset coverage.** RFC 2047 decoding covers UTF-8, ISO-8859-1, and Windows-1252
  (ISO-8859-1 read as its CP1252 superset); other charsets fall back to a UTF-8-lossy
  read (a full charset table is a later refinement). `References` *is* fetched (a
  separate `BODY.PEEK[HEADER.FIELDS (REFERENCES)]` item — see Normalization above).
  Outbound non-ASCII subjects/display names are RFC 2047 `B`-encoded but **not folded**
  into 75-octet words (a later refinement).
- **Server literals are capped at 64 MiB.** A `{n}` larger than the cap is rejected
  (an adversarial server cannot drive an unbounded allocation); generous for any
  metadata response.
- **iTIP/iMIP scheduling**: the inbound parse/reconcile/trust/apply pipeline and
  the RSVP write primitive are **implemented** in `engine_core::scheduling` +
  `provider_caldav::imip` (`calendar-semantics.md`/`caldav.md`). The piece that
  touches *this* crate — **delivering an iTIP `REPLY` as an iMIP email** — is
  deferred: SMTP MIME assembly now supports multipart bodies and attachments, but
  the iTIP-specific `text/calendar` body builder is not wired yet (long
  encoded-words/folding are likewise unrefined). The `ServerAutoSchedule` RSVP path
  (conditional `PUT`, the server delivers the `REPLY`) needs no SMTP and is fully
  wired. **CalDAV/CardDAV** is the other step-5 slice.

## Testing

- **Offline (always green, no Docker):** the parsers and normalizers are
  unit-tested, including a panic/hang/overflow-resistance pass over adversarial
  input. A **mock async stream** replays full IMAP and SMTP transcripts to exercise
  the real transport, command sequencing, literal handling, snapshot/delta paging,
  UIDVALIDITY reset, per-recipient rejection, and post-`DATA` ambiguity. An
  **engine-sync integration** drives `ImapProvider` over the mock through
  `sync_mail_streamed` into a real `SqliteStore` (container-before-member, per-page
  progress, FTS search). The `needs_confirmation` → `NeedsConfirmation` bridge is
  locked in `engine-sync`. The **QRESYNC** path is covered offline by replaying the
  **exact bytes captured from live Stalwart** (`CAPABILITY`/`ENABLE`,
  `SELECT (CONDSTORE)`, and `UID FETCH … (CHANGEDSINCE … VANISHED)` with its
  `VANISHED (EARLIER)` + full-metadata FETCH) — through the parsers, the cursor
  roundtrip (incl. the pre-QRESYNC `;m`-less form), `qresync::delta_page`, and an
  engine-sync integration that snapshot-syncs then delta-syncs into a real
  `SqliteStore`, asserting the flag change *and* the expunge tombstone land with no
  re-snapshot.
- **Live (gated on `STALWART_IMAP_ADDR`, skips otherwise):** `tests/live_imap.rs` —
  connects over implicit TLS (trusting the self-signed cert via a test-only
  no-verify verifier, never a host store), and asserts the INBOX seed, the
  duplicate-`Message-ID` pair as two distinct objects, the **COPY-in-Archive
  distinctness** (the IMAP identity contrast), streamed paging with progress, an
  **SMTP submission** that delivers and files the Sent copy (found by its generated
  `Message-ID`), and a **`save_draft`** that files a draft and reads it back flagged
  `\Draft`. Reuses `crates/stalwart-harness`. A second gated file,
  `tests/live_imap_qresync.rs`, exercises the **QRESYNC incremental delta** against
  Stalwart (which advertises `CONDSTORE QRESYNC` post-auth): it snapshot-syncs the
  dedicated `QResync` seed mailbox, then re-flags one message and **expunges** another
  via `edit_mail`, and asserts the next sync — a delta, not a snapshot — reflects both
  the flag change and the tombstone in the store. The dedicated mailbox isolates the
  mutation from the count-asserted INBOX/Archive/Projects. The `stalwart` CI job runs
  both files; they are excluded from the offline coverage metric, like the harness
  probes and `provider-jmap/tests/`.
- **Real-provider exploration:** `examples/imap_explore.rs` connects to a *real*
  IMAP server over a verifying TLS connector (Mozilla roots) and lists folders +
  recent mail (read-only; opt-in `IMAP_QRESYNC` verifies the CONDSTORE/QRESYNC delta,
  `IMAP_DRAFT` saves a draft, and `IMAP_SEND` submits over SMTP `AUTH PLAIN` + implicit
  TLS). Validated against a real Dovecot server — read, UTF-8 subjects, and draft
  creation; authenticated SMTP send is implemented and offline-tested, exercisable via
  `IMAP_SEND`. The **CONDSTORE/QRESYNC delta** was validated read-only against
  **Soverin** (Dovecot): it advertises `CONDSTORE QRESYNC` post-auth, `SELECT (CONDSTORE)`
  returns `HIGHESTMODSEQ`, and the `IMAP_QRESYNC` check confirms the second sync is an
  incremental `Delta` (changed/removed ≈ 0, no re-snapshot) rather than re-listing the
  mailbox — the same path the live Stalwart test exercises with a full mutate→delta
  cycle. This is the "external provider smoke test" `north-star.md` step 7 anticipates,
  ahead of schedule.
