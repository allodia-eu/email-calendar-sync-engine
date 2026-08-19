# IMAP/SMTP Client Guidance

This document is authoritative for the **IMAP (RFC 9051 / RFC 3501, negotiated) read/sync + SMTP
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
  the self-signed fixture each supply their own. The injected `TlsConnector` is
  built from the shared `engine_tls::TlsClientConfig::connector()` (`tls.md`).
  Because it drives rustls directly, this is the **only** adapter that can report a
  negotiated `ConnectionInfo::tls_version` (`tls_info.rs`, captured in
  `connect_session`); it describes the IMAP session, not the per-send SMTP dial. For
  the same reason it is the only one to emit `ConnectStep::TlsEstablished` on the
  connect-observer seam (`providers.md`), followed by `ConnectStep::Authenticated` once
  `LOGIN` succeeds — and nothing else: IMAP dials a known address and runs no
  discovery, so it has no `Redirected`/`Discovered` step. Both are emitted from
  `open_session`, the stream-generic half of the dial, so the offline suite asserts the
  exact sequence over a `MockStream`. A rejected `LOGIN` emits no `Authenticated`. The
  observer rides on `ImapConfig` (`config.rs`), so an `ImapWatcher`'s dedicated
  connection — which shares `connect_session` — is observed too.
- Layers: `transport` (connect + the tagged line protocol: `LOGIN`/`CAPABILITY`/
  `ENABLE`/`SELECT [(CONDSTORE)]`/`UID FETCH [(CHANGEDSINCE … VANISHED)]`/`LIST`/
  `CREATE`/`APPEND`, literal handling), `transport_starttls` (the plaintext `STARTTLS`
  preamble + the raw-socket unwrap and greeting-less `resume` for the upgrade — see
  **STARTTLS** below), `parse` (pure response parsers,
  panic-resistant on hostile input), `mail` (normalize rows → `Message`/`Mailbox`),
  `cursor` (the per-mailbox `SyncState` — `UIDVALIDITY`/`UIDNEXT` plus an optional
  QRESYNC `HIGHESTMODSEQ` — + opaque `PageToken` encodings), `sync` (snapshot/delta
  UID-window paging), `qresync` (the QRESYNC incremental delta — flag changes +
  expunges via `CHANGEDSINCE`/`VANISHED`), `idle`/`watch` (the `IDLE` push primitives +
  the `ImapWatcher`), `smtp` (the submission *conversation*; the RFC 5322/MIME
  message assembly it feeds to `DATA` is the shared `engine-rfc5322` crate — see
  **SMTP submission**), `provider` (the `Provider` impl).

## How IMAP differs from JMAP (the shape)

- **Email scope is per mailbox.** JMAP has one account-global `Email` scope; IMAP
  state is per folder (`UIDVALIDITY`/`UIDNEXT`). So an `ImapProvider` is **bound to
  a single mailbox** for email: `email_scope` names that mailbox
  (`SyncScope::ImapMailbox{account, mailbox}`), and `stream_email` streams a
  resumable UID `FETCH` over it (a cold backfill row by row, a delta/reset via the
  page path — below). The folder list syncs under the new per-account
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
  `;m{modseq}` when the session negotiated QRESYNC, and an optional `;b{low}`
  **backfill watermark** (the lowest UID a still-descending cold backfill has
  committed — see the next bullet) while one is in flight (a completed non-QRESYNC
  cursor is byte-identical to the old format, and cursors lacking `;m`/`;b` decode
  with those fields `None`); a foreign/garbage cursor decodes to "no cursor" →
  snapshot. The **page path** below (used by a delta or a `UIDVALIDITY`-reset
  re-snapshot) pages **newest UIDs first, up to `limit` *messages* per page**: a page
  fetches
  a UID window and, if a gap (expunged UID) leaves it under-filled, **widens the
  window downward** until it has `limit` messages (or reaches the floor) — so
  `limit` is a count of messages, not a span of UID slots. Any older overshoot is
  capped off and re-fetched by the next page (whose window ends strictly below the
  lowest kept UID, so no duplication). The next boundary travels in the opaque
  `PageToken`. No `SEARCH` — windows are fetched directly, so expunged UIDs are
  simply absent (a gap), and a snapshot's accumulated `present` set is exactly the
  existing UIDs (tombstoning the rest). `limit` `0` means the whole remaining window
  in one page (the drain default).
