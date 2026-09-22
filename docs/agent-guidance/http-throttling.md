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
| a status the **adapter** recognises | as if it were a `429` | Two servers signal a throttle with something else, and each already knows it. See "When the status is not the answer". |
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

| Adapter | Signals a throttle as | Concurrency ceiling | Evidence | Narrows the gate | Classifies a body |
|---|---|---|---|---|---|
| `provider-graph` | `429` (+ `Retry-After`) | **4 per mailbox** | Documented `MailboxConcurrency`, and measured: a sharp cliff, 4 clean over 460 requests and 5 refused 13% | **yes**, in `HttpTransport::new` | no — the status is the whole answer |
| `provider-jmap` | `429`, method `rateLimit`/`overQuota`, **and a `400`** (below) | the session's `maxConcurrentRequests` | **The server states it** (RFC 8620 §2) — 4 on Stalwart, 10 on Fastmail | **yes**, in `JmapClient::connect`, once the session resolves | **yes**, `400` → `JmapThrottles` |
| `provider-google` | **both**: `429` for concurrency, `403` for the per-minute quota | **between 48 and 64 per user**, measured | See "Gmail has two limits, and they are not the same limit" | yes, at `MAX_CONCURRENT_GETS` (20), comfortably under it | **yes**, `403` → `GoogleThrottles` |
| `provider-caldav` + CardDAV | `429` (Stalwart); SabreDAV refuses nothing | **none found** — clean at widths 1–64 on both servers | Measured on two implementations; `provider-caldav/tests/live_concurrency.rs` | **no** | no — the status is the whole answer |
| `provider-imap` | n/a | 1 **per connection** | Not HTTP, and one connection is one command at a time | n/a — not an HTTP adapter | n/a |

⚠️ **Graph and Gmail both have a concurrency ceiling; Graph's is the one a gate is sized
against.** Graph's is 4 and an ordinary pass walks straight into it. Gmail's is between 48 and
64 and nothing this engine does goes near it — the adapter fans out 20 wide — so the gate is
narrowed there for a different reason, stated below. JMAP's is honoured because the server
states a number and ignoring a stated limit is indefensible whether or not it bites. On the
DAV servers the thing that refuses is a **rate**, and a width bound does not control a rate:
halving the width of a drain that then runs twice as long delivers the same requests per
second. Keep the two quantities apart — conflating them is how the Gmail figures below came to
be wrong twice, in opposite directions.

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

### Gmail has two limits, and they are not the same limit

This section has now been wrong twice, in opposite directions, and both times for the same
reason: one probe, one conclusion. What follows is what three probes say.

**It used to record a width that was really a rate.** "20 per user — 20 clean, 30 occasional,
50 throttles a tenth" was a width conclusion drawn from a width experiment run flat out. Run
that way a *fixed* width of 5 goes 0% → 21% → 82% refused across three consecutive blocks:
nothing about the width changed, the quota emptied. Re-run rested — 75-second gaps, widths in
randomised order, 100 requests a block — widths 5, 20 and 50 all ran 200 requests with **zero**
refusals and throughput scaling linearly.

**Then it recorded "Gmail never sends a `429`", which is true only below 50.** The rested sweep
stopped at width 50, and Gmail's concurrency ceiling is just above it. Re-run as salvos — W
simultaneous `messages.get`s, five times, a full quota window's rest between widths, widths in
randomised order so a monotone result cannot be drift:

| width | 8 | 16 | 32 | 48 | 64 | 96 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|
| refused `429` | 0% | 0% | 0% | **0%** | **1.6%** | 2.5% | 8.4% | 10.4% |

Clean through 48, refusing from 64, and the share climbs with width — the signature of a
ceiling on requests in flight, and the same shape Graph's cliff has at 4. The refusal is a
`429`:

```
429 RESOURCE_EXHAUSTED  rateLimitExceeded
"Too many concurrent requests for user."
```

**And the quota limit is real, is separate, and is the one an ordinary pass meets.** Hold the
width at 8 — far below the ceiling above — and sustain it, and the refusal is a `403`:

```
403 PERMISSION_DENIED  rateLimitExceeded
"Quota exceeded for quota metric 'Total Query Cost' and limit 'Units per minute per user'"
  quota_limit:        totalQueryCostPerMinutePerUser
  quota_limit_value:  6000
  quota_unit:         1/min/{project}/{user}
  window_start_time:  1789912553
```

