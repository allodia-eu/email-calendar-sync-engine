# Waiting on a silent server

**Every wait on a server is bounded by one of six categories in
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
| `dial` | 15 s | each of the TCP connect (name lookup included) and the TLS handshake | A few round trips to a server that is up, which a congested mobile link completes in seconds; fifteen seconds still covers the first three SYN retransmissions. Unbounded, a connect waits out the OS's SYN retries (over two minutes on Linux) and a handshake waits forever. |
| `greeting` | 15 s | a server's first line on a connection it has just accepted: an IMAP or SMTP greeting | Owed before the client has asked anything, so the server has no work to do for it. A greeting delayed against spam is seconds, and belongs to the port that receives mail from other servers, not a submission port. |
| `setup` | 30 s | each SMTP reply after the greeting and before the message: `EHLO`, `STARTTLS`, `AUTH`, `MAIL`, `RCPT`, `DATA`'s `354` | Answered in well under a second, some after a lookup on the server's side (a credential, a recipient). Shorter than `reply` because a sender is waiting, and safe because nothing has been handed over: a timeout here is retried. RFC 5321 §4.5.3.2's five minutes are written for relays between busy MTAs. |
| `reply` | 1 min | an answer the server owes to a request it has in full: each line of an IMAP response, the head of an HTTP response | Most requests are answered in well under a second, but a server that is working can be silent for a while before a read's answer (a `SEARCH` over a large mailbox, a `REPORT` over years of events). |
| `stall` | 1 min | each piece of a transfer under way: each read of a body, each 8 KiB (IMAP, SMTP) or 16 KiB (HTTP) piece of a body being sent | A bound on silence, never on size, so a large body that keeps moving is never cut off. |
| `submission` | 10 min | the answer to a request that submits a message: SMTP's reply to the final `.`, a JMAP `EmailSubmission/set`, a Graph `sendMail`, a Gmail `messages.send` | RFC 5321 §4.5.3.2.6. The server may still be filtering, and this is the one wait whose expiry cannot say what happened, so a shorter bound asks the user "did it send?" about a server that was merely slow. |

`dial` is shorter than `reply` on purpose: an HTTP request without a body counts its reply
bound from the send, connect included, which is only sound while the connect gives up first.
`Deadlines`' own tests pin that order, and that `greeting` and `setup` stay shorter than
`reply`.

### Why a send's waits are shorter than a read's

A user who sends against a server that is not answering should learn it in seconds, so the
send can wait in the outbox to be retried rather than read as under way for minutes. Every
wait before the hand-over is therefore short: `dial`, `greeting`, then `setup` for each SMTP
command. A send that meets nothing at all fails after 15 s at the connect, a server that
accepts and never greets after 15 s, and one that stops answering mid-conversation within
30 s of its last reply. None of these can have delivered anything, so each is `Retryable`.

The two waits that stay long on a send are the ones a short bound would get wrong: `stall`,
because a large message on a slow uplink keeps moving however long it takes, and
`submission`, because its expiry cannot say whether the message went.

Reads keep `reply`. An IMAP command's response and an HTTP response head are not shortened
for a send's sake, because a working server can be silent before a read's answer for longer
than a send's command needs, and cutting a sync off there turns a slow server into a failing
one. An HTTP submission has no command exchange before its message: its request goes out
under `stall` and its answer waits `submission`.

## Where each transport applies them

- **IMAP and SMTP** (`provider-imap/src/deadline.rs`): `connect` and `handshake` take `dial`;
  each protocol's `read_greeting` takes `greeting`; `Connection::read_line` takes `reply`, so
  every IMAP command's response is bounded without a call site having to remember, as
  `SmtpStream::read_reply_lines` takes `setup` for every SMTP reply; a `BODY[]` fetch reads
  under `stall`; every write goes out through `deadline::write`, 8 KiB a piece under `stall`;
  `smtp::converse` waits `submission` for the end of data.
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
| IMAP, SMTP | greeting | `greeting` | `Retryable` |
| IMAP | `LOGIN`, `AUTHENTICATE`, `CAPABILITY`, `ENABLE`, `STARTTLS`, `SELECT`/`EXAMINE`, `LIST`, `SEARCH`, metadata `FETCH`, `STORE`, `MOVE`, `EXPUNGE`, `CREATE`, `APPEND`'s continuation and completion, `IDLE`'s start and `DONE` | `reply` per line | `Retryable` |
| IMAP | a `BODY[]` literal, single or batched | `stall` per read | `Retryable` |
| IMAP | every write, an `APPEND` literal included | `stall` per 8 KiB | `Retryable` |
| SMTP | `EHLO`, `STARTTLS`, `AUTH`, `MAIL`, `RCPT`, `DATA`'s `354` | `setup` per line | `Retryable` |
| SMTP | the message | `stall` per 8 KiB | `Retryable`: its end never left |
| SMTP | the final `.` itself (written after the hand-over) | `stall` | `NeedsConfirmation`: it may have reached the server |
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