- **Streaming cold backfill (resumable).** `stream_email` (`stream.rs`) `SELECT`s the
  mailbox once and splits into two paths. A **cold backfill** — a first sync (no
  cursor), or one resuming below a prior `;b` watermark under the same UID space — is
  streamed here rather than through the page path: it descends **newest-UID-first** in
  `fetch_batch`-wide UID groups (`UID SEARCH SINCE` UIDs when windowed, else the
  `1..=UIDNEXT-1` range chunked), and pulls each group's `UID FETCH` **one row at a
  time** (`uid_fetch_stream_start`/`next_fetch_row` in `fetch_stream.rs`), so it yields
  an additive chunk every `chunk_size` messages *within* one batched fetch and a host
  surfaces mail before the whole batch downloads. Each group **checkpoints
  `backfill_low`** = the group's lowest UID into the cursor (`;b{low}`), so a kill
  resumes below the watermark instead of restarting; the last group clears the
  watermark to the steady-state `frontier` cursor (the `UIDNEXT`/`HIGHESTMODSEQ`
  captured when the backfill first started, so mail arriving during it is caught by the
  first delta afterwards). Previews are **not** hydrated on this path (reading bodies
  would defeat fast metadata streaming); a host fetches bodies on demand. The
  connection **self-heals** a dropped mid-command streamed fetch: an abandoned tag is
  recorded in `pending_tag` and drained by the next command (`drain_pending`). A
  **delta** (new arrivals, or a QRESYNC flag/expunge reconcile) or a `UIDVALIDITY`-reset
  **re-snapshot** instead delegates to the tested `sync_page_selected` (reusing the one
  `SELECT`) and re-chunks each page with `split_page` — small (a delta) or rare (a
  reset), so fetching a page whole before re-chunking is fine, and previews *are*
  hydrated there.
- **Sync-depth window (per sync).** The sync-depth floor is now a **per-sync
  argument** — `stream_email(…, window, …)` takes a `SyncWindow { since }` — so a host
  changes depth without reconnecting the provider; `ImapConfig::with_since(date)`
  survives only as the `default_sync_window` the whole-scope `sync_email` drain fetches
  under. Either way it bounds a **snapshot/backfill** to mail delivered on or after
  `date`: a single `UID SEARCH SINCE <dd-Mon-yyyy>` (`transport::uid_search_since`,
  parsed by `parse_search`, tolerating both classic `* SEARCH` and extended
  `* ESEARCH … ALL`) yields the in-window UIDs, and the sync starts at the **lowest** of
  them (older mail is never fetched), reporting their count as the `total` progress
  denominator. No matches yields an empty snapshot that still tombstones stale rows
  below the window. A **delta** is already bounded to new arrivals, so the window never
  narrows it (no `SEARCH` is issued). With no cutoff (the default) the whole mailbox
  syncs. This is how a host implements "configurable sync depth" without an
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
  in the cursor. A delta with a prior baseline then **splits the UID space at the prior
  cursor's `UIDNEXT`**, because the two halves are worth different amounts of network.
  *Below* it the message is already stored and its content cannot have moved — IMAP has no
  in-place edit, so an edit or a move mints a new UID — which makes `FLAGS` the whole of what
  a `CHANGEDSINCE` row there can be reporting: it is fetched
  `UID FETCH 1:<next-1> (UID FLAGS) (CHANGEDSINCE <modseq> VANISHED)` and each row becomes a
  `MailStateChange` the store applies to that row's state columns. This is the half a "mark all
  read" lands in; it used to return every changed message's `ENVELOPE` and `BODYSTRUCTURE` to
  write a flag bitfield. *At or above* it the message is new to us and comes back with the full
  metadata a first sync needs, as `changed`.

  Two traps guard that second fetch, and they are not the same one. `UID FETCH n:*` matches the
  **highest** UID when nothing reaches `n` (RFC 9051 §6.4.8), so the range is issued only when
  the current `UIDNEXT` has moved — and, separately, every returned row is floored at
  `uid >= n`, because `UIDNEXT` can have moved while nothing at or above it *survives* (mail
  arrived since the baseline and was expunged again). Without the floor that case re-maps the
  newest already-synced message as an arrival and rewrites a payload the pass had no reason to
  touch. A row with no `ENVELOPE` is an unsolicited flag-only `FETCH` the server may interleave
  once CONDSTORE is on (RFC 7162 §3.2) and becomes a state change rather than an empty-envelope
  object.

  `* VANISHED (EARLIER) <set>` rides the first command and lists the UIDs expunged since the baseline, which become
  the page's `removed` keys (the store tombstones them inline — `store-and-sync.md`
  `Delta { changed, removed }`). The set is expanded per UID and bounded by a cap so a
  hostile range cannot exhaust memory. The pass is a single page (the changed set is
  bounded to what moved since the last sync); the new baseline is the SELECT-time
  `HIGHESTMODSEQ`. So a host "refresh" that must reflect server-side flag/move/delete
  changes no longer needs `Engine::clear_mail_cursors` against a QRESYNC server — a plain
  delta sync reconciles them.
