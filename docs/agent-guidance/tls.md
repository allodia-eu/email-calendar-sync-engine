# TLS trust policy

Authoritative for how the engine trusts server certificates. Read before touching
`engine-tls`, a provider's transport construction, or a client's trust wiring.

## The one rule

**Every provider derives its trust from one host-selected [`TlsPolicy`], realized
into one ring-backed `rustls::ClientConfig` by `engine-tls`.** There is no per-
provider trust store. Before this, IMAP/SMTP verified against bundled `webpki-roots`
while the reqwest providers (CalDAV/JMAP/Graph) silently used reqwest 0.13's default
`rustls-platform-verifier` — two trust sources and two crypto backends
(`ring` + `aws-lc-rs`) in one app, and the platform verifier's Android OCSP behavior
broke CalDAV against Let's Encrypt certs. `engine-tls` removes that split. EAS
joined later: it was imported on a crate-local `reqwest` 0.12 + `native-tls`
transport with an `accept_invalid_certs` escape, and the same relocation series
switched it onto this policy (`eas.md` → "TLS decision record").

## The policy (`engine_tls::TlsPolicy`)

```
Roots { bundled: bool, system: bool, custom: Vec<CertificateDer> }  // rustls's own verifier
PlatformVerifier { extra_roots: Vec<CertificateDer> }               // OS verifier delegation
```

- **`Roots`** verifies with rustls's own webpki verifier over the **union** of the
  enabled sources — bundled Mozilla roots (`webpki-roots`), the OS store
  (`rustls-native-certs`), and/or explicit `custom` roots. No OS-verifier
  delegation, so **no Android `Context`** and **no platform revocation/OCSP
  behavior**.
- **`PlatformVerifier`** delegates the whole chain decision to the OS
  (`rustls-platform-verifier`): faithful to platform policy — including Android
  network-security-config / MDM CAs — but on Android the host **must** initialize
  the JVM `Context` first, and it carries the platform's revocation behavior.

Presets: `bundled()`, `bundled_and_system()`, `system_only()`, `pinned(roots)`,
`platform_verifier()`.

## Defaults: the Firefox model, split by layer

We mirror Firefox: bundled Mozilla roots as the base, **augmented** by the OS store
(Firefox's `security.enterprise_roots`), so an enterprise CA injected into the OS
store "just works" while public CAs always resolve.

- **Engine library default = `bundled()`** (pure Mozilla roots). Hermetic and
  reproducible — server/CLI hosts and tests never depend on the build machine's OS
  store. `TlsClientConfig::default()` is bundled, and each provider config defaults
  to it, so a host that selects nothing gets bundled trust.
- **Native clients ship `bundled_and_system()`** (bundled ∪ OS, the Firefox
  preset) so enterprise/MDM CAs are honored with zero configuration.

> Trusting the OS store means trusting whatever an admin/MDM — or a corporate MITM
> proxy / local AV interceptor — installed there. Firefox accepts this; a
> sovereignty-strict tenant can stay on pure `bundled()`, or `pinned(roots)` to
> trust *only* an explicit CA.

## How each provider consumes it

The host builds **one** `TlsClientConfig` (`engine_tls::client_config(&policy)?`) and
shares it (cloning is a cheap `Arc` bump):

- **CalDAV / JMAP** carry it in their config (`CalDavConfig::tls` /
  `JmapConfig::with_tls`), defaulting to bundled. `connect` reads `config.tls`.
- **Graph** (token-based, no config struct) takes it as a parameter:
  `GraphClient::connect(token, &tls)` / `for_mailbox` / `with_base`.
- **EAS** takes it as a parameter at `EasClient::new(config, &tls)`, which
  builds its `reqwest::Client` via `tls.reqwest_builder()`; the autodiscover
  flow's `http` client is the caller's and must come from the same builder
  (`eas.md` → "TLS decision record").
- **IMAP/SMTP** keep taking a `tokio_rustls::TlsConnector` (the host builds it via
  `tls.connector()`); the library bakes in no root store.

`TlsClientConfig::reqwest_builder()` returns a preconfigured `reqwest::ClientBuilder`
(each HTTP provider adds its own non-TLS settings, e.g. redirect policy). It
advertises ALPN `h2` then `http/1.1`, so the HTTP providers negotiate HTTP/2 where
the server supports it (JMAP and Microsoft Graph do) and fall back to HTTP/1.1 —
reqwest's preconfigured-TLS path keeps the config's ALPN rather than deriving its
own, so it is set here. The shared connector (IMAP/SMTP) carries no ALPN.

## Crypto backend and the reqwest wiring

- **One backend: `ring`.** Every config is built with an explicit
  `rustls::crypto::ring::default_provider()`; `aws-lc-rs` is out of the tree.
- **TLS 1.2 floor, uniform across providers.** The shared config uses
  `with_safe_default_protocol_versions()` (rustls's safe defaults, TLS 1.2 + 1.3;
  rustls implements nothing older), so every provider — the reqwest HTTP
  providers (CalDAV/JMAP/Graph/EAS) and the IMAP/SMTP connector — has the same
  1.2 minimum by construction. Do **not**
  reach for reqwest's `min_tls_version`: it has no effect on the preconfigured-TLS
  path, so the floor must live in the shared config (which is why it is uniform).
- reqwest uses the **`rustls-no-provider`** feature (not `rustls`), which gives the
  rustls integration without `aws-lc-rs` and without reqwest's own platform-verifier
  path. We always hand reqwest a preconfigured config via `tls_backend_preconfigured`.
  We also enable reqwest's **`http2`** feature so the preconfigured client can speak
  HTTP/2 when ALPN negotiates it (see "How each provider consumes it").
- **Footgun:** under `rustls-no-provider`, a reqwest client built *without*
  preconfigured TLS panics on its first HTTPS request. All clients must go through
  `TlsClientConfig::reqwest_builder()`. As insurance, `client_config` installs a
  `ring` process-default `CryptoProvider` once.

## Cargo features (`engine-tls`)

`default = []` (bundled only). Opt-ins: `reqwest` (the HTTP builder; the HTTP
providers enable it), `tls-native-certs` (the `system` root source),
`tls-platform-verifier` (the `PlatformVerifier` policy), `dangerous-testing`
(`TlsClientConfig::dangerous_accept_any`, for the self-signed harness cert only).
A `System`/`PlatformVerifier` policy returns `TlsError::Unsupported` when its
feature is off (the enum stays stable across builds for FFI).

## What the engine reports back (`ConnectionInfo`, `ConnectStep`)

Each adapter surfaces what its transport negotiated through the one post-connect
seam, `Provider::connection_info()` (`providers.md`). The trust *policy* is
deliberately **not** in it: the host selected it and already knows it, so the host
logs it. The object carries only what the **server** decided — and what it can carry
is asymmetric:

- **IMAP/SMTP** (`tokio-rustls`): `tls_version` comes from rustls'
  `ClientConnection::protocol_version()`, read off the finished handshake in
  `provider-imap`'s `tls_info` (the last place the concrete `TlsStream` is visible
  before `Connection<S>` erases it). No `http_version`. It describes the **IMAP**
  session; SMTP submission re-dials per send, so its handshake is not a durable fact
  of the provider.