**An answer is the server's own unless it says otherwise.** After a submission may have been
received, a status the server sends is a refusal and keeps its class, with two exceptions
that are lost answers in disguise: a gateway's `502` or `504` says the server behind it gave
none, and a success whose body cannot be read (or a JMAP response missing the call) is an
answer nobody can read. Both are `NeedsConfirmation`. A submission whose hand-over could not
be recorded never sends its end (`SendError::withheld`), so it was not received and is
`Retryable` (`store-and-sync.md` → "A send survives its process").

## Testing

Every suite runs on tokio's paused clock under a one-hour guard, so a bound costs no wall
time and a wait with none fails as a hang rather than hanging the run: `deadline_tests.rs`
in `engine-http`, each HTTP adapter and `provider-imap`, plus `deadline_imap_tests.rs`,
`deadline_submit_tests.rs` and the watch suites. `engine_http::test_server` (behind the
`test-server` feature, which only dev-dependencies turn on) is the silent and the trickling
server the HTTP adapters share.

⚠️ **On the paused clock, bytes in flight lose to every timer.** tokio moves the paused clock
to the next timer whenever every task is waiting, and a task waiting for a socket counts as
waiting while the kernel is still delivering bytes to it. How soon loopback reports them is the
operating system's business: Linux usually reports them before the runtime next looks, macOS
often does not, and a reply then meets a one-minute bound it never spent ("the server sent
nothing for 60s", with the head on its way). Waiting longer or retrying would hide that, not
remove it, so no exchange a test expects to complete runs with the clock free:

- **The clock is held while the exchange happens.** A blocking task inhibits tokio's advance
  for as long as it runs; `engine_http::test_server::hold_clock` runs one until it is dropped,
  and `held_until(event, work)` holds it until an event the test names has happened. A test
  about a silent server holds it until the server has the request
  (`SilentServer::has_received`), so the silence is the only thing the clock passes; one whose
  connection is never answered holds it until the connection is accepted.
- **The trickling server holds it while its bytes are on their way**: from the start until the
  client has the head, and from each piece until the client has read it, so only the pauses
  between pieces pass on the clock. The client's side of that is the funnel counting each step
  a reply takes on its thread (`deadline::PROGRESS`, compiled only into test builds).
  `trickling_late` writes every piece late from another thread, which is what macOS does to
  loopback, and its test measures exactly the server's own pauses.
- A transfer the client sends is driven without a socket where it can be (the upload test polls
  the request body directly).
- `engine-http`'s throttle suites run with the bounds off on their thread
  (`send_test_support::client`), because they answer every request and test something else.
  IMAP's suites run over in-memory streams, which the paused clock measures exactly.

## Known gaps

- **Writes other than submissions keep their lost-connection class.** A JMAP
  `CalendarEvent/set` carries no lost-update guard, so a replay after a lost reply can repeat
  a scheduling message the server sent. A timeout there behaves as a reset connection always
  has; the guard is the fix, not the timeout.
- **A JMAP send's requests before its submission wait `reply`, not `setup`.** `submit::send`
  reads the account's mailboxes and identity, and uploads any attachment, as ordinary
  requests before the one that submits, so a JMAP server that completes the handshake and
  then says nothing holds a send for a minute where an SMTP server would hold it for 30 s.
  Graph and Gmail submit in one request and have no such step. Giving those requests
  `setup` needs an `Exchange` of their own in `engine-http`.
- `dav-cli` and the live suites build reqwest clients from `reqwest_builder` directly and are
  not bounded. They are tools, and a hang there is visible to whoever is running them.
