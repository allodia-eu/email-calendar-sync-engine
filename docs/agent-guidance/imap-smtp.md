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
  the credential is accepted — and nothing else: IMAP dials a known address and runs no
  discovery, so it has no `Redirected`/`Discovered` step. Both are emitted from
  `finish_session`, the stream-generic half of the dial, so the offline suite asserts the
  exact sequence over a `MockStream`. A rejected credential emits no `Authenticated`. The
  observer rides on `ImapConfig` (`config.rs`), so an `ImapWatcher`'s dedicated
  connection — which shares `connect_session` — is observed too.
- **A refused `LOGIN` is not always a bad credential.** `NO` is `ImapError::Auth`
  (`FailureClass::Authentication`), except a `NO` whose text opens with `[LIMIT]`
  (RFC 5530), which is `ImapError::RateLimited` (`FailureClass::RateLimited`): the server
  is refusing sessions for the account and never judged the password. A host that read it
  as `Authentication` would ask the user to sign in again, which cannot help. Observed
  live as `NO [LIMIT] LOGIN Rate limit hit.` on a session opened seconds after five
  others had authenticated with the same credential. The rule is scoped to `LOGIN`:
  RFC 5530's `LIMIT` is any implementation limit, so on `STORE` it can mean too many
  keywords, which is `InvalidState` like any other `NO`. An `AUTHENTICATE` carrying a token
  is classified the same way, since it is the other way into a session. SMTP `AUTH`, with a
  password or a token, follows the same rule in its own terms: a 4xx is "try again later"
  (RFC 5321 §4.2.1), so it is `RateLimited` too, except RFC 4954's `432` (a password
  transition is needed), which only the user can resolve and stays `Authentication`. Nothing in the adapter waits either
  out; the class is the host's cue to back off (`http-throttling.md` covers the HTTP
  adapters, where the engine does the waiting).
