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
named is what turns one throttle into several. **Both** forms are read (RFC 9110 §10.2.3):
delta-seconds, and the HTTP-date form in all three syntaxes RFC 9110 §5.6.7 requires a
recipient to accept.

A date is honoured **only if it is strictly in the future**. An HTTP-date is always GMT, so
there is no zone to get wrong, but the delay it implies is measured against *this device's*
clock rather than the server's — a device an hour slow would otherwise read "available at
07:28 GMT" as an hour of sleeping. A date already past falls through to the backoff schedule;
so does a zero wait from either form, because having just been refused, retrying in the same
instant spends an attempt to learn nothing. A date far in the future needs no separate guard:
the total-wait budget declines it exactly as it declines a large delta-seconds.

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

| Adapter | Signals a throttle as | Concurrency ceiling | Evidence | Narrows the gate |
|---|---|---|---|---|
| `provider-graph` | `429` (+ `Retry-After`) | **4 per mailbox** | Documented `MailboxConcurrency`, and measured: a sharp cliff, 4 clean over 460 requests and 5 refused 13% | **yes**, in `HttpTransport::new` |
| `provider-jmap` | `429`, method `rateLimit`/`overQuota`, **and a `400`** (below) | the session's `maxConcurrentRequests` | **The server states it** (RFC 8620 §2) — 4 on Stalwart, 10 on Fastmail | **yes**, in `JmapClient::connect`, once the session resolves |
| `provider-google` | **`403` with a rate-limit `reason`**, `429` in theory | **none found** — clean at widths 5, 20 and 50 | Measured; see "Gmail bounds rate, not width" | yes, at `MAX_CONCURRENT_GETS`, but **not as a server ceiling** — see below |
| `provider-caldav` + CardDAV | `429` (Stalwart); SabreDAV refuses nothing | **none found** — clean at widths 1–64 on both servers | Measured on two implementations; `provider-caldav/tests/live_concurrency.rs` | **no** |
| `provider-imap` | n/a | 1 **per connection** | Not HTTP, and one connection is one command at a time | n/a — not an HTTP adapter |

⚠️ **Only Graph has a concurrency ceiling that a gate is the right instrument for.** JMAP's is
honoured because the server states a number and ignoring a stated limit is indefensible
whether or not it bites. Everywhere else the thing that refuses is a **rate**, and a width
bound does not control a rate: halving the width of a drain that then runs twice as long
delivers the same requests per second. Keep the two quantities apart — conflating them is how
the Gmail figure below came to be wrong for months.

⚠️ **`provider-imap`'s `1` is the odd one out among the *widths*, and it is the reason
`min`-ing these into a fan-out is wrong.** Every HTTP adapter's number is a bound on the
*account*: one mailbox, one user, one session, however many providers address it. IMAP's is a
bound on *one socket*, and an account holds one per folder — so an IMAP account with thirteen
folders can genuinely run thirteen commands at once. Read the two as the same kind of number
and you serialize IMAP to fix Graph.


## The account gate

**`engine_http::RequestGate` is the bound on concurrency; the fan-out is not.** A host builds
**one gate per account**, passes it to `RetryConfig::gated`, and hands clones to every provider
it connects for that account — folder-bound mail providers, calendar, contacts. `send_retrying`
holds one lane of it per call, the backoff sleeps included, because a request waiting out a
`Retry-After` is still a request in progress.

The **adapter** states the number, via `RequestGate::narrow_to`, as it connects. A host that had
to know it would be branching on which provider it is talking to, which is the engine leaking
(`providers.md`). Narrowing only ever narrows, so an account whose adapters disagree takes the
strictest.

### Why not just clamp `MAX_CONCURRENT_FOLDERS`

That was the proposal in #216, and it is wrong for two measured reasons. Against a live
Microsoft 365 mailbox of 56 folders, alternating widths inside one run (the methodology
`provider-graph/src/provider.rs` insists on, since the same width minutes apart differed by 40%):

| Concurrent requests | 1 | 2 | 3 | 4 | 5 | 6 | 8 |
|---|---|---|---|---|---|---|---|
| refused | 0% | 0% | 0% | **0%** (460 requests) | **13.4%** | 22.4% | 40.3% |

Every refusal, all 107 of them: `ApplicationThrottled: Application is over its MailboxConcurrency
limit`. So it is concurrency, not the requests-per-window budget — that never came into it.

1. **The ceiling is not per folder**, so `min(fan-out, concurrent_fetches)` fixes the wrong
   number on one adapter and breaks another (the IMAP note above).
2. **Folder sync is not the only traffic.** The same mailbox, given four folder syncs plus one
   body warm — five requests, two endpoint families — refused at 8.2%, and refused *both*
   families: 41 folder pages and 19 `$value` fetches in one block. A bound that sees only the
   fan-out cannot hold a total the server counts across everything.

`MAX_CONCURRENT_FOLDERS` therefore stays 5 and stays a *store* figure — how many folders contend
for the store's single write connection. A folder task that cannot get a lane waits at the gate,
which costs a parked future and nothing else.

### Gmail bounds rate, not width — and the old figure here measured the wrong thing

This file used to record Gmail's ceiling as "20 per user — measured: 20 clean, 30 occasional,
50 throttles a tenth". That was a **width** conclusion drawn from a **rate** experiment, and it
is wrong in both directions: 50 is clean, and 5 is not, depending only on what ran before.

