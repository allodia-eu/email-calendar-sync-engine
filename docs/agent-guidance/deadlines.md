# Waiting on a silent server

**Every wait on a server is bounded by one of four categories in
`engine_provider::Deadlines`, and every transport uses the same values.** A server that
accepted the connection and then says nothing (a paused process behind a port forwarder, a
captive portal, a stalled middlebox, a NAT mapping that died) reports nothing either. Left
unbounded, a sync holds its lane and a send sits in no queue anyone can retry or edit.

## The bounds

`Deadlines::STANDARD` is the one value. A host cannot choose another: each figure is a
judgement about servers rather than about a device, and a bound tuned per platform would make
one provider's silence a failure on one device and a hang on another. The suites need no
other value either, because they run on tokio's paused clock.

| Category | Value | What it bounds | Why this value |
|---|---|---|---|
| `dial` | 30 s | each of the TCP connect (name lookup included on HTTP) and the TLS handshake | A few round trips to a server that is up. Unbounded, a connect waits out the OS's SYN retries (over two minutes on Linux) and a handshake waits forever. |
| `reply` | 1 min | an answer the server owes to a request it has in full: a greeting, each line of an IMAP response, each SMTP reply before the message, the head of an HTTP response | Servers answer an interactive client in well under a second. RFC 5321 §4.5.3.2's five minutes are written for relays between busy MTAs. |
| `stall` | 1 min | each piece of a transfer under way: each read of a body, each 8 KiB (IMAP, SMTP) or 16 KiB (HTTP) piece of a body being sent | A bound on silence, never on size, so a large body that keeps moving is never cut off. |
| `submission` | 10 min | the answer to a request that submits a message: SMTP's reply to the final `.`, a JMAP `EmailSubmission/set`, a Graph `sendMail`, a Gmail `messages.send` | RFC 5321 §4.5.3.2.6. The server may still be filtering, and this is the one wait whose expiry cannot say what happened, so a shorter bound asks the user "did it send?" about a server that was merely slow. |

`dial` is shorter than `reply` on purpose: an HTTP request without a body counts its reply
bound from the send, connect included, which is only sound while the connect gives up first.
`Deadlines`' own tests pin that order.

## Where each transport applies them

- **IMAP and SMTP** (`provider-imap/src/deadline.rs`): `connect` and `handshake` take `dial`;
  `Connection::read_line` takes `reply`, so the greeting and every command's response are
  bounded without a call site having to remember; a `BODY[]` fetch reads under `stall`; every
  write goes out through `deadline::write`, 8 KiB a piece under `stall`; `smtp::converse`
  waits `submission` for the end of data.
- **HTTP** (`engine-http/src/deadline.rs`): the client every adapter builds with
  `engine_http::client(tls)` carries `dial` as reqwest's connect timeout, which covers the
  lookup, the connect and the handshake together. `send_retrying` hands reqwest the request
  body in pieces and watches them go: `stall` between pieces, then `reply` (or `submission`,
  for an `Exchange::Submission`) once the last one is out. `Sent::bytes` and `Sent::text`
  read each chunk under `stall`.

reqwest's own `timeout` and `read_timeout` are not used. `timeout` bounds the whole request,
and `read_timeout` starts with the request and is not reset while the body goes out; either
would cut off a slow upload of a large attachment.

### Two streams are silent on purpose

| Stream | Its bound instead | Why |
|---|---|---|
| IMAP `IDLE` (`idle::idle_wait_change`) | the watch's keep-alive (28 min by default, RFC 2177), after which `DONE` is sent and its completion waits `reply` | Silence is what `IDLE` is. |
| JMAP EventSource (`watch::quiet_bound`) | the `ping` interval the watcher asked for, plus `reply` for the ping that is owed | RFC 8620 §7.3 has the server ping whenever the interval passes without an event, so a longer silence is a dead connection. |

## Classification: a timeout is a lost connection, and a send is never repeated

A bound that fires fails the operation exactly as a connection lost at that point does. For
everything but a submission that is the class the transport already gave a lost connection:
`Retryable` on every adapter (`ImapError::Io`, and each HTTP adapter's `transport_class`,
which is `Retryable` for everything but a body that did not decode).

**A submission is the exception, and the reason the HTTP funnel reports more than a
timeout.** `engine_http::SendError::may_have_been_received` is `false` only when the server
cannot have had the whole request: the connect or handshake never completed (a reqwest
connect error), or the server stopped taking the body before its end. Anything later, a
silent reply included, may have sent the message, so `into_submission_error` in each mail
adapter turns it into `ProviderError::needs_confirmation`, which the outbox parks as
`PendingOutcome::NeedsConfirmation` and never retries. reqwest cannot say whether a request
head reached a pooled connection's server, so the funnel assumes it may have: the answer
that never sends a message twice.

