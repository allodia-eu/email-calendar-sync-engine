# SabreDAV protocol test harness

A small, deterministic [SabreDAV](https://sabre.io/dav/) CalDAV server in Docker —
the **second** CalDAV protocol fixture beside the Stalwart one
([`docker/stalwart/`](../stalwart/)). SabreDAV is the stack many real providers
(Soverin, Fastmail-style hosts) run, and it diverges from Stalwart in exactly the
ways that exercise the client: the **two-step RFC 6764 discovery** (the
`calendar-home-set` lives on the principal, not the root) and the
`http://sabre.io/ns/sync/N` **sync-token** form. It reuses the **same** calendar
seed as Stalwart, so one dataset validates both servers and the same assertions
hold in [`provider-caldav`'s gated tests](../../crates/provider-caldav/tests/).

It is **test infrastructure, not product code**: PHP + Composer + `sabre/dav` over
a SQLite backend, served by the PHP built-in web server, seeded over CalDAV.

## What it is

- `Dockerfile` — `php:8.3` (pinned by digest) + `sabre/dav` installed from the
  committed `composer.lock`, plus the vendored SQL schema (`sql/`, since SabreDAV's
  composer dist omits `examples/`).
- `server.php` — a minimal CalDAV server: **HTTP Basic** auth against one throwaway
  account (the engine client uses Basic, not the stock PDO backend's Digest), the
  PDO principal/calendar backends, and a `/.well-known/caldav` redirect.
- `entrypoint.sh` — initializes the SQLite DB from the schema, seeds one principal,
  starts the server, seeds the calendar via CalDAV, writes a readiness marker, then
  runs the server in the foreground.
- `seed.sh` — `MKCALENDAR` + `PUT` the shared `../stalwart/seed/calendar/*.ics`.
- `docker-compose.yml` — the single service, health-gated on the post-seed marker.

## Running it

```sh
cd docker/sabredav
docker compose up -d --wait          # builds, self-seeds, healthy when ready

export SABREDAV_HTTP_ADDR=127.0.0.1:18081
cargo test -p provider-caldav --test live_sabredav -- --nocapture

# explore it read-only with the example:
CALDAV_URL=http://127.0.0.1:18081 \
  CALDAV_USER=alice@test.local CALDAV_PASS=sabredav-alice-pw \
  cargo run -p provider-caldav --example caldav_explore

docker compose down -v               # stop + wipe
```

Without `SABREDAV_HTTP_ADDR` set, the gated test **skips**, so the offline
`cargo test --workspace` stays green.

## Host port & account

| Item        | Value                                   |
| ----------- | --------------------------------------- |
| HTTP (CalDAV) | `127.0.0.1:18081` (Stalwart uses 18080) |
| Account     | `alice@test.local`                      |
| Password    | `sabredav-alice-pw` (throwaway)         |
| Calendar    | `/calendars/alice@test.local/default/`  |

The credentials are throwaway and committed on purpose — this server never holds
real data. Do not wire it to a host trust store or real accounts.

## Determinism

The base image is pinned by multi-arch digest and the PHP dependencies by
`composer.lock`; the seed inputs (the shared `.ics`) use fixed 2026 dates. CI always
starts from a clean build. To bump SabreDAV: regenerate `composer.lock`
(`composer update sabre/dav`) and re-resolve the base image digest.