Run flat out with five-second pauses, a *fixed* width of 5 goes 0% → 21% → 82% refused across
three consecutive blocks. Nothing about the width changed; the quota emptied. Re-run rested —
75-second gaps, widths in randomised order, 100 requests a block:

| width | blocks | ok | refused | mean req/s |
|---|---|---|---|---|
| 5 | 2 | 200 | **0** | 14.7 |
| 20 | 2 | 200 | **0** | 29.6 |
| 50 | 2 | 200 | **0** | 50.5 |

**No refusals at any width.** Throughput scales linearly with width, which is what "no
concurrency ceiling in this range" looks like. What Gmail actually enforces it says itself, in
the refusal body — no guessing required:

```
403 PERMISSION_DENIED  rateLimitExceeded
"Quota exceeded for quota metric 'Total Query Cost' and limit 'Units per minute per user'"
  quota_limit:       totalQueryCostPerMinutePerUser
  quota_limit_value: 6000
  quota_unit:        1/min/{project}/{user}
```

**6,000 cost units per minute, per user, per project** — a budget spent over a rolling window,
which is why the same width is clean on a rested quota and refused on a drained one, and why
every "how wide can we go" measurement here has to start from rest.

Across ~1,600 observed refusals, **every single one was a `403`. Gmail never sent a `429`.**

Two consequences, and neither is comfortable:

- **The gate does not address Gmail's limit.** `provider-google` still narrows it to
  `MAX_CONCURRENT_GETS`, and that is worth doing for a different reason — an account's mail,
  calendar and contacts providers would otherwise each run their own 20-wide fan-out against
  one user's quota — but it is the adapter's own measured fan-out width applied account-wide,
  **not** a server ceiling. Do not describe it as one.
- **`send_retrying` never waits out a `403`.** `retryable()` reads status alone (`429`, or
  `503` when idempotent), so Gmail's *only* refusal shape falls straight through to the
  adapter, which classifies it `RateLimited` and fails the scope. The work is delayed by a
  whole pass rather than by a backoff. That was filed under Known gaps as an edge case; it is
  not an edge case, it is Gmail's normal behaviour under load.

### DAV bounds rate too, on one server and not at all on the other

Measured on both harness implementations, `PROPFIND` + `calendar-query` REPORT, six-second
blocks (`provider-caldav/tests/live_concurrency.rs` is the suite, and it keeps this honest).
Refusal *shares* move between runs — 150/s gave 11.5% once and 4.8% the next time — so the
shape is the result, not the numbers:

| | Stalwart | SabreDAV |
|---|---|---|
| fixed 25/s, widths 1 · 4 · 16 · 64 | 0% refused throughout | 0% refused throughout |
| fixed width 8, rates 10 · 50 · 150 · 400/s | 0% · 0% · **a few %** · **tens of %** | 0% throughout |

So **neither bounds concurrency**; Stalwart bounds rate (tripping between 50/s and 150/s, and
saying so with a proper `429`), and SabreDAV refused nothing at all up to 400/s. The second arm
matters as much as the first: without a rate that *does* provoke a refusal, "clean at every
width" would just mean the probe never pushed hard enough.

That is why no DAV adapter narrows the gate — now on evidence rather than on nobody having
looked.

### One gate, one account — the scope is the whole difficulty

The number is easy and the thing it attaches to is not. A host had already written this bound by
hand, correctly set to 4, as a semaphore field on its per-account token source — and then built
the core twice in one process, which is ordinary on Android where a one-time sync worker and the
periodic one overlap. Two cores, two semaphores, eight concurrent requests, and a mailbox
refusing 40% of them. That host had *already* moved its credential state to be shared per account
per process for exactly this reason; the semaphore was left behind.

So: the gate belongs wherever the host's per-account state lives, not on any one provider, and
not on a `RetryConfig` shared process-wide — the policy and the observer genuinely are
process-wide, so clone it per account through `gated` rather than building a second.

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

- **Nothing waits out a rate limit, only a concurrency one.** `send_retrying` retries `429`
  (and `503` when idempotent) and nothing else. That is exactly right for Graph, whose refusal
  is a `429` carrying a `Retry-After`. It is exactly wrong for Gmail, whose refusal is a `403`
  — *every one* of ~1,600 observed — which falls through unretried and costs a whole sync pass
  instead of a backoff. Fixing it means reading a body in the shared layer, or having the
  adapter classify before the retry decision rather than after; both are real designs and
  neither is in #216's scope.
- **A `ThrottleEvent` does not say which ceiling was hit**, or even that one was. Graph names
  it in the body (`MailboxConcurrency`), Gmail in a `403` `reason`; neither reaches the host,
  and the `403` never reaches the observer at all. So a diagnostic log cannot distinguish "too
  many at once" from "too many this minute" — which is why #216 needed a live probe to answer
  a question 31 recorded events could not.
- **Rate is unbounded everywhere.** The gate bounds width, and on three of the five adapters
  width is not what the server counts. Nothing in the engine paces requests per second.
- A transport failure (connection reset, timeout) is not retried here. Whether the server
  acted is unknowable from this layer, and the sync pass above already repeats a failed pass.
- `Method::is_idempotent` answers `false` for the WebDAV extension methods, so a `503` on
  `PROPFIND`/`REPORT` is not retried even though their own RFCs say a replay is safe.
  Conservative in the safe direction.
- Google's `403`-with-a-rate-limit-reason is classified as `RateLimited` by the adapter but is
  not retried by `send_retrying`, which reads status alone. Sniffing a body to decide would
  put provider knowledge in the shared layer; the adapter's classification is what carries it.
