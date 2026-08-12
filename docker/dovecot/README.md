# Dovecot protocol test harness

A deterministic [Dovecot](https://www.dovecot.org/) IMAP server in Docker — the **second**
IMAP fixture beside the Stalwart one ([`docker/stalwart/`](../stalwart/)), and the
**IMAP4rev1** half of the pair. It reuses the **same** mail seed as Stalwart
([`../stalwart/seed/mail`](../stalwart/seed/mail)), so one dataset validates both servers.

Dovecot is what most IMAP accounts in the world actually terminate on, and it is a
different protocol *dialect* from Stalwart rather than merely a different implementation:

| | Stalwart | Dovecot (this fixture) |
| --- | --- | --- |
| Dialect the client negotiates | `IMAP4rev2` (advertised, enabled, confirmed) | `IMAP4rev1` |
| SPECIAL-USE on an extended `LIST` | volunteered whether asked or not | **only** when the return option asks |
| Non-ASCII mailbox names | UTF-8, once rev2 is enabled | **modified UTF-7** (`&ANw-berweisungen`) |
| Mailbox names in `LIST` rows | always quoted | unquoted atoms where quoting is unnecessary |
| Tagged completion | `LIST completed` | `List completed (0.028 + 0.000 + 0.027 secs).` |

Three things it proves that **Stalwart cannot** (see "Which server proves what" in
[`imap-smtp.md`](../../docs/agent-guidance/imap-smtp.md)):

- **That an extension actually gets negotiated.** rev2 folds SPECIAL-USE into the base
  protocol and Stalwart volunteers the attributes regardless, so a client that forgets to
  ask is green there forever. Here, forgetting costs every folder its role — and with it
  the Sent folder a filed copy is placed in ([`place.rs`](../../crates/provider-imap/src/place.rs)).
- **Modified UTF-7, in both directions.** Once the client enables rev2 on Stalwart, that
  server stops emitting modified UTF-7 altogether, so
  [`utf7`](../../crates/provider-imap/src/utf7.rs) — decoding a `LIST` name *and* encoding
  one back for a `SELECT` — has no live coverage anywhere else. This fixture has no rev2 to
  switch to, so it pins that path permanently.
- **That only untagged lines are data.** Dovecot's completion detail is prose whose first
  word is the command name and whose last character is a period, so a parser that reads
  the completion line as data invents a mailbox named `.` from it.

It is **test infrastructure, not product code**.

## What it is

- `docker-compose.yml` — the single service: the stock `dovecot/dovecot` image pinned by
  digest, with host ports on loopback only.
- `harness.conf` — a drop-in merged over the image's own config
  ([the documented mechanism](https://doc.dovecot.org/main/installation/docker.html)), so
  the vendor rootless setup stays intact. It turns the UTF-8 mailbox-name extensions
  **off** (making this a true rev1 server) and declares the special-use mailbox set
  explicitly rather than relying on the image's defaults.
- `entrypoint.sh` — starts the server, seeds the shared `.eml` fixtures through `doveadm`,
  writes a readiness marker, then holds the server in the foreground.

## Running it

```sh
cd docker/dovecot
docker compose up -d --wait          # self-seeds; healthy when ready

DOVECOT_IMAP_ADDR=127.0.0.1:11994 \
  cargo test -p provider-imap --test live_dovecot -- --nocapture

docker compose down -v               # stop + wipe
```

Without `DOVECOT_IMAP_ADDR` set the gated test **skips**, so the offline
`cargo test --workspace` stays green.

## Host ports & account

| Item | Value |
| --- | --- |
| IMAP (implicit TLS) | `127.0.0.1:11994` (Stalwart uses 11993) |
| IMAP (STARTTLS) | `127.0.0.1:11144` (Stalwart uses 11143) |
| Account | `alice@test.local` |
| Password | `dovecot-alice-pw` (throwaway) |

The image's stock passdb is `static`, so **any** username authenticates with that one
password; the account name above is simply the one the seed and tests use. The credentials
are throwaway and committed on purpose — this server never holds real data. The TLS
certificate is the image's own self-signed one; tests trust-skip explicitly and never
touch a host trust store. Do not wire this to real accounts.

## The seeded mailbox

`INBOX` holds the nine shared `.eml` fixtures (all unread) and `Sent` holds one. The
special-use set — `Drafts`, `Sent`, `Trash`, `Junk`, `Archive` — is declared in
`harness.conf` and created at first login, plus `Überweisungen` — an ordinary folder whose
only job is to have a name that must survive the encoding. The Stalwart harness seeds the
same folder under the same display name, so a test can assert that one mailbox reaches the
model with one identity from two servers that put entirely different bytes on the wire.

## Determinism

The image is pinned by multi-arch digest, the config is committed, and the seed inputs are
the shared fixtures. Assertions are on harness-controlled content (roles, names, counts),
never on server-assigned UIDs. CI always starts from a clean volume. To bump Dovecot,
re-resolve the digest and re-run the gated suite — and re-read `harness.conf`, since the
image's default mailbox set has changed before.
