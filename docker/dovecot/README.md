# Dovecot protocol test harness

**Two** deterministic [Dovecot](https://www.dovecot.org/) IMAP servers in Docker, from one
image and one seed, differing only in the IMAP revision they speak. They join the Stalwart
fixture ([`docker/stalwart/`](../stalwart/)) to make three live IMAP servers, of which two
are rev2. Both reuse the **same** mail seed as Stalwart
([`../stalwart/seed/mail`](../stalwart/seed/mail)), so one dataset validates every server.

Dovecot is what most IMAP accounts in the world actually terminate on, and its rev1 mode is
a different protocol *dialect* from Stalwart rather than merely a different implementation:

| | Stalwart | `dovecot-rev1` | `dovecot-rev2` |
| --- | --- | --- | --- |
| Dialect the client negotiates | `IMAP4rev2` | `IMAP4rev1` | `IMAP4rev2` (experimental) |
| SPECIAL-USE on an extended `LIST` | volunteered whether asked or not | **only** when the return option asks | **only** when the return option asks |
| Non-ASCII mailbox names | UTF-8 | **modified UTF-7** (`&ANw-berweisungen`) | UTF-8 |
| Mailbox names in `LIST` rows | always quoted | unquoted where quoting is unnecessary | quoted only where needed |
| `* ENABLED` casing | `IMAP4rev2` | — | `IMAP4REV2` |
| Tagged completion | `LIST completed` | `List completed (0.028 + 0.000 + 0.027 secs).` | same prose form |

Things this fixture proves that **Stalwart cannot** (see "Which server proves what" in
[`imap-smtp.md`](../../docs/agent-guidance/imap-smtp.md)):

- **That an extension actually gets negotiated.** rev2 folds SPECIAL-USE into the base
  protocol and Stalwart volunteers the attributes regardless, so a client that forgets to
  ask is green there forever. On **both** halves here, forgetting costs every folder its
  role — and with it the Sent folder a filed copy is placed in
  ([`place.rs`](../../crates/provider-imap/src/place.rs)).
- **That one server's reading of a spec is not the spec.** `dovecot-rev2` exists for this.
  RFC 9051 makes the role attributes base `LIST` data and defines no `RETURN (SPECIAL-USE)`
  option, which reads like a rev2 client need never ask; Dovecot's rev2 advertises RFC 6154
  too and keeps RFC 6154's rule, stripping every role from an extended `LIST` that did not
  ask. With one rev2 server the client had shipped the reading rather than the protocol.
- **Modified UTF-7, in both directions.** Once the client enables rev2 on a server, that
  server stops emitting modified UTF-7 altogether, so
  [`utf7`](../../crates/provider-imap/src/utf7.rs) — decoding a `LIST` name *and* encoding
  one back for a `SELECT` — has live coverage on `dovecot-rev1` and nowhere else. That is
  why the rev1 half pins `imap4rev2_enable = no` instead of moving with the image default.
- **That only untagged lines are data.** Dovecot's completion detail is prose whose first
  word is the command name and whose last character is a period, so a parser that reads
  the completion line as data invents a mailbox named `.` from it.

It is **test infrastructure, not product code**.

## What it is

- `docker-compose.yml` — two services, `dovecot-rev1` and `dovecot-rev2`, sharing one
  digest-pinned `dovecot/dovecot` image, one entrypoint and one seed via a YAML anchor. Host
  ports on loopback only.
- `harness.conf` — the drop-in **both** services share
  ([the documented mechanism](https://doc.dovecot.org/main/installation/docker.html)), so
  the vendor rootless setup stays intact. It declares the special-use mailbox set explicitly
  rather than relying on the image's defaults.
- `rev1.conf` / `rev2.conf` — one file per service, three settings each, and the *entire*
  difference between the two servers. Anything that diverged for another reason would make
  them incomparable.
- `entrypoint.sh` — starts the server, seeds the shared `.eml` fixtures through `doveadm`,
  writes a readiness marker, then holds the server in the foreground. Both services run it.

One server cannot cover both dialects: the client `ENABLE`s IMAP4rev2 wherever a server
offers it ([`capability.rs`](../../crates/provider-imap/src/capability.rs)), so the dialect a
session settles on is the server's to decide. Hence two services rather than a client knob.

## Running it

```sh
cd docker/dovecot
docker compose up -d --wait          # both self-seed; healthy when ready

DOVECOT_REV1_IMAP_ADDR=127.0.0.1:11994 \
DOVECOT_REV2_IMAP_ADDR=127.0.0.1:11995 \
  cargo test -p provider-imap --test live_imap_contract -- --nocapture
# then the dialect suites: --test live_imap_rev1 / --test live_imap_rev2

docker compose down -v               # stop + wipe
```

Each suite skips the servers whose address variable is unset, so the offline
`cargo test --workspace` stays green.

## Host ports & account

| Item | Value |
| --- | --- |
| IMAP rev1 (implicit TLS) | `127.0.0.1:11994` (Stalwart uses 11993) |
| IMAP rev1 (STARTTLS) | `127.0.0.1:11144` (Stalwart uses 11143) |
| IMAP rev2 (implicit TLS) | `127.0.0.1:11995` |
| Account (both) | `alice@test.local` |
| Password (both) | `dovecot-alice-pw` (throwaway) |

STARTTLS is exposed on the rev1 service only: it is a transport concern with nothing
dialect-specific about it, so a second listener would be surface no test uses.

The image's stock passdb is `static`, so **any** username authenticates with that one
password; the account name above is simply the one the seed and tests use. The credentials
are throwaway and committed on purpose — these servers never hold real data. The TLS
certificate is the image's own self-signed one; tests trust-skip explicitly and never
touch a host trust store. Do not wire this to real accounts.

## The seeded mailbox

On each server, `INBOX` holds the nine shared `.eml` fixtures (all unread) and `Sent` holds
one. The special-use set — `Drafts`, `Sent`, `Trash`, `Junk`, `Archive` — is declared in
`harness.conf` and created at first login, plus `Überweisungen` — an ordinary folder whose
only job is to have a name that must survive the encoding. The Stalwart harness seeds the
same folder under the same display name, so a test can assert that one mailbox reaches the
model with one identity from servers that put entirely different bytes on the wire.

## Determinism

The image is pinned by multi-arch digest, the configs are committed, and the seed inputs are
the shared fixtures. Assertions are on harness-controlled content (roles, names, counts),
never on server-assigned UIDs. CI always starts from a clean volume. To bump Dovecot,
re-resolve the digest and re-run both gated suites — and re-read all three conf files. The
image has changed underneath this fixture before: its default mailbox set has moved, and
2.4.4's own `dovecot.conf` turns `imap4rev2_enable` **on**, which is why `rev1.conf` states
the value it needs rather than trusting a default. rev2 is experimental upstream
(`--enable-experimental-imap4rev2`, compiled in as of 2.4.4; the setting first appears in
2.4.2), so expect it to move — and note Dovecot refuses to start on
`imap4rev2_enable = yes` without `mail_utf8_extensions = yes`.
