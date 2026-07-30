# Stalwart protocol test harness

A reproducible, deterministic [Stalwart](https://stalw.art) server in Docker,
seeded with **one shared dataset that every protocol sees** (JMAP, IMAP, SMTP,
CalDAV, CardDAV). It is the fixture the engine's provider clients (build-order steps 4–5)
and the connectivity smoke suite target. See
[`docs/agent-guidance/stalwart-harness.md`](../../docs/agent-guidance/stalwart-harness.md)
for the design, the bootstrap flow, the per-fixture invariants, the gating
contract, and the determinism rules.

This is **test infrastructure**, not a product. It runs on loopback with
throwaway credentials and never holds real data.

## Run it

One self-bootstrapping service. It completes Stalwart v0.16's first-run setup
through the management API, creates the accounts, and seeds the dataset inside
its entrypoint, then reports healthy once seeding is done:

```sh
cd docker/stalwart
docker compose up -d --wait   # self-bootstrap + seed; healthy == ready
```

Then point the Rust smoke suite at it (from the repo root):

```sh
export STALWART_HTTP_ADDR=127.0.0.1:18080
export STALWART_IMAP_ADDR=127.0.0.1:11993          # IMAP implicit-TLS
export STALWART_SMTP_ADDR=127.0.0.1:11025          # SMTP plaintext
export STALWART_SMTP_TLS_ADDR=127.0.0.1:11465      # SMTP submission implicit-TLS
export STALWART_IMAP_STARTTLS_ADDR=127.0.0.1:11143 # IMAP STARTTLS
export STALWART_SMTP_STARTTLS_ADDR=127.0.0.1:11587 # SMTP submission STARTTLS
export STALWART_ACCOUNT=alice@test.local
export STALWART_PASSWORD=harness-alice-pw
cargo test -p stalwart-harness --test smoke -- --nocapture
```

Reset to a clean slate (the harness re-bootstraps from empty volumes; CI always
starts fresh):

```sh
docker compose down -v && docker compose up -d --wait
```

Without `STALWART_HTTP_ADDR` set, every Stalwart-touching test **skips**, so
`cargo test --workspace` stays green with no Docker.

## Host ports (loopback only)

| Protocol                      | Container | Host    | Transport          |
| ----------------------------- | --------- | ------- | ------------------ |
| HTTP — JMAP + CalDAV/CardDAV + admin  | 8080      | `18080` | plaintext          |
| SMTP                          | 25        | `11025` | plaintext          |
| SMTP submission               | 465       | `11465` | implicit TLS (self-signed) |
| IMAP                          | 993       | `11993` | implicit TLS (self-signed) |
| IMAP                          | 143       | `11143` | STARTTLS (self-signed)     |
| SMTP submission               | 587       | `11587` | STARTTLS (self-signed)     |

Stalwart supports STARTTLS on 143/587 but [recommends](https://stalw.art/docs/install/security/)
implicit TLS (993/465), so a fresh bootstrap does not create these listeners; the
entrypoint provisions them via the management API to exercise the engine's STARTTLS paths.

## Seeded accounts

Created at startup via Stalwart's management API (v0.16 has no declarative config
file — see the design doc).

| Account             | Password           | Role                          |
| ------------------- | ------------------ | ----------------------------- |
| `alice@test.local`  | `harness-alice-pw` | primary; holds the whole seed (mail + calendar + contacts) |
| `bob@test.local`    | `harness-bob-pw`   | scratch: SMTP recipient, and **organizer** in the scheduling suite |
| `carol@test.local`  | `harness-carol-pw` | scratch: **attendee** in the scheduling suite |
| `admin`             | `harness-admin-pw` | fallback admin (management)   |

Bob and Carol hold none of the seed. RFC 6638 auto-scheduling makes the server mail
both parties of an invitation, so the scheduling tests run entirely between these
two — mail delivered to Alice would break the exact INBOX count the mail suites
assert (`docs/agent-guidance/stalwart-harness.md`).

The entrypoint also **raises Stalwart's inbound rate limiters** (`x:MtaInboundThrottle`)
at bootstrap, logging `relaxed inbound rate limiters`. The default of 25 messages/hour
per sender-domain→recipient otherwise breaks the scheduling suite after a few runs, and
does so silently — see the harness guidance doc.

## Layout

```text
docker/stalwart/
├── docker-compose.yml      # single service (image pinned by digest)
├── entrypoint.sh           # self-bootstrap via API → restart → accounts → seed
├── seed.sh                 # curl: IMAP + CalDAV/CardDAV writes
└── seed/
    ├── mail/*.eml          # messages: dup/missing Message-ID, attachment, …
    ├── calendar/*.ics      # events: recurring+exceptions, attendees, …
    └── contacts/*.vcf      # international person + group
```