**6,000 cost units per minute, per user, per project** — a budget spent over a window, which is
why the same width is clean on a rested quota and refused on a drained one, and why every "how
wide can we go" measurement here has to start from rest.

So the two limits answer two different probes, and each one hides the other:

| Probe | Meets | Because |
|---|---|---|
| wide salvos, rested between | `429` concurrency | too many at once, quota barely touched |
| narrow and sustained | `403` quota | quota drained, never more than 8 at once |

Both are captured verbatim — `provider-google/tests/fixtures/error/concurrency_exceeded.json`
and `quota_exceeded.json` — because a count of refusals cannot tell the next reader which
limit was counted.

Consequences:

- **The gate is not sized against Gmail's ceiling, and should not be.** 20 is the adapter's own
  fan-out width; the server's ceiling is somewhere past 48. Narrowing to 20 is still worth
  doing, for the gate's own reason: an account's mail, calendar and contacts providers would
  otherwise each run their own 20-wide fan-out against one user's quota, and three times the
  width drains it three times as fast.
- **The `403` is the one that matters in practice**, because the adapter never fans out wide
  enough to meet the other. It is what "When the status is not the answer" exists for.
- **Neither refusal carries a `Retry-After`** — checked against every header of both captures.
  The `403` states the window's start instead, so the wait to the window's end is a number the
  *server* named; `provider-google/src/throttle.rs` reads it, defensively, and every way it can
  fail lands back on the backoff schedule.

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

## When the status is not the answer

Two of the four adapters signal a throttle with a status that reads as something else, and
**each of them already knows it** — the verdict simply arrived after the retry decision, which
had read the status and moved on.

| Adapter | Arrives as | Reads as | Really is |
|---|---|---|---|
| `provider-google` | `403 PERMISSION_DENIED` | no permission | the per-minute quota, when `errors[0].reason` is a rate-limit reason |
| `provider-jmap` | `400` + `urn:ietf:params:jmap:error:limit` | a bad request | too many at once, when `limit` is `maxConcurrentRequests` |

**The adapter reads the body; `engine-http` still owns every question about waiting.** A
`RetryConfig` carries a `ThrottleClassifier` the adapter supplies through `classifying`, the
same way it supplies its label through `labelled` — how a server says no is the one thing only
the adapter knows. Putting a `serde_json` call in `send_retrying` instead would be shorter by a
day and would end with four providers' error vocabularies in one `match` in the neutral crate.

Three properties hold the design together:

- **It is additive.** The status rule runs first and alone, so a `429` or an idempotent `503`
  is waited out on exactly the path it always was. A classifier can turn a reply that was being
  handed back into one that is waited out, and can never do the reverse.
- **Bodies are read only where the adapter asked.** `reads_body_of(status)` is consulted before
  anything is buffered, and claims `403` for Google and `400` for JMAP — nothing else. A
  classifier that claimed `200` would buffer every page the adapter ever fetched.
- **Nothing is reconstructed.** Reading a body used to mean losing the reply —
  `Response::bytes` consumes `self` — so the first version of this took the reply apart and
  built a new one around the bytes, copying the status, version, headers and extensions across
  by hand. That is a list of fields somebody has to keep in step with reqwest, and one of them
  was already nearly missed: `TlsInfo` lives in the extensions, and dropping it would have made
  a throttled account the one account reporting no TLS version, with nothing failing to say so.
  The URL could not be carried at all. So the body is drained through `Response::chunk`, which
  takes `&mut self`, and **the reply handed back is the object the client produced**. `Sent`
  wraps it: it derefs to the response for everything taken by reference, and its own `text`
  and `bytes` hand back what was already read. A caller cannot reach past it to an empty
  body — `Response`'s consuming readers are unreachable through a `Deref`, so it does not
  compile.

A classifier may also name a wait. Google's does: the `403` carries the quota window's start,
so the time to the window's end is the server's own number and is honoured exactly as a
`Retry-After` would be — never undercut, reported to the host as `server_asked`, and still
subject to the total-wait budget. JMAP's does not, because RFC 8620's problem details say which
limit was hit and nothing about when it clears.

