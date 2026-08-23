# HTTP throttling

**Every HTTP adapter sends through `engine_http::send_retrying`.** One `429` is answered one
way, whichever server sent it. Adding a `.send()` anywhere in `provider-google`,
`provider-graph`, `provider-jmap` or `provider-caldav` puts a fifth answer in the tree.

`engine-http` is the sibling of [`engine-tls`](tls.md): a cross-cutting HTTP concern a host
configures once and every provider inherits.

## What is waited out

| Reply | Retried | Why |
|---|---|---|
| `429` | every method | The request was refused, not performed. A replay cannot duplicate it. |
| `503` | idempotent methods only | The server may have applied it and failed on the way back. A replayed `POST` is a message sent twice. |
| everything else | no | Including `5xx`: a failed pass is repeated by the sync above, and a blind retry of a write is not safe. |

`Retry-After` wins where the server sends one — guessing shorter than a number the server
named is what turns one throttle into several. Only the **delta-seconds** form is read; the
HTTP-date form is legal (RFC 9110 §10.2.3), is not sent by any of these services, and would
mean trusting the local clock against the server's.

Jitter is added either way, including on top of `Retry-After`. The requests being throttled
are concurrent, and twenty of them backing off by an identical amount retry in lockstep.

Two bounds, stopping different things: **attempts** (5) stops a server that keeps saying `429`
with a short `Retry-After`; **budget** (60 s total) stops a *single* long `Retry-After` from
parking a task. Exceeding the budget hands the work to the next pass, which is right — a pass
that gives up costs a delay, a task asleep for two minutes is indistinguishable from a hang.

## Reporting: the engine has no logger

Nothing in the engine calls `log` or `tracing`; a host owns its I/O. A wait a user would
otherwise experience as an unexplained stall is reported through `ThrottleObserver`, which the
host implements and logs. A host wires one the way it wires `SyncObserver`.

A `ThrottleEvent` carries the provider label, status, attempt, delay, whether the server named
the delay, and whether this attempt gave up. **It carries no URL** — a request path on a mail
API names a mailbox or a message, and these events are written to a diagnostic log a user
attaches to a support request.

## What each provider does

Surveyed across every adapter, because a throttle is not a provider-specific symptom:

| Adapter | Signals a throttle as | Concurrency ceiling | Where the ceiling comes from |
|---|---|---|---|
| `provider-google` | `429`, and `403` with a rate-limit `reason` | 20 per mailbox | Measured: 20 clean, 30 occasional, 50 throttles a tenth |
| `provider-graph` | `429` (+ `Retry-After`) | 4 per mailbox | Documented `MailboxConcurrency`, and measured: 4 clean, 6 draws a `429` |
| `provider-jmap` | `429`, method `rateLimit`/`overQuota`, **and a `400`** (below) | the session's `maxConcurrentRequests` | The server states it (RFC 8620 §2) |
| `provider-caldav` | `429` | not fanned out | No per-object fetch to overlap: `calendar-multiget` batches |
| `provider-imap` | n/a | 1 | Not HTTP, and one connection is one command at a time |

**JMAP's is a `400`.** RFC 8620 §3.6.1 returns *every* request-level error with a `400`,
`urn:ietf:params:jmap:error:limit` among them, and its `limit` property names which limit was
hit. Only `maxConcurrentRequests` is one a client clears by waiting — `maxSizeRequest` and
`maxCallsInRequest` describe the request that was sent. Read as a bare status this is
`Permanent`, and a body dropped as permanent is one no later pass fetches again.

Note also that RFC 8620 scopes `maxConcurrentRequests` to the API endpoint and defines no
companion for downloads — but a server may apply one number to both, and Stalwart does:
exceeding it on a blob download is refused, not queued. So it is what bounds a body warm too,
and it defaults to `1` when a session omits it rather than to a guess.

## Known gaps

- A transport failure (connection reset, timeout) is not retried here. Whether the server
  acted is unknowable from this layer, and the sync pass above already repeats a failed pass.
- `Method::is_idempotent` answers `false` for the WebDAV extension methods, so a `503` on
  `PROPFIND`/`REPORT` is not retried even though their own RFCs say a replay is safe.
  Conservative in the safe direction.
- Google's `403`-with-a-rate-limit-reason is classified as `RateLimited` by the adapter but is
  not retried by `send_retrying`, which reads status alone. Sniffing a body to decide would
  put provider knowledge in the shared layer; the adapter's classification is what carries it.