- **Normalization.** `UID FETCH (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE
  BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (REFERENCES)])` (safe metadata — none sets
  `\Seen`). The `References` header is not an `ENVELOPE` field, so it rides a
  separate peek-safe body-header item to feed threading (`threading.md`).
  `BODYSTRUCTURE` feeds `Message.has_attachment` without downloading parts: explicit
  attachments or named non-CID parts count; CID inline resources do not, a `text/plain`/
  `text/html` body part is never counted on a bare `name=` alone, RFC 2231 split/encoded
  filenames (`name*0*`) are recognized, and `message/global` is read like `message/rfc822`.
  This keeps the list-time flag in step with `engine-mime::extract_attachments`. Flags → keywords:
  `\Seen`/`\Flagged`/
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
- **An extended `LIST` returns only the extended data its return options name**
  (RFC 5258 §3), including data the same server volunteers on a plain `LIST`. A server
  is merely *permitted* to offer SPECIAL-USE unasked (RFC 6154 §2), so the moment a
  `RETURN (…)` clause is added for anything else, `SPECIAL-USE` must be in it too or
  every folder silently loses its role — which costs the sidebar its icons and ordering
  *and* misfiles sent copies, since `place.rs` resolves the Sent folder from those same
  attributes. `list_command` builds the clause, and asks for `SPECIAL-USE` only where the
  capability was advertised: an unadvertised return option is a `BAD`, which costs the
  whole folder list rather than only its roles.
- **Unread counts** (`Mailbox::unread_count`, `unseen.rs`). `LIST` carries none, so the
  folder-list sync asks for `UNSEEN` too: one round trip via
  `LIST "" "*" RETURN (SPECIAL-USE STATUS (UNSEEN))` where the server advertised
  **LIST-STATUS** (RFC 5819), else one `STATUS <mailbox> (UNSEEN)` per selectable mailbox — capped at
  `MAX_STATUS_PROBES`, so a pathological account cannot turn a folder list into a
  minutes-long stall. `\Noselect` containers are never probed (`STATUS` on one is an
  error, not a zero); a mailbox the server refuses (`NO`/`BAD`) is left uncounted
  rather than failing the whole list; a transport error still propagates, because
  every later probe would go down a dead connection. A mailbox with no answer keeps
  `unread_count: None` — **absent is not zero**, and a host must not render it as one.
  Note `UNSEEN` in a `STATUS` response is a *count*, while the same word in a `SELECT`
  response code is the first unseen message's sequence number; only the former is read.
  Both parsers read **untagged lines only** (`Response::untagged`, never `into_all_lines`,
  which exists for a response code that may ride the completion line): Dovecot completes a
  `LIST` with `List completed (0.003 + 0.000 secs).` — four items whose first word is the
  keyword and whose last is a bare `.`, so read as data it invents a mailbox named `.`.