- Layers: `dial` (the connect sequence itself: TCP, TLS, authenticate, negotiate; behind
  `ImapAccount::connect` and every connection the account's pool opens after it),
  `credentials`/`sasl` (a password or an OAuth 2.0 access token, and the two SASL
  mechanisms that carry a token — see **Authentication** below),
  `probe` (what a server accepts, read *before* there is a credential to present),
  `transport_auth`/`smtp_auth` (the `AUTHENTICATE`/`AUTH` exchanges those drive),
  `transport` (connect + the tagged line protocol: `LOGIN`/`CAPABILITY`/
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
  **SMTP submission**), `smtp_stream` (its framing: reading a reply, writing a command
  or the message), `deadline` (how IMAP and SMTP apply the shared bounds on a silent
  server, `deadlines.md`), `pool` + `account` (the per-account connection budget — see
  **Connections** below), `provider` (the `Provider` impl).

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

## Connections: one budget per account

- **A host connects an account, then binds folders.** `ImapAccount::connect(&config,
  connector)` dials once — proving the credentials and reading the capabilities every
  folder's `ConnectionInfo` reports — and keeps that connection in the account's pool.
  `ImapAccount::provider(mailbox)` dials nothing; `ImapAccount::watch(mailbox, keepalive)`
  spends one connection for as long as the watcher lives. There is deliberately no
  per-folder connect any more: the old `ImapProvider::connect` / `ImapWatcher::connect`
  each dialled a socket they held for their whole lifetime, so an account's socket count
  was a property of the *type* — one per bound folder, idle or not, all re-dialled in one
  burst on every network drop (measured: 107 connections in four hours on one Android
  device). Two `ImapAccount`s for one account are two budgets; build one.
- **Why the engine must bound it, not the server.** IMAP has no way to learn a server's
  connection limit, and only some servers say when it was hit: RFC 5530 has no "too many
  connections" code. Yahoo answers with `NO [LIMIT]` at `LOGIN` (`RateLimited`, above);
  Dovecot's `mail_max_userip_connections` (default 10) refuses at `LOGIN` with
  `AUTHENTICATIONFAILED`, which reads as a *wrong password*, and a client that believes it
  tells the user their sign-in expired. Either way the account does not connect, so the
  only defence is never to need the limit. The budget is `pool::DEFAULT_MAX_CONNECTIONS` = 5 (Thunderbird
  desktop's `max_cached_connections`), not host-configurable until a host needs it.
- **Budget arithmetic: sockets, not borrows.** Workers (sync, writes, fetches, filing)
  borrow a connection per call — a streamed pass for as long as the stream lives — and
  wait when the budget is spent. A watch takes one out for good (a connection in `IDLE`
  can only send `DONE`), and `reserve_watch` refuses the one that would leave no worker:
  a budget spent on push is an account that can never sync. The refusal is
  `InvalidState` (poll instead), and `ImapAccount::watch_headroom` lets a host warn
  before offering the choice. A parked connection holds no permit, so the pool dials
  only with nothing parked, and a watch reuses a parked connection rather than dialling
  beside it — otherwise `parked + borrowed + watches` exceeds the ceiling.
- **When a connection is proved alive.** A parked connection is checked with `NOOP`
  before reuse once it has rested `pool::VALIDATE_AFTER_REST` (30 s, on the **wall**
  clock — Linux's monotonic clock stops during suspend, so a laptop that slept an hour
  would look like it rested a second). Younger ones are handed out unchecked, because a
  sync pass's back-to-back calls would otherwise each pay a round trip. The two ways a
  young connection can still be dead are closed elsewhere: **any call that fails does not
  park its connection** (`PooledConnection::settle` — a `NO` leaves a usable session, a
  dropped socket or desynchronised reply does not, and telling them apart at every call
  site is more fragile than one extra dial after a rare refusal), and
  `ImapAccount::invalidate` drops every resting connection for a host that has seen the
  network change. A stream abandoned mid-fetch parks normally: `pending_tag` drains its
  leftover response before the next command.
- **A dial refused beside working connections waits, and lowers the ceiling.** Five is our
  number, and the server's limit is shared with every other client of the user's. A caller
  whose dial fails while another *worker* holds a connection waits for one to come back
  rather than failing; it fails only once no worker is left. When the server *answered*
  (`LOGIN` `NO`, `NO [LIMIT]`, `BYE` at the greeting) the ceiling also drops by that
  connection, since a `NO` to a credential other sessions are logged in with right now is
  a session limit, not a verdict on the credential; `ImapAccount::invalidate` restores it,
  a new network being a new address. An unanswered dial (the network) lowers nothing.
  A refused **token** first gets its one renewal (**Authentication** below), so an expired
  token that a fresh one replaces never reaches the pool as a refusal; only a fresh token
  refused as well does, and that is the session limit again. Proof: `pool_refusal_tests.rs`,
  `dial_renewal_tests.rs`.
- **The pool re-dials on its own.** A provider holds no socket, so a dead one is replaced
  on the next call rather than failing every call until the host rebuilds the provider.
  Every pool dial reports through the config's connect observer, so a host still sees
  each socket the account opens. SMTP is outside the budget: it dials per send and closes.
- **Proof.** `pool_tests.rs`/`account_tests.rs` count dials over scripted streams. The live
  bound is `tests/live_imap_pool.rs`: twelve folders syncing at once beside a held watch
  log in at most five times against Stalwart. It is an absence claim, so it carries a
  control arm (an account per folder must log in twelve times) and was proved red by
  lifting the ceiling (13 logins).

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
  `date`: `UID SEARCH SINCE <dd-Mon-yyyy>` (`transport_search`, split by UID span above a
  `MESSAGELIMIT`, parsed by `parse_search`, tolerating both classic `* SEARCH` and
  extended `* ESEARCH … ALL`) yields the in-window UIDs, and the sync starts at the **lowest** of
  them (older mail is never fetched), reporting their count as the `total` progress
  denominator. No matches yields an empty snapshot that still tombstones stale rows
  below the window. A **QRESYNC delta** issues no `SEARCH`, so the fetch is unbounded; the
  orchestrator drops any arrival the window does not admit, which is what stops an old
  message *filed* into the folder (a fresh UID, since IMAP has no in-place edit) from
  re-entering. With no cutoff (the default) the whole mailbox
  syncs. This is how a host implements "configurable sync depth" without an
  account-wide message delta — the cutoff is a host-supplied calendar date, so this
  crate stays free of any depth/duration policy.
- **Snapshot vs delta.** First sync (no cursor) or a UIDVALIDITY mismatch →
  **snapshot** (rediscover from UID 1, carry `present`). A matching cursor → **delta**.
  On a QRESYNC session with a prior `HIGHESTMODSEQ` baseline the delta is **incremental
  and complete** — flag changes *and* expunges of already-synced messages, plus new
  arrivals, in one round trip (see **CONDSTORE/QRESYNC** below). Without QRESYNC the delta
  **reconciles** the long way (RFC 4549 §4.3; `resync` module): `UID FETCH … (UID FLAGS)`
  over everything below the cursor's `UIDNEXT`, one short line per message, becomes a
  `MailStateChange` each; the arrivals at or above it come whole; and the keys of both are
  the pass's `present` set, so the final chunk tombstones whatever the server no longer
  holds (expunged, or moved by any client). The state changes ride an intermediate chunk,
  because the final one commits as a snapshot and a snapshot carries no partials. A
  sync-depth window bounds both halves through `UID SEARCH SINCE`, and every `FETCH`
  names at most `fetch_batch` UIDs (see **Message limits** below). CONDSTORE/QRESYNC stay
  optional capabilities, not
  assumptions (`providers.md`); the cost of not having them is one flags line per message
  per pass.
- **Message limits and UID mode** (RFC 9738, RFC 9586, RFC 9394). A server advertising
  `MESSAGELIMIT=<n>` may refuse or cut short a command over `n` messages. Every sync `FETCH`
  is capped at `n` (`Negotiated::within_message_limit`), including a page whose caller set no
  limit. A window search (`transport_search`) that returns `n` matches, or is refused with
  `NO [LIMIT]`, is read as cut off: the window is then decided from each message's own
  `INTERNALDATE`, compared by day as `SINCE` compares, fetched `PARTIAL 1:<n>` at a time
  where `PARTIAL` is advertised and in UID spans of width `n` otherwise. Yahoo is why: it
  answers `UID SEARCH SINCE` with the newest `n` matches by date and a plain `OK`, in either
  mode, and cuts before any other criterion, so narrowing by UID finds nothing more; its
  search `PARTIAL` is ignored, and any range not starting at 1 is `BAD`. Its `FETCH`
  `PARTIAL` works as specified, which is what pages by message count rather than by UID.
  Where `UIDONLY` is advertised the client enables it, because Yahoo documents its default
  mode as showing a folder above `n` only in part. On the account tested it did not: the
  default mode showed all 2584 messages, and UID mode only turned an over-limit `FETCH` into
  `NO [LIMIT]` with partial results. Stalwart offers `UIDONLY` as well, so the harness
  suites run in UID mode. Every command already addresses messages by UID, so the
  only change on the wire is that fetch responses arrive as `* <uid> UIDFETCH (…)`, which
  the fetch, body and `IDLE` parsers read as rows numbered by UID.
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


## Authentication: a password or an OAuth 2.0 token

`ImapConfig::new(addr, server_name, credentials)` takes a **`Credentials`** —
`Password` (IMAP `LOGIN`, SMTP `AUTH PLAIN`) or `OAuth2` (a bearer access token over
SASL). That is the same shape `provider-jmap` and `provider-caldav` already expose, and
the same posture as Graph and Google: **the engine stays OAuth-agnostic** — acquiring,
storing and refreshing the token is the host's job (`north-star.md`).

**Every dial asks for its credential.** The config holds a `CredentialSource`, not a
credential: an async trait with `credentials()` (what to present now) and `renew(refused)`
(a replacement after the server refused one, `None` by default). A `Credentials` is its own
source, which is what `ImapConfig::new` wraps; a host whose account signs in with OAuth
passes its token store to `ImapConfig::from_credential_source`. The source is asked once
per connection the account's pool dials and once per SMTP submission, because the pool
dials for as long as the host runs and a token lives about an hour: a pool that presented
the token it connected with would lose every new connection an hour in, while the sessions
already open went on working. A session outlives the token that opened it, so nothing
renews an open one.

- **A refused token is renewed once, on a fresh connection.** An `Auth` refusal asks the
  source's `renew`; a replacement is presented on a new connection (a server may drop a
  connection after a failed sign-in, and counts the attempt either way). One re-dial at
  most. A password's source never renews, so a refused password is never sent again: the
  same secret to the same server gets the same answer, at a provider that may be counting
  towards a lockout. The same rule wraps SMTP, where `AUTH` precedes `MAIL FROM`, so a
  retried submission has sent nothing. Proof: `dial_renewal_tests.rs` and
  `filing_smtp_server_tests.rs`.
- **A source that cannot produce a credential dials nothing.** It fails with
  `ImapError::Credential`, classified as the source classified it: a revoked grant is
  `Authentication`, an unreachable token endpoint is retryable. It is not a session
  refusal, so the pool's ceiling does not move.

**The mechanism is negotiated, never configured.** Two SASL mechanisms carry a bearer
token, and providers disagree about which:

| Provider | IMAP advertises | Source |
| --- | --- | --- |
| Gmail | `AUTH=XOAUTH2` **and** `AUTH=OAUTHBEARER` | **observed** |
| Yahoo / AOL | `AUTH=XOAUTH2` **and** `AUTH=OAUTHBEARER` | **observed** |
| Microsoft 365 | `AUTH=XOAUTH2` only | documented |
| Stalwart | both | documented |

⚠️ **Yahoo's row contradicts Yahoo's own documentation**, which presents `OAUTHBEARER`
as *the* mechanism for its IMAP and never mentions `XOAUTH2` there. The server
advertises both. Both real capability lines are captured verbatim in `sasl_tests.rs`, so
the preference order below rests on observed bytes rather than on a vendor page — which
is the difference between a fix and a guess, and here it inverted the answer.

So `sasl::select` reads the server's own `AUTH=` list and picks. Nothing above this
crate knows which provider it is talking to — a host that had to ask would be the engine
leaking its job outward ("Provider-neutral by default", `AGENTS.md`), and there is
deliberately **no knob**, because a caller able to pin a mechanism would be encoding the
provider in its configuration.

**`OAUTHBEARER` wins where both are offered, and the reason is testability.** Every
server this repo can reach offers both, so the preference decides only which of the two
code paths ever runs live — and `OAUTHBEARER` is the one more likely to be subtly wrong:
it is newer (RFC 7628, 2015), it carries a GS2 header and `host`/`port` pairs `XOAUTH2`
does not, and its rejection is acknowledged with `AQ==` rather than an empty line.
`XOAUTH2` is a decade-old de-facto standard whose exact bytes Google publishes. Given one
live proof to spend, it buys more on the standard mechanism. `XOAUTH2` stays the fallback
and is what a server offering only it (Microsoft 365) gets.

Preferring the other way round was the first attempt here, on the reasoning that
`XOAUTH2` is what every deployed client sends and is therefore the better-trodden path.
That is true, and it is the wrong trade: it would have left `OAUTHBEARER` — the riskier
implementation — running against **no** live server at all, since the argument that Yahoo
would exercise it rested on a documentation claim the server disproves.

- **Both protocols carry the same bytes**, which is why `sasl.rs` is protocol-neutral:
  IMAP frames the blob as `AUTHENTICATE <mech> <base64>` and SMTP as
  `AUTH <mech> <base64>` (RFC 4959 / RFC 4954). `OAUTHBEARER` is a GS2 header plus
  `%x01`-separated pairs — `n,a=<user>,^Ahost=…^Aport=…^Aauth=Bearer <token>^A^A` —
  and `XOAUTH2` the flatter `user=…^Aauth=Bearer …^A^A`. The `host`/`port` describe
  **the connection the response rides on**, so an SMTP submission sends its own host and
  port, never the IMAP session's; `port` is dropped rather than invented when the dial
  address carried no parsable one (RFC 7628 §3.1 makes it optional for a bearer token).
  The initial response is only sent inline where `SASL-IR` is advertised; otherwise the
  client names the mechanism, waits for the bare `+`, and sends the blob on its own line
  — the two-step form every server takes.
- **A rejected token answers with a challenge, not an error — and then waits.** Both
  mechanisms describe the refusal in base64 JSON (`{"status":"invalid_token",…}`) and
  will not send the tagged `NO` (or the SMTP `535`) until the client acknowledges: a
  single `%x01`, base64 `AQ==`, for `OAUTHBEARER` (RFC 7628 §3.2.3), and an **empty
  line** for `XOAUTH2`. A client that skips the acknowledgement does not get an
  authentication error, it gets a **stalled connection** — so this is not politeness, it
  is what turns an expired token into a failure a host can act on. The decoded challenge
  travels in the error, because it is the only place the server says *whether* the token
  was expired, wrongly scoped, or issued for another account.
- **A token is never downgraded into a password attempt.** A server advertising no OAuth
  mechanism fails with an `Auth` error naming what it *did* offer — the usual cause is an
  OAuth credential pointed at an account that only takes a password — and no credential
  reaches the wire.
- **Credential components are screened before encoding.** `%x01` is the key/value
  separator itself, so a token carrying one could append its own `auth=` pair; CR/LF/NUL
  would break out of the command line the base64 rides on. Rejected up front — the same
  guard `smtp.rs` applies to envelope addresses and `engine-rfc5322` to header values.
- **The pre-auth `CAPABILITY` is a third call, not a duplicate of the other two.** The
  session already asks before `STARTTLS` (is the upgrade offered?) and again after
  authenticating (what does *this* session get — servers advertise a different set once
  they know who is asking, which is why that one cannot be optimized away). An OAuth dial
  adds one in between, because the mechanism list and `SASL-IR` are only knowable before
  the credential is sent.
- **Getting a Yahoo token is the hard part, and it is not a code problem.** Yahoo does
  not self-serve mail scopes: `mail-r`/`mail-w` are granted only after a developer-access
  review (<https://senders.yahooinc.com/developer/developer-access/>). `tools/yahoo-oauth`
  drives the flow once that is approved — authorization code, no PKCE, out-of-band
  redirect, HTTP Basic at the token endpoint, all as Yahoo documents. Its `check` command
  prints the scopes a token actually carries, which is the first thing to look at when
  IMAP answers `AUTHENTICATE` with `NO`: a token minted without approved mail scope looks
  perfectly valid and opens nothing.

### Asking before there is a credential: `probe_imap_auth` / `probe_smtp_auth`

A host setting up an account has to decide **which credential to ask the user for**, and
that decision is only the server's to make. `probe_imap_auth(addr, server_name, security,
connector)` and `probe_smtp_auth(…, ehlo_domain, connector)` (`probe.rs`) dial, read the
pre-authentication `CAPABILITY` / `EHLO`, and return an `AuthOffer`: the advertised
mechanisms verbatim, plus `password` and `oauth`. They share `dial::open_secured` with
the real connect, so a probe is byte-for-byte the dial an account would make, minus the
credential.

- **Nothing is presented, and nothing is recorded against the account.** RFC 7628 §3.2.2
  lets a server describe its OAuth configuration — an `openid-configuration` URL
  included — in the challenge it returns when it *rejects* a token, so a client could
  discover the authorization server by sending a deliberately invalid one. This does not.
  It would leave a failed authentication on the user's account before they had signed in
  once, on the screen where they are most likely to give up, and the field is optional:
  Stalwart answers a bad token with a bare `NO [AUTHENTICATIONFAILED]` and no challenge.
  A host discovers the authorization server over HTTPS instead.
- **On a STARTTLS dial, the reading is the post-upgrade one.** A server commonly withholds
  its mechanism list, or advertises `LOGINDISABLED`, until the link is secured, so the
  cleartext capability describes a session nobody will use. Same on SMTP: the `EHLO` that
  negotiated the upgrade is not the one whose `AUTH` line counts.
- **`LOGINDISABLED` and "no `AUTH=` mechanism" are different answers.** The capability
  withdraws the `LOGIN` *command* (RFC 3501 §6.2.3), not the password: a server may
  disable it and still take `AUTH=PLAIN`, and a server advertising no mechanism at all
  still takes `LOGIN`. Reading either as "no password" hides the only route that works.
  Microsoft 365 is the case where both are true at once, and where the honest answer is
  that a password field would be a dead end.
- **SMTP has no such fallback.** Submission authenticates over SASL or not at all
  (RFC 4954), so an `AUTH` line naming no password mechanism means no password.
- **Every failure means one thing to a caller: the question went unanswered.** A refused
  dial, a bad server name, an unadvertised `STARTTLS` — a host offers what works
  everywhere (a password) rather than a sign-in that may not exist.
- **The `EHLO` domain is screened for CR/LF/NUL in `ehlo` itself**, not in each caller:
  the probe is the first thing a setup screen runs, and it takes the domain straight from
  a host, so the guard belongs where the command line is built (`converse` already
  screened the envelope addresses it owns). One CR would append a second SMTP command
  (RFC 5321 §2.3.8).

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
  `APPEND` — a draft or the Sent copy — no server is in the loop to add one).
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
- **Keywords on the Sent copy.** A draft's `sent_copy_keywords` ride the Sent `APPEND` as
  flags, and only where the folder's `PERMANENTFLAGS` carries `\*` (RFC 9051 §7.1):
  without it the server answers `OK` and keeps no new keyword, the same silent success
  `report.rs` refuses. Reading `PERMANENTFLAGS` needs a `SELECT`, which the first attempt
  issues only when the draft asks for keywords, so an ordinary send costs nothing extra;
  the retry reads it from the `SELECT` its probe already makes, and gives a copy it finds
  already placed the keywords with an idempotent `+FLAGS`. A `SELECT` or `STORE` that
  fails leaves them out rather than failing the placement. The receipt's
  `sent_copy_keywords` names the ones the copy carries; a `PERMANENTFLAGS` that lists a
  keyword by name without `\*` counts as not allowing it, since only `\*` is parsed. The
  repair (`refile`) sets them the same way and returns only the key, so what it kept is
  read from the next sync (`place.rs`, `tests/live_imap_sent_keywords.rs`).
- **Folder resolution.** The sent copy / draft is filed into the account's **real
  folder for the role**, discovered via the `\Sent`/`\Drafts` SPECIAL-USE attribute
  in a `LIST` (so a Gmail `[Gmail]/Sent Mail` or a localized name is honored), and
  only when the server advertises none does it fall back to creating the
  conventional `Sent`/`Drafts` name. This costs one `LIST` per submission (rare path).
- **Three transports.** `ImapConfig::with_smtp(addr)` is **plaintext, no auth** — for
  an MX that accepts local mail (the fixture's port 25). `with_smtp_tls(addr,
  server_name)` is **implicit TLS + `AUTH`** (port 465) using the account credentials and
  the injected connector. `with_smtp_starttls(addr, server_name)` is **STARTTLS + `AUTH`**
  (port 587): the client connects in the clear, `EHLO`s, upgrades the socket with
  `STARTTLS`, re-`EHLO`s over TLS, then authenticates — it **fails** if the server does
  not advertise `STARTTLS` (no cleartext auth). Which `AUTH` runs follows from the
  credential (**Authentication** above): `AUTH PLAIN` for a password, `AUTH XOAUTH2` /
  `AUTH OAUTHBEARER` for an access token. AUTH is only ever attempted once the stream is
  secured (implicit TLS, or after the upgrade), never in the clear — which matters more
  for a token than for a password, since a token grants the whole mailbox and is
  replayable until it expires.
- **`EHLO`'s extensions are read line by line, never from the joined reply.** An
  extension keyword only means anything at the start of its own line, so flattening the
  reply first lets a server's greeting prose answer a question it was never asked — the
  SMTP twin of the `Response::untagged` trap that had Dovecot's `List completed …` line
  read as a mailbox. `read_reply_lines` is the form both the `STARTTLS` check and the
  mechanism list use; `read_reply` (joined) stays for replies whose content is prose. The upgrade is negotiated in `smtp::negotiate_starttls` (greeting
  → `EHLO` → `STARTTLS`), then the caller TLS-wraps the socket and the conversation
  continues over `smtp::send_after_starttls` (which skips the greeting a server does
  not re-send post-upgrade). Data buffered past the `STARTTLS` `220` is rejected — a
  command-injection guard (CVE-2011-0411 class), so injected cleartext never crosses
  the TLS boundary.
- **Per-recipient acceptance/rejection** is captured from each `RCPT TO` reply (a
  `250` accept, a `550` reject). The message still goes to the accepted recipients;
  if none accept, it is a permanent rejection with no `DATA`.
- **A server that stops answering never holds a send** (`deadline.rs`, by the shared
  `Deadlines` of `deadlines.md`). The TCP connect and the TLS handshake are each bounded by
  `dial` (30 s); the greeting and every reply before the end of the message by `reply`
  (1 min); each 8 KiB piece of a write by `stall` (1 min); the reply to the final `.` by
  `submission` (10 min, RFC 5321 §4.5.3.2.6). A bound that fires is an `Io` `TimedOut`, so it
  is classified exactly as a connection lost at the same point: **retryable** anywhere before
  the end of the message, because nothing has been submitted and the outbox may send it
  again, and **ambiguous** after it (below), never retried. Unbounded, a server that accepts
  the connection and never speaks (a paused process behind a port forwarder, a captive
  portal, a dead NAT mapping) holds a send forever, in no queue the host can retry or the
  user can edit. The probes (`extensions`, `negotiate_starttls`) share the same reads and
  bounds, and the IMAP dial (`dial::open_secured`) shares `dial`.
- **The `.` that ends `DATA` is the point of no return.** The message text goes first, then
  the hand-over is recorded (`providers.md`), then the `.` line, written by
  `SmtpStream::write_terminator`, which takes the proof of the record. A record that fails
  drops the connection mid-`DATA` with nothing queued (RFC 5321 §4.1.1.4), and the send is
  retried; a `.` that could not be written is as ambiguous as a lost reply.
- **Post-`DATA` disposition.** `2xx` → delivered; `5xx` → permanent rejection;
  `4xx` → transient (retryable — the message was not queued); any **unreadable
  acknowledgement once the message bytes are on the wire** — a dropped connection,
  a reply that did not come within the `submission` bound, *or* a malformed final reply —
  → **ambiguous** (never a plain transport error, so
  an already-sent message is never reported as a clean failure). The
  ambiguous case becomes `ProviderError::needs_confirmation`, which
  `engine_sync::submit_mail` routes to `PendingOutcome::NeedsConfirmation` rather
  than `Failed` — so the outbox never blind-retries and risks a double-send
  (`providers.md`). This is the one cross-crate touch the slice added:
  `engine-provider`'s `ProviderError` gained `needs_confirmation`/
  `requires_confirmation`, and `engine-sync`'s outbox honors it.
- **Sent placement never fails a send, and is never silent.** Delivering and filing are
  two operations here — SMTP dials fresh per send, the `APPEND` rides a pooled IMAP
  connection that may have died since it was last used — so a connection that went stale
  delivers the mail and loses the copy.
  Three rules follow, and the third is the one that was missing:
  1. A delivered send is **never** returned as an error for a filing failure. The mail has
     gone; a caller that saw `Err` would re-send it.
  2. The placement is **retried once on a connection proved alive** (`acquire_checked`: a
     parked one that answers `NOOP`, else a fresh dial), because a dead pooled connection
     is the expected cause, not an exotic one — the failed one is discarded, not parked. The retry first asks whether the copy
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
     so it probes on *every* attempt, first connection and retry alike — a button gets
     pressed twice. It is deliberately **not** outbox-mediated: the outbox exists so a side
     effect is neither lost nor repeated across a crash, and this one is idempotent by
     construction and safe to ask for again.

  With UIDPLUS the `APPEND` returns `[APPENDUID validity uid]` → the receipt carries the
  real Sent key (the same key the next Sent sync synthesizes); without it the receipt key
  is `Message-ID`-derived and the copy reconciles when Sent is synced.
- **A `LIST` name is three things, and they are not the same string.** The **wire form** is
  what the server said, modified UTF-7 on rev1 (RFC 3501 §5.1.3), so `Travel &- Expenses` is
  one folder. The **id** is that name decoded (`utf7::decode`), whole path included: it is
  what `SELECT`/`APPEND`/`CREATE` are given, with `crate::transport` putting the wire form
  back, and what every `imap:v…:u…@folder` key embeds, so a message does not re-key the day a
  server starts offering rev2. **The inbox's id is always `INBOX`**, as is the parent id of a
  folder inside it: the reserved name is case-insensitive (RFC 9051 §5.1) and a host binds the
  inbox by it, so Yahoo's `Inbox` as an id would sync the same mailbox twice under two scopes.
  The folder writes resolve `INBOX` back to the listed spelling for the paths they send. The
  **display name** is the folder's own, the last segment of
  that path, with the nesting carried by `Mailbox::parent` instead. There is deliberately no
  general encoder: no name this crate sends originates from a decoded one.
- **The folder list is a tree, and IMAP is the only transport that spells it in the name.**
  `LIST` gives a flat set of delimiter-joined paths plus the delimiter itself, per row
  (`Archive/2024`, or `INBOX.Archive.2024` on a Courier-shaped namespace); JMAP and Graph
  hand the nesting over as an explicit parent id. So `mail::mailbox_from_list` splits the
  path at the **last** delimiter: the part before it is the parent's id, the part after it is
  the name.
  A host drawing a tree would otherwise indent a row reading `Archive/2024` underneath one
  reading `Archive`. Two traps, both pinned by `mail_tests.rs`: a delimiter at offset `0` is
  **not** a split (`/Archive` and `.Archive` are rooted namespaces, not children of a
  nameless folder), and a `NIL` delimiter means a flat namespace, where a slash in a name is
  a character the user typed. Role matching stays on the **whole** path, so a folder someone
  called `INBOX` inside another one is an ordinary folder.

  **An escaped delimiter is split on as well, and that is a decision, not an oversight.** IMAP
  defines no way to put the delimiter inside a name, so a server holding a folder someone called
  `29/01/2021` has to invent one; Proton Mail Bridge's is a preceding backslash, and it sends
  `29\/01\/2021`. Honouring that escape recovers the name the person typed, and is still the
  wrong answer here, because Bridge lists a **row per fragment** too (`…29\`, `…29\/01\`,
  `…29\/01\/2021`, three rows for one label, the first two with no `STATUS` behind them). Split,
  and those rows are the chain's own links, which is what Thunderbird and Apple Mail draw, both
  greying the intermediates out. Honour the escape, and they become siblings beside the folder
  they are fragments of: one label, three entries. A folder pane that agrees with every other
  client beats one that is privately more correct. Bridge is splitting its own escaped string,
  which is the bug this rule declines to have twice.

  Gmail reads its hierarchy out of a name too and lands elsewhere, because nothing hands it
  fragment rows: `provider-google`'s `nest_labels` splits only where the prefix is a label the
  account actually has, so `Work/Clients` with no `Work` beside it stays one label.
- **A level of the tree that holds no mail stays in the list, as `Mailbox::selectable: false`.**
  `\Noselect` rows (Gmail's `[Gmail]`, the parent of a folder made inside one nobody made) are
  where their children hang, so dropping one orphans them; `SELECT` or `STATUS` on one is an
  error, so a host neither opens nor syncs it, and `unseen` never probes it. `\NonExistent`
  implies `\Noselect` (RFC 9051 §7.3.1), and **the same folder can arrive either way**: for a
  parent nobody created, Dovecot answers the plain `LIST` with `\Noselect \HasChildren` and the
  extended one (`RETURN (…)`, which is what this crate sends wherever it is offered) with a bare
  `\NonExistent`. So `mail::mailboxes_from_list` keeps a `\NonExistent` row while a listed
  folder sits beneath it and drops it otherwise, when it names nothing a host could draw.
  Stalwart never produces either: it makes a real parent. `live_mailbox_writes` pins both.
- **An unknown escape in a quoted string is kept, not refused.** `QUOTED-CHAR` allows `\` only
  before `"` or `\` (RFC 9051 §4.3), so anything else is a server getting it wrong. A protocol
  error there is scoped to the whole response rather than the one string, so refusing takes every
  mailbox or message on the line with it, and one folder nobody can name costs an account its
  entire `LIST`. `tokenize` keeps both bytes verbatim, so the name survives to be split like any
  other; what the escape *means* is the folder-list rule's business, not the tokenizer's.
- **`put_draft` / `delete_draft` (no SMTP).** A draft is `APPEND`ed into the account's
  Drafts folder (resolved by `\Drafts` SPECIAL-USE, else creating `Drafts`) flagged
  `\Draft \Seen`, so keeping a draft works against any IMAP server even where SMTP
  submission cannot. Unlike Sent placement an `APPEND` failure surfaces: saving the
  draft is the whole op. `ImapProvider::save_draft` remains as the first-save
  convenience the example and the live suite use, and is the same code path.
- **A re-save is `APPEND` then `UID STORE +FLAGS (\Deleted)` + `UID EXPUNGE`, in that
  order, and the removal may fail without failing the save.** An IMAP message is
  immutable, so "replace" is two commands with nothing around them; appending first
  means a partial failure leaves the user two drafts rather than none, which is the
  right way round for a message nobody has sent. Returning an error once the new copy
  is stored would be worse still: the caller retries and appends a third. The key
  therefore **moves** on every save, and the caller keeps whatever `put_draft` returned.
- **A key minted without UIDPLUS names the draft by its `Message-ID`.** `APPENDUID`
  (RFC 4315) gives the real key where the server offers it; otherwise the key is
  `draft:<Message-ID>`, and removing it has to `SEARCH HEADER Message-ID` in the Drafts
  folder before it can address anything. Without that path a draft on a server lacking
  UIDPLUS would accumulate one copy per save, which is the failure worth the extra code.
- **A removal whose `UIDVALIDITY` moved is a `Conflict`, not a success.** The UID now
  names a different message, and deleting it would delete someone else's mail; the
  caller re-syncs instead. A UID the folder simply no longer holds is a `UID STORE`
  no-op, so a repeated delete settles rather than parking the op that drives it.

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

## Folder writes

`edit_mailbox` applies a neutral `MailboxEdit` over the provider's one session
(`mailbox_write.rs`; the `MailboxWrites` impl is a lock-and-call).
`Capabilities::mailbox_writes` is unconditional: `CREATE`, `RENAME` and `DELETE` are base
protocol on both dialects.

- **The key is the path, so it moves.** A folder's path is built from the parent's path, the
  hierarchy delimiter `LIST` reported for it, and the leaf name. `RENAME` changes that path and
  the server renames every folder beneath it too (RFC 9051 §6.3.6), so every receipt names the
  path the folder has **after** the edit. Each edit starts with one `LIST` and judges against
  it.
- **Names.** A leaf carrying the delimiter would make levels nobody asked for, and a
  subfolder on a server whose delimiter is `NIL` has nowhere to go; both are `InvalidState`
  before anything is sent, as is any edit of `INBOX` and a top-level folder called `INBOX`.
  Names go through `quoted_name`, so rev1 sends modified UTF-7 (`Reçus` → `Re&AOc-us`).
- **`Create`** → `CREATE "<path>"`, then `SUBSCRIBE`. A path the `LIST` already holds is not
  created again, and `NO [ALREADYEXISTS]` is success: a retry meets what the first attempt
  made.
- **`Update`** → `RENAME "<from>" "<to>"`. `NO [NONEXISTENT]` or `[ALREADYEXISTS]` is a
  `Conflict`, as is a destination the `LIST` already holds (refused without a request).
- **`Trash`** → `RENAME` under the Trash folder. Where Trash is `\Noinferiors`, or the
  `RENAME` is refused with `[CANNOT]`, the folder is emptied instead: each selectable folder of
  the subtree is `SELECT`ed and `UID MOVE 1:* "<Trash>"`, then the subtree is deleted. The
  receipt is then `removed()`, because no folder remains.
- **`Delete`** → `DELETE` of the folder and everything beneath it, deepest first, after an
  `EXAMINE "INBOX"` so no session holds a mailbox that is about to go. `NO [NONEXISTENT]`
  and a target the `LIST` does not hold are success.
- **Subscriptions do not follow a `RENAME`** (RFC 9051 keeps them apart from the mailbox), and
  a client listing only subscribed folders would lose a folder we moved. So a rename
  unsubscribes each old path of the subtree and subscribes each new one, and a delete
  unsubscribes. A refusal of either is ignored: the folder change itself went through.

Proven live on Stalwart (as the scratch account `carol@`) and both Dovecot dialects by
`tests/live_mailbox_writes.rs`: create (including a non-ASCII name), create again, rename with
a child following, a move to the top level and back, a missing target as `Conflict`, a trash
that keeps the subfolder and its mail, and a permanent delete that is repeatable. Emptying the
folders into Trash is proven offline only: all three servers file a folder inside Trash.

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
- **Read-only open + shared guard.** Resolution is shared with the edit path
  (`target.rs`): parse the key, reject a `CR`/`LF` mailbox (`InvalidState`), open, and
  guard `UIDVALIDITY` (mismatch → **`Conflict`**). A body read opens the mailbox with
  **`EXAMINE`** (read-only, `examine_target`), not `SELECT`, so it takes no write-intent
  open, leaves `\Recent` untouched, and works on a read-only folder. A `UID FETCH`
  that returns no data — the UID was expunged since the last sync — is also a
  **`Conflict`** (re-sync, then drop), not a permanent failure.
- **A session remembers the mailbox it has open** (`transport_select.rs`), and a read
  there skips its `EXAMINE`: a UID cannot change during a session (RFC 9051 §2.3.1.1),
  so the `UIDVALIDITY` read at selection holds for as long as the selection does. The
  selection is forgotten *before* a `SELECT`/`EXAMINE` is sent, because a refused one
  deselects (§6.3.1). A write always `SELECT`s again. `ImapPool::acquire_for(mailbox)`
  hands a read a parked connection that already has its mailbox open.
- **A server that goes silent mid-session is a lost connection.** `Connection::read_line`
  waits `reply` (1 min) for each line, so the greeting and every command's response are
  bounded without a call site having to ask; each read of a body literal waits `stall`
  (1 min) of *silence*, never the whole body; and every write, an `APPEND` literal included,
  goes out 8 KiB a piece under `stall` (`deadline::write`). Each fails `Retryable`, and the
  pool discards the connection (`PooledConnection::settle`); a single fetch that lost its
  connection is tried once more on one proved alive (`fetch::fetch_from_pool`). The one
  read without a bound of its own is a connection in `IDLE`, which the watch bounds by its
  keep-alive (below).
- **Batches: one `UID FETCH <set> (BODY.PEEK[])` per mailbox** (`fetch_batch.rs`,
  `Provider::fetch_message_sources`). A warm of thousands of bodies one request at a time
  spends its time on round trips, not bytes; a set costs one round trip plus its bytes,
  and each body is handed on as it parses off the wire (`next_fetch_body`), so a batch
  holds one message in memory at a time. Rows arrive in the server's order and may carry
  their `UID` after the literal (`parse_any_fetch_body`). A stale `UIDVALIDITY` or an
  expunged UID is a `Conflict` for that message alone; a batch that lost its connection
  asks once more, for what it had not received, on a connection proved alive.
  `ConnectionInfo::sources_per_request` (25) and `concurrent_fetches` (the pool's
  `worker_capacity`: the ceiling less the watches, read per call) tell a host how to cut
  and overlap them. Proof: `tests/live_imap_body_batch.rs` against Stalwart and both
  Dovecot dialects, byte-equal with the single fetch, and a control arm asking for UIDs
  the server does not hold.

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
- **A dedicated connection, gated on `IDLE`.** A watcher (`ImapAccount::watch`) takes its
  **own** connection out of the account's pool for its lifetime, separate from the ones
  the `ImapProvider` that syncs the mailbox borrows — a connection in `IDLE` can only send
  `DONE`, so it cannot also `FETCH`. It holds a `WatchLease` on the account's budget,
  released on **drop** (a watch task is aborted, not stopped). Construction `EXAMINE`s the
  mailbox **read-only** (watching never writes or resets `\Recent`; it also reads any
  backlog a reused connection carried) and fails fast with `InvalidState` if the server
  does not advertise `IDLE` — or if the watch would take the account's last worker (see
  **Connections**). One watcher watches one mailbox, mirroring the bound-mailbox sync
  model; the host decides which mailboxes warrant one (usually just INBOX).
