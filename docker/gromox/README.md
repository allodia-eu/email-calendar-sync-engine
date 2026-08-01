# Gromox harness (MAPI/HTTP spike)

A deliberately lean Gromox: **MariaDB + `gromox-http` only**, not the official grommunio stack
(8 services / 18 supervisord daemons, plus chat, keycloak, office and archive). The spike needs one
protocol endpoint — `mh_emsmdb` (MAPI/HTTP) and `oxdisco` (Autodiscover), both of which are in
gromox's fixed HPM chain and need no enabling.

Not wired into CI. It exists to answer protocol questions for `tools/mapi-spike`; see that
directory's `README.md` and `HANDOFF.md`.

```sh
docker compose up -d --wait      # readiness is a marker file, not a sleep
```

- `http://127.0.0.1:18082` — loopback only (Stalwart holds 18080, SabreDAV 18081)
- `alice@spike.test` / `alicepass` — throwaway credentials, committed on purpose
- Plaintext HTTP with Basic auth: TLS is not what this harness measures

`grommunio` publishes **aarch64** RPMs, so the image builds natively on Apple Silicon as well as
x86_64 — no emulation.

Gromox is **AGPL-3.0**. Running it as a black-box fixture creates no derived work (this repo already
runs Stalwart, also AGPL), but its source is **never** a source of code for this repository. The
client-side reference to port from is the MIT-licensed `OfficeDev/Interop-TestSuites`.

## Known limitation

Out-of-process seeding (`gromox-eml2mt | gromox-mt2exm`) does not work: gromox binds its exmdb IPC
to `[::1]:5000` regardless of `exmdb_listen`, while its own client resolves `localhost` to
`127.0.0.1`. `/etc/hosts` is a bind-mount so it cannot be reordered in-container. MAPI/HTTP itself
is unaffected — `gromox-http` reaches the store in-process — so folder enumeration works; only
mailbox *contents* are empty.
