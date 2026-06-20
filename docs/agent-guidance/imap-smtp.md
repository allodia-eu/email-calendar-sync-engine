# IMAP/SMTP Client Guidance

This document is authoritative for the **IMAP (RFC 9051) read/sync + SMTP
(RFC 5321) submission provider** — the mail half of build-order step 5
(`north-star.md`). It covers the `provider-imap` crate and the IMAP/SMTP
specifics it implements against the Stalwart fixture. Read it before touching
`provider-imap` (and the submission paths in `engine-provider`/`engine-sync`),
alongside `providers.md` (the Provider Contract), `store-and-sync.md` (the
apply/lease model and `SyncScope`), `jmap.md` (the precedent it mirrors), and
`stalwart-harness.md` (the fixture).

CalDAV/CardDAV is the **other** step-5 slice and is not covered here.

## The crate

- **`provider-imap`** — a hand-rolled minimal IMAP + SMTP client over a **generic
  async stream**, implementing `engine_provider::Provider`. No third-party
  IMAP/SMTP library: the SMTP per-recipient and post-`DATA` invariants stay under
  our control, and the whole protocol is offline-testable by replaying captured
  transcripts through an in-memory stream (mirroring `provider-jmap`'s `Executor`
  seam and the harness probe). TLS is pure-Rust `tokio-rustls`, with the host
  injecting trust policy — the library bakes in no root store, so mobile hosts and
  the self-signed fixture each supply their own.
- Layers: `transport` (connect + the tagged line protocol: `LOGIN`/`SELECT`/`UID
  FETCH`/`LIST`/`CREATE`/`APPEND`, literal handling), `parse` (pure response
  parsers, panic-resistant on hostile input), `mail` (normalize rows →
  `Message`/`Mailbox`), `cursor` (the per-mailbox `SyncState` + opaque `PageToken`
  encodings), `sync` (snapshot/delta UID-window paging), `smtp` (the submission
  conversation + RFC 5322 assembly), `provider` (the `Provider` impl).

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
  `v{validity};n{next}`; a foreign/garbage cursor decodes to "no cursor" → snapshot.
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
- **Snapshot vs delta.** First sync (no cursor) or a UIDVALIDITY mismatch →
  **snapshot** (rediscover from UID 1, carry `present`). A matching cursor → **delta**
  of new arrivals only (UIDs at or above the cursor's `UIDNEXT`). A delta carries
  **no removals**: flag changes and expunges of already-synced messages are not
  detected incrementally without CONDSTORE/QRESYNC (a deferred capability) — a
  periodic snapshot reconciles them. This is the honest baseline `providers.md`
  prescribes ("CONDSTORE/QRESYNC paths are optional capabilities, not assumptions").
- **Normalization.** `UID FETCH (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`
  (Tier-1, all peek-safe — none sets `\Seen`). Flags → keywords: `\Seen`/`\Flagged`/
  `\Answered`/`\Draft` map to their `$`-keywords; `\Deleted`/`\Recent` are
  deliberately not keywords (expunge/session model); custom keywords pass through.
  `INTERNALDATE` → a UTC instant (offset applied). `ENVELOPE` → subject, flattened
  addresses, and the `Message-ID`/`In-Reply-To` hints; **RFC 2047 encoded-words** in
  the subject and display names are decoded (`B`/`Q`, UTF-8/ISO-8859-1, with
  whitespace between adjacent words dropped — `encoded_word.rs`). A quoted string
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
  every interpolated value (`Message-ID`, addresses, subject, display names) is
  **rejected on CR/LF/NUL** (RFC 5322 §2.2 / RFC 5321 §2.3.8 — otherwise a poisoned
  draft could inject headers or split the command stream), and a **non-ASCII subject
  or display name is emitted as an RFC 2047 `B` encoded-word**, never raw 8-bit
  bytes, so headers stay 7-bit clean. A **`Date` header is generated locally**
  (RFC 5322 §3.6 requires it; for an IMAP `APPEND` — `save_draft` / the Sent copy —
  no server is in the loop to add one). The body is normalized so a bare CR/LF never
  reaches the wire. (Long encoded-words are not yet folded into 75-octet runs — a
  later refinement.)
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

## Known limitations (documented, not bugs)

- **No CONDSTORE/QRESYNC.** Deltas bring new arrivals only; flag/expunge changes
  reconcile via a periodic snapshot. Deferred capability.
- **No IMAP `SEARCH` provider-search fallback** yet (the `search-coverage.md`
  slice). The transport does not implement `SEARCH`.
- **No SMTP STARTTLS** (port 587). Implicit TLS (465) + `AUTH PLAIN` is implemented; STARTTLS is a later
  refinement.
- **`References` not populated** (it is not an `ENVELOPE` field; a later threading
  slice). RFC 2047 decoding covers UTF-8/ISO-8859-1; other charsets fall back to a
  UTF-8-lossy read (a full charset table is a later refinement). Outbound non-ASCII
  subjects/display names are RFC 2047 `B`-encoded but **not folded** into 75-octet
  words (a later refinement).
- **Server literals are capped at 64 MiB.** A `{n}` larger than the cap is rejected
  (an adversarial server cannot drive an unbounded allocation); generous for any
  metadata response.
- **iTIP/iMIP scheduling** is out of scope (distinct from event storage —
  `calendar-semantics.md`), as is **CalDAV/CardDAV** (the other step-5 slice).

## Testing

- **Offline (always green, no Docker):** the parsers and normalizers are
  unit-tested, including a panic/hang/overflow-resistance pass over adversarial
  input. A **mock async stream** replays full IMAP and SMTP transcripts to exercise
  the real transport, command sequencing, literal handling, snapshot/delta paging,
  UIDVALIDITY reset, per-recipient rejection, and post-`DATA` ambiguity. An
  **engine-sync integration** drives `ImapProvider` over the mock through
  `sync_mail_streamed` into a real `SqliteStore` (container-before-member, per-page
  progress, FTS search). The `needs_confirmation` → `NeedsConfirmation` bridge is
  locked in `engine-sync`.
- **Live (gated on `STALWART_IMAP_ADDR`, skips otherwise):** `tests/live_imap.rs` —
  connects over implicit TLS (trusting the self-signed cert via a test-only
  no-verify verifier, never a host store), and asserts the INBOX seed, the
  duplicate-`Message-ID` pair as two distinct objects, the **COPY-in-Archive
  distinctness** (the IMAP identity contrast), streamed paging with progress, an
  **SMTP submission** that delivers and files the Sent copy (found by its generated
  `Message-ID`), and a **`save_draft`** that files a draft and reads it back flagged
  `\Draft`. Reuses `crates/stalwart-harness`. The `stalwart` CI job runs it; the file
  is excluded from the offline coverage metric, like the harness probes and
  `provider-jmap/tests/`.
- **Real-provider exploration:** `examples/imap_explore.rs` connects to a *real*
  IMAP server over a verifying TLS connector (Mozilla roots) and lists folders +
  recent mail (read-only; opt-in `IMAP_DRAFT` saves a draft and `IMAP_SEND` submits
  over SMTP `AUTH PLAIN` + implicit TLS). Validated against a real Dovecot server —
  read, UTF-8 subjects, and draft creation; authenticated SMTP send is implemented
  and offline-tested, exercisable via `IMAP_SEND`. This is the "external provider
  smoke test" `north-star.md` step 7 anticipates, ahead of schedule.