| Provider | Operation | Bound | A timeout (or lost connection) there is |
|---|---|---|---|
| every one | connect, TLS handshake | `dial` | `Retryable`: nothing reached the server |
| IMAP | greeting, `LOGIN`, `AUTHENTICATE`, `CAPABILITY`, `ENABLE`, `STARTTLS`, `SELECT`/`EXAMINE`, `LIST`, `SEARCH`, metadata `FETCH`, `STORE`, `MOVE`, `EXPUNGE`, `CREATE`, `APPEND`'s continuation and completion, `IDLE`'s start and `DONE` | `reply` per line | `Retryable` |
| IMAP | a `BODY[]` literal, single or batched | `stall` per read | `Retryable` |
| IMAP | every write, an `APPEND` literal included | `stall` per 8 KiB | `Retryable` |
| SMTP | greeting, `EHLO`, `AUTH`, `MAIL`, `RCPT`, `DATA`'s `354` | `reply` | `Retryable` |
| SMTP | the message | `stall` per 8 KiB | `Retryable`: its end never left |
| SMTP | the reply to the final `.` | `submission` | `NeedsConfirmation` |
| JMAP | session, method calls (sync and every `/set`), blob upload and download | `stall`, then `reply` | `Retryable` |
| JMAP | the `Email/set` + `EmailSubmission/set` request | `stall`, then `submission` | `NeedsConfirmation` once it may have been received, `Retryable` before |
| JMAP | the EventSource stream | `ping` + `reply` per read | `Retryable`: the host reconnects |
| Graph | reads, deltas, `$value`, every write (`PATCH`, `POST`, `DELETE`, drafts) | `stall`, then `reply` | `Retryable` |
| Graph | `sendMail` | `stall`, then `submission` | `NeedsConfirmation` once it may have been received, `Retryable` before |
| Google | Gmail, Calendar and People reads and writes, drafts, `messages.insert` | `stall`, then `reply` | `Retryable` |
| Google | `messages.send` | `stall`, then `submission` | `NeedsConfirmation` once it may have been received, `Retryable` before |
| CalDAV, CardDAV | `PROPFIND`, `REPORT`, `OPTIONS`, `GET` | `stall`, then `reply` | `Retryable` |
| CalDAV, CardDAV | `PUT`, `DELETE`, a `PUT` the server schedules from included | `stall`, then `reply` | `Retryable`: the precondition the write carries makes the replay safe (`caldav.md`) |

## Testing

Every suite runs on tokio's paused clock under a one-hour guard, so a bound costs no wall
time and a wait with none fails as a hang rather than hanging the run: `deadline_tests.rs`
in `engine-http`, each HTTP adapter and `provider-imap`, plus `deadline_imap_tests.rs`,
`deadline_submit_tests.rs` and the watch suites. `engine_http::test_server` (behind the
`test-server` feature, which only dev-dependencies turn on) is the silent and the trickling
server the HTTP adapters share.

⚠️ **On the paused clock, a wait for a real socket moves the clock to the next timer, even
when the socket already holds bytes.** tokio advances on every park unless the time driver
itself was woken. So against a server that *answers*, each wait for the socket spends paused
time it never spent, and three such waits can reach a one-minute bound. The consequences:

- Fetch what a test needs answered in real time, then call `tokio::time::pause()` for the
  silence (the JMAP suite does this for the session).
- Measure a silence exactly only where the bound is counted from the send. A bound counted
  from the last piece of a request body starts wherever the clock had jumped to, so those
  tests accept up to a `dial` bound more.
- A transfer that must keep moving for minutes is driven without a socket where it can be
  (the upload test polls the request body directly) or kept to small pieces at a cadence well
  inside the bound (the download tests).
- `engine-http`'s throttle suites run with the bounds off on their thread
  (`send_test_support::client`), because they answer every request and test something else.
  IMAP's suites run over in-memory streams, which the paused clock measures exactly.

## Known gaps

- **A `5xx` to a submission is still `Retryable`.** A gateway may answer `502` after the
  server accepted the message, and a retry would send it twice. That is a status, not a
  timeout, and changing it belongs with the outbox's handling of submissions rather than here.
- **Writes other than submissions keep their lost-connection class.** A JMAP
  `CalendarEvent/set` carries no lost-update guard, so a replay after a lost reply can repeat
  a scheduling message the server sent. A timeout there behaves as a reset connection always
  has; the guard is the fix, not the timeout.
- `dav-cli` and the live suites build reqwest clients from `reqwest_builder` directly and are
  not bounded. They are tools, and a hang there is visible to whoever is running them.
