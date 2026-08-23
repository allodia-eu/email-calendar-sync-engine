# jmap-client

A **local dev tool** that drives a real JMAP server: capture responses as offline fixtures
for the `provider-jmap` adapter, and measure what a server actually does.

The sibling of [`graph-oauth`](../graph-oauth) and [`google-oauth`](../google-oauth), and
simpler than either — JMAP specifies no authentication mechanism of its own (RFC 8620 §8.2),
so there is no OAuth dance. A bearer token or basic credentials are handed over directly.

It is intentionally **not** part of the engine workspace (its own `[workspace]` table), so it
never affects the engine's fmt/clippy/coverage gates. The engine itself stays
credential-agnostic; hosts own onboarding (`docs/agent-guidance/north-star.md`).

## Credentials

Flags win, then the environment, then `.local/account.json` (gitignored):

```sh
# Once, so later runs need no flags:
cargo run --manifest-path tools/jmap-client/Cargo.toml -- \
  save --url https://api.fastmail.com --token "$FASTMAIL_API_TOKEN"

# Or per-run:
JMAP_URL=http://127.0.0.1:18080 JMAP_USER=alice@test.local JMAP_PASSWORD=harness-alice-pw \
  cargo run --manifest-path tools/jmap-client/Cargo.toml -- session
```

A JMAP API token is a password. Do not commit `.local/`.

## Session URLs

By default the URLs the session advertises are **rebased onto the origin actually dialled** —
the engine's `SessionUrlPolicy::RebaseToConnection`, and what a proxied server needs (the
Stalwart harness advertises `https://mail.test.local/jmap/`, which nothing can reach). Pass
`--trust-advertised` to take the document literally, which is correct for a provider that
genuinely serves its API from another origin.

## Usage

Run from the repo root:

```sh
M=tools/jmap-client/Cargo.toml

# The session: capabilities, limits, URL templates. Prints maxConcurrentRequests first.
cargo run --manifest-path $M -- session
cargo run --manifest-path $M -- session crates/provider-jmap/tests/fixtures/session.json

# One method call, args inline / from a file / from stdin.
cargo run --manifest-path $M -- call Mailbox/get '{"accountId":"c","ids":null}'
cargo run --manifest-path $M -- call Email/query @query.json emails.json

# A raw authenticated GET — a downloadUrl blob, or anything else advertised.
cargo run --manifest-path $M -- get "https://…/download/c/blob-1/message" body.eml

# How much overlapping body downloads buys, and where the server stops.
cargo run --manifest-path $M -- bench --messages 100 --widths 1,2,4,8,16,1
```

`bench` exists because a JMAP page of *metadata* is one `Email/get` for the whole page — the
protocol batches it — while a **body** is one blob `GET` per message. That is the only part of
a sync with a round trip per message to overlap, and it is what
`ConnectionInfo::concurrent_fetches` paces. The last width repeats the first on purpose: if
the two disagree, the sweep measured a warming cache rather than concurrency.

⚠️ Against a local harness the round trip is sub-millisecond, so the *rates* there mean
nothing — only the failure column does (it is how the `400`
`urn:ietf:params:jmap:error:limit` above `maxConcurrentRequests` was found). Rates need a real
server over a real link.