- **The notification gap, closed three ways.** `IDLE` delivers unsolicited responses
  *only while a connection is actively idling*, so a change arriving in any other window
  is never re-sent. The watcher closes this by (1) **staying in `IDLE` continuously**
  across `Changed` events — `next()` reports a change without leaving `IDLE`, so a
  message arriving while the host syncs the previous one on its *separate* connection is
  still captured; (2) the host's prescribed loop syncing **once on start and once after
  every reconnect**; and (3) the mandatory **~28-minute keep-alive re-`IDLE`** (under
  RFC 2177's 29-minute rule), which doubles as a liveness probe and a backstop sync
  trigger — and whose pre-re-`IDLE` `DONE` drain converts a boundary change into
  `Changed` rather than swallowing it. The wait in `IDLE` is the one read in the crate the
  `reply` bound does not cover, because silence is what `IDLE` is; the `DONE` drain does
  get it, so a server that never completes `DONE` is a lost connection
  (`watch_tests::an_idle_is_bounded_by_its_keepalive_and_the_done_by_the_reply_bound`).
  The keep-alive interval is the one host-supplied
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

- **Yahoo's OAuth is still unproven end to end.** Both *mechanisms* are proven live
  against Gmail (below), and Yahoo advertises the same two. What has not run is Yahoo's
  OAuth: its mail scope needs a developer-access review, so `tools/yahoo-oauth` cannot yet
  mint a token. Yahoo with an **app password** is live-tested (`live_imap_password.rs`).
  What a Yahoo account meets that the harnesses do not, all observed: it lists its inbox as
  `Inbox`; it advertises **no `CONDSTORE`/`QRESYNC`** (a proprietary `XYMHIGHESTMODSEQ`
  instead, which is why its `SELECT` still reports `HIGHESTMODSEQ`), answers
  `ENABLE QRESYNC` with an empty `ENABLED` and `ENABLE CONDSTORE` with `ENABLED CONDSTORE`,
  so it takes the reconciling delta above. Its documentation lists `CONDSTORE` and
  `QRESYNC`, but once `ENABLE CONDSTORE` is confirmed a `CHANGEDSINCE` fetch is
  `NO [CANNOT]`, so no form of incremental flag sync is open to it. It states
  `MESSAGELIMIT=1000` and confirms `ENABLE UIDONLY`; its search and `PARTIAL` behaviour is
  under **Message limits and UID mode** above. Its `IDLE` reports no expunges (documented),
  which only the reconciling delta catches.
- **CONDSTORE/QRESYNC fallback when unsupported.** The incremental delta (above) is
  **implemented** for servers that advertise QRESYNC (RFC 7162) — the common case
  (Stalwart, Dovecot, Cyrus, Gmail). A server without QRESYNC takes the reconciling delta
  above, which reads every held message's flags each pass. A **CONDSTORE-only** server
  takes it too: we gate the incremental delta on QRESYNC because the `VANISHED` expunge half
  needs it. Using `CHANGEDSINCE` for the flag half on such a server, beside the present set,
  is a possible later refinement, though not for Yahoo, whose `CHANGEDSINCE` is refused.
  Its present set is not paged: one `UID FETCH … (UID FLAGS)` group per `fetch_batch`
  UIDs, all in one pass.
- **QRESYNC delta is a single page.** The QRESYNC delta issues one
  `UID FETCH 1:* (CHANGEDSINCE … VANISHED)` and does **not** honor the `limit`/paging the
  snapshot path uses: a bulk server-side change — "mark all read" — returns every changed
  message in one response and one transaction. Per-page streaming of the delta is a later
  refinement. It still fetches `1:*` regardless of the sync-depth window, which is correct:
  `VANISHED` needs `1:*` to report already-expunged UIDs, and the window must restrict only
  the *upserts*. It does — in the orchestrator, for every adapter's **delta** at once
  (`SyncWindow::admits`, `store-and-sync.md`), so an out-of-window UID above the cursor
  cannot re-enter the store. The snapshot path is not re-checked there: its bound is the
  `UID SEARCH SINCE` above, in the server's own date semantics. An *unsolicited* flag-only `FETCH` (no `ENVELOPE`) that the server
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
  only then authenticate — refusing to proceed (never sending a password *or* a bearer
  token in the clear) if it is not advertised. Because a STARTTLS dial is byte-for-byte an
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
- **Neither is UIDONLY** (RFC 9586), which needs its own `ENABLE` on either dialect.
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
  UIDVALIDITY reset, per-recipient rejection, and post-`DATA` ambiguity. The **SASL
  OAuth** exchanges are covered the same way on both protocols: the two initial-response
  vectors are the **published ones** (RFC 7628 §4.1's and Google's, decoded and
  re-encoded byte for byte, so the code is pinned to the specifications rather than to
  itself), plus the mechanism choice from an advertised set, the `SASL-IR` and prompted
  forms, and — the branch that matters most — a rejected token's challenge being
  acknowledged (`AQ==` / an empty line) so the failure arrives as an error instead of a
  stall. An
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
- **Live OAuth, gated on a real provider (`tests/live_imap_oauth.rs`):** the one thing a
  mock cannot show — that a real server accepts these bytes. Gated on
  `IMAP_OAUTH_HOST`/`USER`/`TOKEN`, over a **verifying** connector (bundled Mozilla
  roots), not the harness's trust-nothing one. Three assertions: the token authenticates
  *and* the session is usable (a `LIST` proves the server really moved into the
  authenticated state rather than answering `OK` in place); a **corrupted token fails as
  `FailureClass::Authentication` within a timeout**, which is what proves the challenge
  acknowledgement works against a real implementation — without it that test hangs rather
  than fails; and, with `IMAP_OAUTH_SMTP_HOST` set, a submission authenticated the same
  way. **Run and green against Gmail** (`tools/google-oauth`'s default scope is already
  `https://mail.google.com/`, the Gmail IMAP/SMTP scope), which exercises `OAUTHBEARER`
  — the preferred mechanism. `XOAUTH2` was proven on the same server by sending this
  crate's exact blob for it by hand (`AUTHENTICATE XOAUTH2 <base64>` over `openssl
  s_client`), which Gmail also answered `OK`; so both mechanisms are confirmed against a
  real server even though only one runs through the adapter. Yahoo is the same three
  environment variables via `tools/yahoo-oauth`, once Yahoo approves developer access to
  the mail scope.
- **Live auth probe (`tests/live_imap_probe.rs`, harness-gated):** the offline suite pins
  the classification against captured capability lines; only a live server shows that a
  **STARTTLS** probe reads the post-upgrade capability rather than the cleartext one, on
  both protocols. It also probes ten times in a row against one server, because Stalwart
  caps concurrent connections per account: a probe that leaked a session would surface
  there as a refused dial rather than as a leak nobody notices until a user has retyped
  their address a few times.
- **Real-provider exploration:** `examples/imap_explore.rs` connects to a *real*
  IMAP server over a verifying TLS connector (Mozilla roots) and lists folders +
  recent mail (read-only; opt-in `IMAP_QRESYNC` verifies the CONDSTORE/QRESYNC delta,
  `IMAP_DRAFT` saves a draft, and `IMAP_SEND` submits over SMTP + implicit TLS). Setting
  `IMAP_TOKEN` instead of `IMAP_PASS` runs the whole exploration on an OAuth credential,
  so pointing it at a new provider is an env var rather than an edit. Validated against a real Dovecot server — read, UTF-8 subjects, and draft
  creation; authenticated SMTP send is implemented and offline-tested, exercisable via
  `IMAP_SEND`. The **CONDSTORE/QRESYNC delta** was validated read-only against
  **Soverin** (Dovecot): it advertises `CONDSTORE QRESYNC` post-auth, `SELECT (CONDSTORE)`
  returns `HIGHESTMODSEQ`, and the `IMAP_QRESYNC` check confirms the second sync is an
  incremental `Delta` (changed/removed ≈ 0, no re-snapshot) rather than re-listing the
  mailbox — the same path the live Stalwart test exercises with a full mutate→delta
  cycle. This is the "external provider smoke test" `north-star.md` step 7 anticipates,
  ahead of schedule.

## Reporting a message (junk / not junk / phishing)

`UID STORE` then `UID MOVE`, in `crate::report`. As on JMAP the report is the registered
keyword — `$Junk`, `$NotJunk`, `$Phishing` — but IMAP keeps the flag and the folder in
different verbs, so it is two commands rather than one.

**`PERMANENTFLAGS` is read before the write, and it is the only probe IMAP offers here.**
RFC 9051 §7.1 makes `\*` the server's statement that a client may create new keywords.
Without it a server answers `UID STORE +FLAGS ($Junk)` with a plain `OK` and stores nothing
— the report would read as delivered and be absent on the next `FETCH`. `SelectData` now
carries `permanent_flags_allow_new`, and a mailbox without it makes the report an
`InvalidState` rather than a silent no-op.

**That refusal branch has no server to exercise it.** Stalwart and both Dovecot dialects
advertise `\*`, so the live suite can only pin the *premise* (`live_imap_report.rs` →
`every_configured_server_permits_new_keywords`); the offline suite is what proves the check
exists and refuses. Stated plainly because a test that cannot fail is the thing this file
keeps warning about.

A report into the message's own mailbox skips the `UID MOVE`: moving into the selected
mailbox is a copy-and-expunge onto itself on some servers, and a no-op is cheaper than
finding out which. The move mints a new UID, so the receipt names the **source** key — the
same contract as `MailEdit::MoveTo`.