The same asymmetry decides who can emit `ConnectStep::TlsEstablished` on the
connect-phase observer seam (`providers.md`): only `provider-imap`, for exactly the
reason below — it owns the finished `TlsStream`. A `reqwest` adapter has no version
to report and invents none.

- **JMAP / CalDAV / Graph** (`reqwest`): `http_version` comes from
  `reqwest::Response::version()`, recorded at each transport's single response funnel
  into a shared `engine_provider::ObservedHttpVersion`. It is the **latest** observation,
  not the first: both JMAP and CalDAV disable reqwest's redirect following and resolve
  the well-known `30x` themselves, so the first response belongs to the redirector —
  possibly a different origin, and a different negotiated version, from the `apiUrl` /
  calendar home that serves every real request. Latching the first would permanently
  misreport those providers.
  `tls_version` is **always `None`**: reqwest 0.13's `TlsInfo` exposes only the peer
  certificate, never the negotiated protocol version (its internal
  `Version::from_rustls` serves min/max config only). Extracting it would need a
  custom connector layer that downcasts to the rustls stream — brittle across
  reqwest/hyper bumps, for a fact these providers negotiate at TLS 1.3 in practice.
  **Do not add one.**

  Tracked upstream as [seanmonstar/reqwest#3066][reqwest-tls-version], which proposes
  a `TlsInfo::negotiated_version() -> Option<reqwest::tls::Version>` populated from
  `rustls`'s `CommonState::protocol_version()`. If that lands, the fix here is to read
  it off the response (behind `ClientBuilder::tls_info(true)`) and fill `tls_version`
  in for the three HTTP adapters — **not** to write a connector. Until then the `None`
  is correct and deliberate, and the asymmetry it creates is the reason
  [`ConnectionInfo`]'s two version fields are independently optional.

[reqwest-tls-version]: https://github.com/seanmonstar/reqwest/issues/3066

## Testing

- Offline provider fakes bypass TLS entirely, so unit/offline tests are unaffected.
- The Stalwart harness serves CalDAV/JMAP over **plaintext HTTP** and its IMAP
  self-signed cert is runtime-generated, so the live suite validates *function*, not
  reqwest certificate verification.
- `engine-tls`'s `tests/roundtrip.rs` is the authoritative verification proof: an
  in-process `tokio-rustls` server proves one policy makes **both** the reqwest
  client and the connector accept a trusted (pinned/union) cert and reject an
  untrusted (bundled) one.
- `provider-imap`'s `tls_info` tests stand up an in-process TLS server speaking just
  enough IMAP to complete `connect_session`, pinned once to the default versions and
  once to TLS 1.2 — so the reported version is proven to be *read from the handshake*,
  not assumed. The reqwest adapters' `http_version` is covered the same way, against
  their in-process mock HTTP/1.1 servers.

## Host / FFI wiring

The `TlsPolicy` enum + DER bytes is the FFI-facing surface; `TlsClientConfig` is an
opaque handle. A host builds a policy per platform, realizes one `TlsClientConfig`,
and threads it into every provider. With `bundled_and_system()`, Android needs no
JVM `Context` (native-certs reads the store directly); a `PlatformVerifier` build is
the only one that requires the host to initialize the platform verifier first.
Exposing `TlsPolicy` over the UniFFI/C-ABI bindings is a later slice.
