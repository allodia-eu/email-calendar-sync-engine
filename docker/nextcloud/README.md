# Nextcloud DAV test harness

A deterministic [Nextcloud](https://nextcloud.com/) server in Docker, serving CalDAV and
CardDAV: the third DAV fixture beside Stalwart ([`docker/stalwart/`](../stalwart/)) and
SabreDAV ([`docker/sabredav/`](../sabredav/)). Nextcloud is SabreDAV underneath, configured
the way a real deployment is, so it exercises what the bare SabreDAV fixture does not:

- **A real principal layout.** The well-known paths redirect to `/remote.php/dav/`, which
  challenges with `Basic`, and the principal is `/remote.php/dav/principals/users/alice/`.
- **A home with several collections.** The calendar home holds `personal`, a generated
  birthday calendar, a scheduling inbox and outbox, and the seeded `harness` calendar, so a
  client has to bind the one it was asked for.
- **Read-only address books.** Beside the writable `contacts` and `harness` books, the home
  lists the instance's users (`z-server-generated--system`) and an app's recently contacted
  (`z-app-generated--contactsinteraction--recent`), neither of which the account may add to.
- **Scheduling.** Its `OPTIONS` reports `calendar-auto-schedule` (RFC 6638), the positive
  answer to the SabreDAV fixture's negative one on the same server software.
- **Verbatim storage.** Like SabreDAV, it hands back the iCalendar bytes it was given.

It is **test infrastructure, not product code**: the stock `nextcloud` image on the stock
`postgres` image, both with their data on a `tmpfs`, so every start is a fresh install. Not
SQLite: Nextcloud writes on every authenticated request, a read included, so two requests at
once fail one of them with "database is locked", which a slower CI runner meets.

## What it is

- `docker-compose.yml`: Nextcloud and its PostgreSQL database, each pinned by digest.
  Nextcloud is published on loopback port 18082 and health-gated on the seed's marker and on
  `status.php` reporting an installed server; the database is not published.
- `hooks/seed.sh`: the image's post-installation hook. It gives the account the address
  `alice@test.local`, creates the `harness` calendar and imports the shared
  [`../stalwart/seed/calendar/*.ics`](../stalwart/seed/calendar/) into it, and creates the
  `harness` address book.

## Running it

```sh
cd docker/nextcloud
docker compose up -d --wait          # installs, seeds, healthy when ready
NEXTCLOUD_HTTP_ADDR=127.0.0.1:18082 \
  cargo test -p provider-caldav --test live_nextcloud --test live_nextcloud_contacts
docker compose down -v
```

The first start takes about a minute while Nextcloud installs. `dav-cli` reaches it as the
built-in `nextcloud` profile, bound to the `harness` calendar.

## Account

| | |
|---|---|
| User | `alice` (also the instance's administrator) |
| Password | `nextcloud-alice-pw` |
| Address | `alice@test.local` |
| Base URL | `http://127.0.0.1:18082` |

Throwaway credentials, committed on purpose. The suites read `NEXTCLOUD_USER` and
`NEXTCLOUD_PASS` when set.

## Bumping the image

Re-resolve the multi-arch digest for the new tag (`docker buildx imagetools inspect
nextcloud:<tag>-apache`), replace it in `docker-compose.yml`, and run both gated suites.