**Classifying is not the same as waiting.** Google names four rate-limit reasons and the
adapter calls all four `RateLimited`, rightly — a host backs off for any of them. Only
`rateLimitExceeded` and `userRateLimitExceeded` describe a limit the funnel can outlast;
`dailyLimitExceeded` and `quotaExceeded` name a quota measured in days, so the classifier
declines them and the reply goes straight back after one send, exactly as it did before any
classifier existed. Retrying those five times would spend five requests to be told the same
thing, for a window that clears tomorrow.

### JMAP's `400`, and where it is actually enforced

RFC 8620 §3.6.1 returns *every* request-level error with a `400`,
`urn:ietf:params:jmap:error:limit` among them, and its `limit` property names which limit was
hit. Only `maxConcurrentRequests` is one a client clears by waiting — `maxSizeRequest` and
`maxCallsInRequest` describe the request that was sent, and both are pinned as fixtures from
the real server precisely so the reader can be shown discriminating between them.

RFC 8620 scopes `maxConcurrentRequests` to the API endpoint and defines no companion for
downloads, but a server may apply one number to more than that. **Stalwart applies it to
uploads and, as measured here, to nothing else:** 300 simultaneous API calls and 300
simultaneous blob *downloads* were all answered `200`, while 16 simultaneous uploads drew six
`400`s — reported under the name `maxConcurrentRequests` even though the session advertises a
separate `maxConcurrentUpload`. One counter, two names. It still defaults to `1` when a session
omits it, rather than to a guess.

That is also why the refusal is only reachable at all with **more than one client**:
`JmapClient::connect` narrows the account's gate to the stated number, so this adapter alone
cannot exceed it. A gate bounds one account *in this process*; the server counts the user's
other devices too. `provider-jmap/tests/live_concurrency_limit.rs` is four clients of one
account, which is what that sentence looks like in practice.

## Known gaps

- **Rate is unbounded everywhere.** The gate bounds *width*, and on three of the five adapters
  width is not the quantity the server counts — Gmail bills units per minute, Stalwart's DAV
  endpoint trips somewhere between 50/s and 150/s, and nothing in the engine paces requests per
  second. What exists now is the **reactive** half: a rate limit, once hit, is waited out
  properly. The proactive half — a token bucket per account, which is a different instrument
  from `RequestGate` and would need a rate to be stated or learned — is deliberately not built.
  Two reasons, and the second is the real one: no server in this set states a rate the way JMAP
  states a width, so any bucket would be a guess; and a guess low enough to be safe on a
  drained quota is a guess that halves throughput on a rested one. Revisit it if a host reports
  quota refusals that the backoff does not absorb.
- **A `ThrottleEvent` still does not say which ceiling was hit.** It now carries the status,
  which for Gmail distinguishes the two (`429` is concurrency, `403` is the quota) — but that
  is a coincidence of Gmail's, not a property of the type. Graph names its ceiling in the body
  (`MailboxConcurrency`) and JMAP in its `limit` property, and neither reaches the host. A
  diagnostic log can now at least see that *something* was waited out, which before this it
  could not: the `403` never reached the observer at all.
- **A classifier reads a body it may not get.** `reads_body_of` is asked about a status, not a
  content type, so a `403` served as HTML by a captive portal is buffered and parsed before
  being declined. That is cheap and correct, but it is a body read for nothing; if a provider
  ever claims a status whose replies are large, narrow it there rather than here.
- **Google's `429` has no live test.** The concurrency ceiling is measured and captured, but
  reaching it costs a salvo of 64+ requests, and the adapter fans out 20 wide — so a live test
  would be provoking a limit the shipping code cannot reach, at real quota cost, to prove the
  status rule that has worked since the crate landed. The `403` is the one with a live test
  (`provider-google/tests/live_quota_throttle.rs`), because it is the one an ordinary pass
  meets.
- A transport failure (connection reset, timeout) is not retried here. Whether the server
  acted is unknowable from this layer, and the sync pass above already repeats a failed pass.
- `Method::is_idempotent` answers `false` for the WebDAV extension methods, so a `503` on
  `PROPFIND`/`REPORT` is not retried even though their own RFCs say a replay is safe.
  Conservative in the safe direction.
- **JMAP's method-level `rateLimit`/`overQuota` are out of reach of all this.** They arrive
  inside a `200 OK`, one per method call in a batch, so the HTTP funnel never sees a refusal to
  wait out. The method layer classifies them `RateLimited` and the pass repeats — the same
  whole-pass delay the `400` used to cost. Fixing it means retrying *a call within a batch*,
  which is a different mechanism from this one.