## SMTP submission

- **`submit_email`** runs the conversation `EHLO → [AUTH] → MAIL FROM → RCPT TO* →
  DATA`, then files the sent copy. The pre-generated `Message-ID` is on the message
  so the sent copy reconciles by it.
- **Message assembly (`engine_rfc5322::assemble_message`)** lives in the shared
  **`engine-rfc5322`** crate (the Graph adapter reuses it for `sendMail` in MIME
  format — `graph.md`), returning the engine-neutral `ProviderError`; `provider-imap`
  feeds its bytes to the SMTP `DATA` command. It is hardened against header injection:
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
- **Three transports.** `ImapConfig::with_smtp(addr)` is **plaintext, no auth** — for
  an MX that accepts local mail (the fixture's port 25). `with_smtp_tls(addr,
  server_name)` is **implicit TLS + `AUTH PLAIN`** (port 465) using the account
  credentials and the injected connector. `with_smtp_starttls(addr, server_name)` is
  **STARTTLS + `AUTH PLAIN`** (port 587): the client connects in the clear, `EHLO`s,
  upgrades the socket with `STARTTLS`, re-`EHLO`s over TLS, then authenticates — it
  **fails** if the server does not advertise `STARTTLS` (no cleartext auth). AUTH is
  only ever attempted once the stream is secured (implicit TLS, or after the upgrade),
  never in the clear. The upgrade is negotiated in `smtp::negotiate_starttls` (greeting
  → `EHLO` → `STARTTLS`), then the caller TLS-wraps the socket and the conversation
  continues over `smtp::send_after_starttls` (which skips the greeting a server does
  not re-send post-upgrade). Data buffered past the `STARTTLS` `220` is rejected — a
  command-injection guard (CVE-2011-0411 class), so injected cleartext never crosses
  the TLS boundary.
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
- **Sent placement never fails a send, and is never silent.** Delivering and filing are
  two operations here — SMTP dials fresh per send, the `APPEND` rides the standing IMAP
  session — so a session that went stale while idle delivers the mail and loses the copy.
  Three rules follow, and the third is the one that was missing:
  1. A delivered send is **never** returned as an error for a filing failure. The mail has
     gone; a caller that saw `Err` would re-send it.
  2. The placement is **retried once on a freshly dialed session**, because a dead standing
     session is the expected cause, not an exotic one. The retry first asks whether the copy
     is already there (`UID SEARCH HEADER Message-ID`, `place::find_placed_copy`) —
     `APPEND` is not idempotent, and a first attempt that committed but lost its response
     must not become two copies in Sent.
  3. When neither attempt files it, the receipt says so: `SentCopy::Unfiled { detail }`,
     carried through `engine_sync::SubmitOutcome` to the host. **Nothing later can
     rediscover this** — there is no copy on the server to reconcile against — so a host
     that drops the outcome drops the fact for good. A receipt unable to express it means a
     delivered message leaves no trace in the sender's Sent folder while every layer reports
     success.
  4. The host can then ask for the repair: `Provider::file_sent_copy` (→
     `ImapProvider::refile`, reached through `Engine::file_sent_copy`) files the copy of an
     already-delivered message and **sends nothing**. It is what a "try again" control calls,
     so it probes on *every* attempt, standing session and fresh dial alike — a button gets
     pressed twice. It is deliberately **not** outbox-mediated: the outbox exists so a side
     effect is neither lost nor repeated across a crash, and this one is idempotent by
     construction and safe to ask for again.

  With UIDPLUS the `APPEND` returns `[APPENDUID validity uid]` → the receipt carries the
  real Sent key (the same key the next Sent sync synthesizes); without it the receipt key
  is `Message-ID`-derived and the copy reconciles when Sent is synced.
- **Mailbox names: the wire form is the id, the decoded form is the label.** A `LIST` name
  is modified UTF-7 (RFC 3501 §5.1.3), so `Travel &- Expenses` is one folder called
  `Travel & Expenses`. `utf7::decode` produces `Mailbox::name` for display **only**;
  `MailboxId`, every `imap:v…:u…@folder` key, and every `SELECT`/`APPEND`/`CREATE`
  argument keep the server's own bytes. Decoding an id instead would re-key every message
  in a non-ASCII folder and address the server with a name it never advertised. There is
  deliberately no encoder: no name this crate sends originates from a decoded one.
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
  lossless and serves the body, inline CID resources, and downloadable attachments from
  the cached raw with no re-fetch (`providers.md`, `store-and-sync.md`).
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
  example's `IMAP_IDLE` opt-in watches a real account read-only. The push path was also
  validated against **Soverin** (Dovecot): the read-only watch negotiates `IDLE`, enters
  it, and re-issues on the keep-alive (a `KeepAlive` heartbeat), and a draft `APPEND`ed by
  a second connection pushes a `Changed` — confirming the path across a second server
  implementation, like the QRESYNC delta was.

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
  of the delta is a later refinement), and it does **not** re-apply the sync-depth
  window — the window bounds only a snapshot/backfill, and the QRESYNC delta fetches
  `1:*` regardless (so now that the window is a per-sync `stream_email` argument, this
  is the one path still ignoring it), so a flag change to an *out-of-window* message can
  re-enter the store. Bounding the delta — correctly, since `VANISHED` needs `1:*` to
  report already-expunged UIDs while a window must restrict only `changed` — is a later
  refinement. An *unsolicited* flag-only `FETCH` (no `ENVELOPE`) that the server
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
- **STARTTLS is implemented** for both IMAP (`ImapConfig::with_starttls`, port 143)
  and SMTP submission (`with_smtp_starttls`, port 587), alongside implicit TLS. Both
  connect in the clear, verify the peer advertises `STARTTLS`, upgrade the socket, and
  only then log in / authenticate — refusing to proceed (never sending credentials in
  the clear) if it is not advertised. Because a STARTTLS dial is byte-for-byte an
  implicit-TLS one *after* the upgrade, the provider stays generic over a single
  `TlsStream` type — the upgrade happens on the raw socket before it becomes the
  session stream (`Connection::start_tls` / `into_inner_stream` / `resume`).
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
  touches *this* crate — **delivering an iTIP `REPLY` as an iMIP email** — is now
  **implemented too** (issue #105): a `Draft` carrying a `DraftCalendar { ical, method }`
  is assembled as a `text/calendar` **alternative body part**, and this adapter advertises
  `Capabilities::scheduling_submission`. A caller needs that path whenever the account's
  calendar server does not schedule for it (`Capabilities::calendar_scheduling` is
  `false`) — the very common IMAP-mail-plus-plain-CalDAV shape, where the
  `ServerAutoSchedule` `PUT` stores the answer and tells the organizer nothing. What is
  still not the engine's job is *building* the `REPLY` object: there is no `Event` → iTIP
  serializer, and the answer keys to a `UID`/`SEQUENCE` only the caller holds. (Long
  encoded-words/folding are likewise still unrefined.) **CalDAV/CardDAV** is the other
  step-5 slice.
  - The part is a sibling of the text body inside `multipart/alternative`, ordered last
    (most faithful, RFC 2046 §5.1.4), with `method=` on its `Content-Type` (RFC 6047 §2.4),
    `charset=utf-8`, base64 rather than `7bit` (§2.5 — iCalendar content lines are long and
    folded, and a transport free to re-wrap them corrupts the object), and **no**
    `Content-Disposition`. It is a representation of the message, not a file; the shared
    assembly lives in `engine-rfc5322`, so the Graph and Google submit paths get it too.

## Which server proves what — rev1 vs rev2

**The client speaks IMAP4rev2 where a server offers it, and IMAP4rev1 everywhere else —
one code path, chosen by negotiation rather than branched on.** `capability.rs` holds the
whole rule:

- **Advertised is not enabled.** A dual-revision server announces `IMAP4rev2` in its
  greeting and keeps behaving as rev1 until the client sends `ENABLE IMAP4rev2` *and it
  answers* `* ENABLED IMAP4rev2` (RFC 5161 §3.1). Only the confirmation counts. Reading the
  capability as the dialect leaves the client parsing modified UTF-7 as though it were
  UTF-8.
- **One `ENABLE` buys the whole set.** rev2 is largely rev1 with a dozen extensions folded
  into the base protocol (RFC 9051 Appendix E items 2–3: NAMESPACE, UNSELECT, UIDPLUS,
  ESEARCH, SEARCHRES, ENABLE, IDLE, SASL-IR, LIST-EXTENDED, LIST-STATUS, MOVE, LITERAL-,
  the FETCH side of BINARY, SPECIAL-USE's mailbox attributes, STATUS=SIZE), so
  `Extension::folded_into_rev2` lets a rev2 session rely on them without the server naming
  any of them. `enable_arguments` therefore sends the dialect plus only the extensions that
  need announcing, in a single command.
- **QRESYNC is not folded in.** rev2 took only its `CLOSED` response code (item 9), so
  CONDSTORE/QRESYNC keeps its own capability and its own `ENABLE`.
- **Folded in ≠ will arrive.** rev2 folds in SPECIAL-USE's *attributes* and makes them base
  `LIST` data (§7.3.1) — it defines **no** `RETURN (SPECIAL-USE)` option of its own, which
  reads like a rev2 session never has to ask. **Dovecot's rev2 disproves that**: it
  advertises RFC 6154 as well and keeps RFC 6154's rule, so an extended `LIST` that does not
  ask comes back with every role stripped — no `\Sent`, and `place.rs` has nowhere to file a
  sent copy. `must_request_special_use` therefore gates on the **advertised** capability,
  never on the dialect: RFC 6154 advertised means that return option is enabled (§6.3.9), and
  Stalwart accepts it on a rev2 session where the attributes were coming anyway. It must not
  gate on `has` either — `has` is true on rev2 for a server that never advertised RFC 6154,
  and an unadvertised return option is a `BAD` that costs the whole folder list rather than
  only its roles.
- **The outcome is reported, not inferred.** `finish_session` emits a `ConnectStep::Negotiated`
  carrying the dialect and the extensions the session may use, so a diagnostic log says which of
  the two dialects an account settled on and what came with it. It reports *usable*, not
  advertised — on rev2 that includes everything folded in, which is the distinction a support
  report turns on.
- **The dialect reaches the data in exactly one place: mailbox names.** rev1 encodes them
  as modified UTF-7, rev2 as UTF-8 (§5.1, item 16). `utf7` handles both directions and the
  transport owns the conversion, so a `Mailbox` id is the decoded name on either dialect and
  a message key built from it does not change the day a server starts offering rev2. Never
  decode unconditionally: `R&AD-` is a mailbox name on rev2 and a shift sequence on rev1.

**Three** live servers hold this up — two of them rev2, which is not redundancy:

| | Stalwart (`docker/stalwart/`) | Dovecot rev1 (`docker/dovecot/`) | Dovecot rev2 (same image) |
| --- | --- | --- | --- |
| Dialect the client negotiates | **IMAP4rev2** (advertised, enabled, confirmed) | **IMAP4rev1** | **IMAP4rev2**, experimental (`imap4rev2_enable`) |
| Mailbox names on the wire | UTF-8 | modified UTF-7 | UTF-8 |
| SPECIAL-USE on an extended `LIST` | volunteered whether asked or not | **only** when the return option asks | **only** when the return option asks — on rev2 |
| Mailbox names in `LIST` rows | always quoted | unquoted atoms where quoting is unnecessary | quoted only where needed |
| `* ENABLED` casing | `IMAP4rev2` | — | `IMAP4REV2` (atoms are case-insensitive) |
| Tagged completion | `LIST completed` | `List completed (0.028 + 0.000 secs).` | same prose form |

Three asymmetries decide where a new test belongs.

**A server that volunteers data cannot show you that you forgot to ask for it.** Stalwart
returns SPECIAL-USE either way, so an omitted return option is green there in perpetuity,
while on either Dovecot it costs every folder its role *and* misplaces the sent copy
(`place.rs` resolves that folder from the same attributes).

**A dialect only proves its own half.** The rev1 encode/decode round trip has no live
coverage on a rev2 server, and rev2's UTF-8 names have none on a rev1 one.

**One implementation of a dialect is not the dialect.** This is why Dovecot runs twice.
"rev2 folds SPECIAL-USE in, so a rev2 session need not ask" is a defensible reading of
RFC 9051 that Stalwart happens to satisfy and Dovecot's rev2 does not — and with one rev2
server the client shipped the reading, not the protocol. A second implementation of the
*same* dialect is the only thing that separates the two. Note that a server cannot serve
both dialects at once: the client `ENABLE`s rev2 wherever it is offered, so the pair is two
services (`rev1.conf` / `rev2.conf`) rather than one server and a switch.

So: contract assertions go in `live_imap_contract.rs` and run against *every* configured
server; dialect-specific ones go in `live_imap_rev1.rs` / `live_imap_rev2.rs`, which are
keyed on the dialect and not on the vendor — a server moves between them when it changes
dialect, with no test rewritten. When adding coverage, ask which server would notice if the
behaviour broke; if the answer is none, the test is not proving what it claims.

## Testing

- **Offline (always green, no Docker):** the parsers and normalizers are
  unit-tested, including a panic/hang/overflow-resistance pass over adversarial
  input. A **mock async stream** replays full IMAP and SMTP transcripts to exercise
  the real transport, command sequencing, literal handling, snapshot/delta paging,
  UIDVALIDITY reset, per-recipient rejection, and post-`DATA` ambiguity. An
  **engine-sync integration** drives `ImapProvider` over the mock through
  `sync_mail` into a real `SqliteStore` (container-before-member, per-chunk
  commit/progress, FTS search). The `needs_confirmation` → `NeedsConfirmation` bridge is
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
  `\Draft`. Reuses `crates/stalwart-harness`.
- **Live, dialect-independent (`tests/live_imap_contract.rs`):** the folder-list contract,
  looped over every configured server and skipping those whose `*_IMAP_ADDR` is unset.
  Asserts special folders by **role** (the harnesses name them differently), that every
  listed mailbox carries a count, that no mailbox is invented from a completion line, that
  a mailbox id equals its name, and — the identity model's own claim — that one non-ASCII
  folder reaches the model under the same identity on both dialects despite completely
  different bytes on the wire.
- **Live, per dialect (`tests/live_imap_rev1.rs`, `tests/live_imap_rev2.rs`):** only what
  one dialect can show, looped over the servers speaking it. rev1: a modified-UTF-7 name
  decoded into the identity, and a `SELECT` by that decoded identity reaching the mailbox
  (which is the encode half). rev2, across **both** rev2 servers: a UTF-8 name needing no
  decoding, a `SELECT` sending it unencoded, and the roles surviving the dialect — which
  Stalwart cannot fail and Dovecot's rev2 fails the moment the client stops asking.

  A third gated file,A third gated file,
  `tests/live_imap_tls.rs`, covers the **TLS transports beyond that baseline** —
  every important IMAP/SMTP port is thus exercised live: an **IMAP STARTTLS** dial
  (`STALWART_IMAP_STARTTLS_ADDR` / 143) that reports a negotiated `tls_version` (proof
  the cleartext socket upgraded before login) and loads the INBOX seed; an **SMTP
  submission over STARTTLS** + `AUTH PLAIN` (`STALWART_SMTP_STARTTLS_ADDR` / 587); and an
  **SMTP submission over implicit TLS** + `AUTH PLAIN` (`STALWART_SMTP_TLS_ADDR` / 465,
  the `with_smtp_tls` path) — the first two pin the `STARTTLS` command shape the offline
  mocks cannot validate, the third gives the implicit-TLS submission path its first live
  proof. A second gated file,
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
