# Design note: one provider-neutral `CredentialSource`

Status: proposal. Direction approved by the owner: **credentials are asked for, not held.** This
note lands before the capability-trait split, which comes after the query seam and this.

## Problem

Five of the six protocol paths take a credential once, at construction, and keep it for the life
of the client. Only IMAP/SMTP asks a source each time it dials. So every host that uses OAuth wraps
the HTTP adapters to rebuild them when a token changes, and none of the HTTP adapters can recover
from a token the server refuses before it expires. Each adapter also has its own credential type
(`provider_imap::Credentials`, `provider_jmap::Credentials`, `provider_caldav::Credentials`, a bare
`String` for Graph and Google), and only CalDAV lets the host limit which hosts receive the
credential.

The engine serves three kinds of host, and none of them is well served by a credential the adapter
holds:

| | Sync host (cross-platform app) | Transfer host (migration tool) | Direct host (store-less webmail) |
|---|---|---|---|
| Where secrets live | OS keychain; refresh token in keychain | Token broker, per mailbox pair | The user's OIDC session, exchanged for a backend token per request |
| Credential kinds | Password, app password, OAuth (refreshed) | Graph app-only, Google domain-wide delegation, IMAP/JMAP password or OAuth | Bearer from token exchange; sometimes a password the backend issues |
| Principals per process | A few accounts | Many mailboxes, a few at a time per worker | Thousands of users at once |
| Adapter lifetime | Hours to days | One migration job (minutes to hours) | One user request, or a short idle window |
| Refresh | Proactive, before expiry; single-flight per account | The broker mints short-lived tokens; the worker never sees a refresh token | Per request; the session expires with the user's login |
| After a refusal | Refresh once and retry; then "sign in again" | Ask the broker once; then mark the pair as needing consent | Ask once; then send the user to log in again |
| Which hosts may receive it | The account's servers, plus whatever discovery reaches | Only the audience the broker minted the token for | Only the configured backend origins |
| Pooling | Per account, long-lived | Per mailbox, for one job | Per user, short idle timeout, process-wide ceiling |
| What it cannot afford | Rebuilding adapters every hour (JMAP re-runs session discovery) | A token for one tenant reaching another tenant's server | One adapter, one HTTP client and one TLS config per user |

## Survey of the adapters

Read from `origin/main` at `468c841`. "Construction" means the secret is copied into the
client when it is built and never asked for again.

| | IMAP | SMTP (in `provider-imap`) | JMAP | CalDAV / CardDAV | Graph | Google API |
|---|---|---|---|---|---|---|
| Credential types | `Credentials::{Password, OAuth2{username, access_token}}` | same type | `Credentials::{Basic{username,password}, Bearer(String)}` | `Credentials::{Basic, Bearer}` (a third type) | bare `String` token | bare `String` token |
| Where the secret is read | `CredentialSource::credentials()` on **every dial** (`dial.rs`, `account.rs::dial_tls`) | the same source, **once per submission** (`filing.rs`) | **construction** (`Transport::new`) | **construction** (`DavClient::new`) | **construction** (`HttpTransport::new`) | **construction** (`HttpTransport::new`) |
| Refresh | the source's job; asked again on each dial | the same | none: the host rebuilds the client | none: the host rebuilds the client | none: the host rebuilds the client | none: the host rebuilds the client |
| Refusal | `NO` → `ImapError::Auth` (`refused_sign_in`; `[LIMIT]` → rate-limited, temporary codes → retryable), then `renew()` once on a **new** connection (`with_renewal`) | `535` → `Auth`, then `renew()` once; `432` and other 4xx handled separately | `401` whose challenge lacks the scheme used: re-framed once (Basic ↔ Bearer) and latched (`auth.rs`). `401` that offers it: `Authentication`, no retry | no credential sent until a trusted origin challenges (`CredentialScope`); a `401` to a request that carried it: `Authentication`; a challenge for another scheme: `UnsupportedAuthentication` (permanent) | `401` → `Authentication`, no retry | `401` → `Authentication`, no retry |
| Connection pooling | `ImapPool`, one per `ImapAccount`, so per config and therefore per source; authenticated sockets, ceiling 5 | none: one connection per submission | `reqwest` pool inside one `Transport`, keyed by origin, unauthenticated | `reqwest` pool inside one `DavClient` | `reqwest` pool per `GraphClient`; `GraphProvider::new` takes the client **by value**, so one client per folder | `reqwest` pool per `GoogleClient` |
| Keyed by credential? | yes, implicitly: one pool per source | n/a | no (and HTTP auth is per request) | no | no | no |
| Host binding | the configured `server_name` only; no redirects | the configured host only | follows its own well-known redirects (`redirect_target`: https → http refused unless same host and port); takes the credential to whatever origin the chain ends at, and to the origins the session advertises (a `downloadUrl` on another origin is kept verbatim); no host policy; content URLs never | `CredentialScope`: trusted = configured origin plus the origins discovery reaches; optional host policy `CredentialHosts::permits(host)`; content URLs never | `same_origin(url, base)`; content URLs fetched anonymously | `same_origin(url, base)`; content URLs fetched anonymously |
| SASL / scheme | `LOGIN` for a password; `OAUTHBEARER` over `XOAUTH2`, chosen from `CAPABILITY` (`sasl.rs`) | `AUTH PLAIN`; `OAUTHBEARER` over `XOAUTH2` from `EHLO` | Basic or Bearer, server-chosen | Basic or Bearer | Bearer | Bearer |
| Debug redaction | yes, username kept | yes | yes, username kept | yes, username kept | client `Debug` omits the token | client `Debug` omits the token |

Additional findings that shape the design:

- **Google uses two transports for one account**: the API (`GoogleClient`, bearer) and Gmail IMAP
  (`GoogleProvider::with_imap`, SASL). The same token goes to both, one held and one asked for.
- **Microsoft needs a different token for each audience.** A Graph token is not accepted by its
  IMAP endpoint, and the reverse is also true. A source therefore has to know where the credential
  is going.
- `engine-http` has no `401` handling. Each adapter decides for itself (`http-throttling.md` owns
  `429`/`503` only).
- `engine_http::client` enables no cookie store. That is what makes it safe to share one `reqwest`
  client between principals (see "Direct host"), and it must stay that way.
- The per-account `RequestGate` (`http-throttling.md`, "One gate, one account") already has to
  live "wherever the host's per-account state lives". Credential state belongs in the same place.

**Survey result.** All five HTTP paths share the gap; IMAP/SMTP alone has the right shape.
The fix therefore lands in the neutral layer (`engine-provider`, with the HTTP half in
`engine-http`), not in any one adapter. The SASL mechanism choice and JMAP's Basic/Bearer re-framing
stay in their adapters, because they describe a protocol and not a credential.

## Design

### Types (in `engine-provider`, new module `credential.rs`)

```rust
/// A password, app password or access token. `Debug` prints `<redacted>`; no `Display`, no
/// `Serialize`. Clones share one buffer, which is zeroed when the last clone drops.
#[derive(Clone)]
pub struct Secret(Arc<zeroize::Zeroizing<Box<str>>>);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self;
    /// The only way to read it, and easy to grep for.
    pub fn expose(&self) -> &str;
}

#[derive(Clone)]
#[non_exhaustive]
pub enum Credential {
    /// IMAP `LOGIN`, SMTP `AUTH PLAIN`, HTTP Basic (JMAP may re-frame it as Bearer).
    Password { username: String, password: Secret },
    /// An OAuth 2.0 access token. IMAP/SMTP SASL needs `username` (the `user=` of `XOAUTH2`,
    /// the `a=` of `OAUTHBEARER`); HTTP adapters ignore it.
    Bearer { username: Option<String>, token: Secret },
}

/// Who a source signs in as: stable, not secret, unique within the process. Supplied by
/// the host (sync: its account id; transfer: the mailbox-pair side; direct: tenant + OIDC `sub`).
pub struct CredentialSubject(Box<str>);

/// Where a credential is about to go: scheme, lowercase host, effective port. For IMAP and SMTP
/// the host is the TLS server name, not the dial address (a loopback fixture maps them).
pub struct Origin { /* scheme, host, port */ }

#[non_exhaustive]
pub struct CredentialRequest<'a> {
    pub origin: &'a Origin,
}
```

There are no SASL-specific variants. `XOAUTH2` and `OAUTHBEARER` carry the same username and
token, and the adapter picks the mechanism from what the server advertises (`sasl.rs`). A caller
that could pin the mechanism would be saying which provider it talks to. An authorization
identity (IMAP `AUTHENTICATE PLAIN` with an `authzid`, or Dovecot master-user login), which a
transfer host might want for admin impersonation, is left out until a real need and a live test
exist (open question 4). `#[non_exhaustive]` lets it arrive as a later variant without breaking
anyone.

`Debug` on `Credential` prints the variant and `username: <present>`, not the address. The IMAP
type prints the username today. A host serving thousands of tenants should not have addresses
appear in a `{:?}` (open question 1).

### The trait

```rust
#[async_trait]
pub trait CredentialSource: Send + Sync + core::fmt::Debug {
    /// Who this source signs in as. Everything authenticated is keyed by it.
    fn subject(&self) -> &CredentialSubject;

    /// Whether the credential may go to `origin`. Checked before the first byte reaches the
    /// configured origin and before any origin discovery moves to. Default: yes.
    fn permits(&self, origin: &Origin) -> bool { true }

    /// The credential to present now. A token source returns one valid now, refreshing
    /// first if it is near expiry. Called concurrently; it must be cheap when cached and
    /// coalesce refreshes (single-flight).
    async fn credential(&self, request: &CredentialRequest<'_>) -> Result<Credential, ProviderError>;

    /// A replacement for `refused`, which the server has just refused, or `None`. Asked at
    /// most once per dial, submission or HTTP request. The default is `None`: the same
    /// password gets the same answer, and the provider may be counting failures toward a
    /// lockout. Callers refusing the same token share one refresh.
    async fn replace(
        &self,
        refused: &Credential,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> { Ok(None) }
}

/// A fixed credential as a source. `new` takes a subject; `unique` mints a process-unique one,
/// so two fixed sources never share a pool by accident.
pub struct FixedCredential { /* subject, credential */ }
```

**Account-scoped.** A source answers for one subject. An adapter instance is bound to one source
for its whole life, and nothing authenticated (an IMAP socket, a JMAP session, a CalDAV trusted
origin set, a request gate) is ever shared across subjects.

**Error classes** reuse `FailureClass`; no new variant is needed. A revoked grant, or a broker that
says "no consent for this mailbox", is `Authentication`. An unreachable token endpoint is
`Retryable`. A `429` from the token endpoint is `RateLimited`. "No credential for this origin" is
`Permanent`. The adapter passes the source's error through with its class unchanged (IMAP already
does, through `ImapError::Credential`). A server refusing a credential that was just replaced is
`Authentication`. Each ask is bounded by the dial deadline (`deadlines.md`), and a timeout is
`Retryable`, so a hung token endpoint cannot hang a sync (open question 3).

**Replacement after refusal** is one engine helper, generalized from `provider_imap::with_renewal`:
ask, attempt, and on a refusal ask `replace` once and attempt again. A second refusal is returned
as it came. What counts as a refusal is decided by each adapter, and the rule is narrow:

- IMAP: `ImapError::Auth` from `LOGIN`/`AUTHENTICATE` (not `[LIMIT]`, not temporary codes). The
  retry uses a **new connection**, as today.
- SMTP: `535` at `AUTH`, before `MAIL FROM`, so a send is never repeated.
- HTTP: a `401` to a request **that carried the credential**, where the challenge offers the
  credential's scheme or no challenge was sent at all (YAHOO-1 in `spec-deviations.md`). JMAP first
  runs its scheme re-framing, which does not change the credential, and asks for a replacement only
  if the `401` survives that. A CalDAV `401` to an anonymous request is a challenge, not a refusal.
  A `403` is never a refusal.
- A replay happens only on a `401` **response**, never after a transport failure. A `401` means the
  request was not processed, so replaying a submission (`sendMail`, `EmailSubmission/set`) is safe.
  The ambiguity rules in `deadlines.md` and the hand-over record stay untouched. A request whose
  body cannot be cloned is not replayed: it returns `Authentication`, and the outbox retries.

The HTTP half lives in `engine-http` beside the send funnel, as `send_authenticated(source,
request, build, is_refusal)`. `build` makes the request from a credential, and `is_refusal` is the
adapter's predicate. Every adapter goes through this one funnel, so no path can forget to ask, to
check `permits`, or to replay.

### Host binding

There are two layers, and the source can only narrow:

1. **Engine rules, which no source can loosen.** A URL named by content never receives the
   credential (`same_origin`, the anonymous byte fetch). A redirect that leaves TLS for another
   host is refused (`redirect_target`). Only origins the account's own server led discovery to are
   trusted (`CredentialScope`). IMAP and SMTP never authenticate before TLS (no STARTTLS downgrade;
   `Plaintext` SMTP sends no `AUTH`).
2. **`CredentialSource::permits(origin)`, the host's policy.** It is checked for the configured
   origin before anything is sent, and for every origin discovery adds. A refusal fails the
   connect, and nothing is sent. It replaces `provider_caldav::CredentialHosts`, which keeps
   working as a deprecated shim combined with `permits` (both must agree). The origin includes the
   scheme and port, not only the host, so a host can refuse plaintext or a nonstandard port.

The default stays "permit". That keeps today's behaviour for every host that sets nothing. That behaviour is deliberate, not a gap: RFC 8620 requires every request to be authenticated, downloads and uploads included (§6.1, §6.2), and puts no origin restriction on the session's `apiUrl`, `downloadUrl`, `uploadUrl` or `eventSourceUrl` (§2). A server the user trusts names those origins, and Fastmail serves `downloadUrl` from a separate cookie-less origin that needs the credential (`jmap.md`, the session bullet). TLS stays the floor (§8.1). `permits` lets a host narrow further; the engine never refuses a session-advertised origin by default. A host
holding OAuth tokens should override it: the sync host from the token's issuer, the transfer host
from the audience the broker minted for, and the direct host from its configured backend origins.

### Direct host: per-request credentials without rebuilding adapters

The source is the per-request part. Everything expensive moves into a shared **endpoint**, one per
backend per process, and the per-user provider becomes a cheap handle:

```rust
// Once per process per backend: HTTP client, TLS config, retry policy, per-subject caches.
let jmap = JmapEndpoint::new(base_url, &tls, &retry, &user_agent)?;
// Per user request: no I/O when the subject's session is cached.
let provider = jmap.bind(Arc::new(SessionSource::from(&oidc_session)));
```

- **HTTP connections are shared; authenticated state is not.** One `reqwest` client per endpoint
  is safe across users because `Authorization` travels on each request and there is no cookie store.
  A test should pin the absence of a cookie store. Keyed by `CredentialSubject`, with bounded size
  and idle eviction: the JMAP session (`apiUrl`, `primaryAccounts`, capabilities), the CalDAV
  trusted and answered origin sets and the discovered home, JMAP's latched scheme, and the
  `RequestGate`. Never keyed by origin alone: two users of one JMAP server have different sessions.
- **IMAP sockets are authenticated**, so the endpoint holds a pool registry keyed by `(origin,
  subject)`. Each pool has the existing per-subject ceiling, and the registry adds a process-wide
  ceiling with idle eviction. A pool remembers the username its first dial presented, and a later
  credential naming another username fails the dial (`InvalidState`). That cheaply catches a host
  bug that gives one user's subject another user's credential.
- **Graph and Google have no session**, so `bind` there is just "share the client". This also fixes
  the sync host's one-client-per-folder fan-out (`GraphProvider::new` takes the client by value).
- `connect` keeps working for sync and transfer hosts: it is `endpoint.bind(source)` followed by
  the first request.

Two alternatives were rejected:

- **An ambient credential in a task-local.** It is invisible in signatures, it is lost in spawned
  tasks (watch loops, pool dials), and a pool dial made on behalf of user A from user B's task
  would present B's credential. That is a cross-tenant bug with no type to stop it.
- **A credential argument on every `Provider` method.** It breaks every method on every adapter
  and collides with the capability-trait split, which is about to reshape those same signatures.

## Migration per adapter

| Adapter | Change | Compatibility |
|---|---|---|
| `engine-provider` | New `credential.rs` (types, trait, `FixedCredential`, replacement helper); adds the `zeroize` dependency (already in the tree through rustls; MIT/Apache-2.0) | Additive |
| `engine-http` | `send_authenticated` | Additive |
| `provider-imap` | `ImapConfig` holds `Arc<dyn engine_provider::CredentialSource>`; `with_renewal` becomes the shared helper; `dial`/`filing`/`smtp_auth` match on `Credential` | **Breaking** for implementers of `provider_imap::CredentialSource` (`credentials`→`credential`, `renew`→`replace`, new `subject`) and for matches on `Credentials::OAuth2`. Callers keep compiling: a deprecated `Credentials` alias with its `password`/`oauth2` constructors, and `ImapConfig::new` taking a fixed credential |
| `provider-jmap` | `Transport` asks per request through `send_authenticated`; re-framing reads `Credential`; `JmapConfig::from_credential_source`; later `JmapEndpoint` | `JmapConfig::new(base, provider_jmap::Credentials, ua)` kept and converted into a `FixedCredential` with a unique subject. **Behaviour change:** a `401` can now cause one replayed request when the source replaces |
| `provider-caldav` / CardDAV | `DavClient` asks per request; `CredentialScope` works on `Credential`; `permits` merged with `CredentialHosts` | **Breaking:** `CalDavConfig.credentials` and `CardDavConfig.credentials` are `pub` fields of `provider_caldav::Credentials`. Replacing them with a source breaks struct-literal construction. Constructors stay. `CredentialHosts` and `with_credential_hosts` are deprecated, not removed |
| `provider-graph` | `HttpTransport` asks per request; `GraphClient::connect_with_source`; `GraphProvider` shares the client (`Arc`) | `connect(token, …)`, `for_mailbox` and `with_base` kept, wrapping a fixed bearer. `GraphProvider::new(GraphClient, …)` can keep its signature if `GraphClient` becomes a cheap clone |
| `provider-google` | Same as Graph; `with_imap` and the API side bind **one** source, so both transports present the same token | `connect(token, …)` kept |

### What the sync host's wrappers become

These live in `mail-calendar/crates/mailcal-account`, read-only here:

- **`jmap/refreshing.rs` + `refreshing_provider.rs` (`RefreshingJmapProvider`), and
  `refreshing_contacts.rs`.** They mint a token before every call and rebuild `JmapProvider` when
  it changed, which re-runs session discovery about hourly. **They disappear.** The provider is
  built once over the account's source.
- **The token half of `RefreshingGraphProvider` and `RefreshingGmailProvider`
  (`graph/mail_provider.rs`, `google/mail_provider.rs`), which cache a delegate keyed by token.
  This half disappears**, and with it the hourly rebuild of the HTTP client. A token revoked before
  its stated expiry, which these wrappers never retry today, now recovers through `replace`.
  `429` waiting already moved into `engine-http`.
- **`reconnect.rs` (`ReconnectingImapProvider`), `stream_redial.rs`, and `should_reconnect` in the
  Graph and Google wrappers.** They retry once on a `Retryable` transport failure, typically a
  socket that died while the machine slept. That is about transport, not credentials, and **they
  stay**. Moving stale-socket recovery into the engine is a separate design.
- **`imap_credentials.rs` (`OAuthCredentialSource`)** becomes the one implementation of
  `engine_provider::CredentialSource`, shared by every adapter of an account. It already does
  single-flight replacement (`GraphTokenSource::access_token_replacing`).
- **The IMAP-or-API decision in `google/imap.rs`** (falling back to the API after repeated IMAP
  sign-in refusals) is product policy, and it stays.

## Security review

- **Nothing secret in logs or errors.** `Secret` has a redacted `Debug` and no `Display` or
  `Serialize`; `expose()` is the only accessor. Errors on the credential path carry a class and a
  fixed detail, never the credential and never the SASL initial response. The engine has no logger.
  `ConnectStep` and `ThrottleEvent` carry no credential and no URL, and nothing new is added to
  them. Error `detail()` strings can still contain server text (a SASL failure JSON of up to 512
  bytes, a Graph error body). Hosts with a "no server text in logs" rule must log the class only,
  as they do today.
- **Zeroizing is best effort, and the note says so.** The engine owns one buffer per secret and
  zeroes it on the last drop. The adapters wrap their own derived copies in `Zeroizing`: the
  base64 SASL response, the `AUTH PLAIN` token, the Basic header input. Copies inside `reqwest`
  `HeaderValue`s (marked sensitive, not zeroed), rustls write buffers and the host's own memory are
  out of reach. The goal is to shorten the lifetime of copies the engine owns, not to guarantee
  that none remain.
- **No reuse across tenants.** A provider gets its source at bind time and never from ambient
  state. Pools, sessions, discovery state and gates are keyed by `CredentialSubject`. The IMAP
  username is pinned per pool. A shared HTTP client is safe only while no cookie store and no
  connection-bound auth (NTLM, Negotiate) exist; CalDAV already refuses those as
  `UnsupportedAuthentication`. A subject is unique within the process, which an `AccountId` is not:
  a store-less multi-tenant host may reuse account ids.
- **TLS.** There is one `TlsPolicy` (`tls.md`), unchanged. Credentials never go over a hop that
  left TLS. A plaintext configured origin is still allowed, because the loopback fixtures need it.
  A host can refuse it through `permits` (open question 6).
- **Lockout.** Password sources never replace by default, and each operation asks for at most one
  replacement. IMAP's `[LIMIT]` and Dovecot's connection-limit-shown-as-`AUTHENTICATIONFAILED`
  (`pool.rs`) keep their current handling.

## Phasing

1. **Slice 1: the neutral types, adopted by IMAP/SMTP.** `engine-provider::credential` plus the
   replacement helper, with `provider-imap` moved onto them. IMAP goes first because it already has
   exactly these semantics: the slice proves the neutral trait against existing tests and live
   servers, and changes no request shape. The sync host re-implements `OAuthCredentialSource`.
2. **JMAP**, together with `engine-http::send_authenticated`. This is the first per-request HTTP
   ask, and it deletes `RefreshingJmapProvider`.
3. **CalDAV/CardDAV**: `permits` absorbs `CredentialHosts`, and the scope is keyed by subject.
4. **Graph and Google API**: the same bearer funnel. Google's IMAP and API halves share one source.
   This deletes the token halves of the Graph and Gmail wrappers.
5. **Endpoints and `bind` for the direct host**: shared HTTP clients, per-subject caches, the IMAP
   pool registry with a process ceiling.

Then the capability-trait split, which can assume one credential seam.

## Test plan

Offline (each new check is proven by reverting the fix and watching it fail):

- `engine-provider`: `Secret` and `Credential` `Debug` never contain the secret or the address;
  `Secret` is `ZeroizeOnDrop` (a trait-bound assertion). The replacement helper asks once, replaces
  at most once, returns the second refusal as it came, passes source error classes through, and a
  fixed credential never replaces.
- IMAP/SMTP: `dial_renewal_tests.rs` and `filing_smtp_server_tests.rs` carry over unchanged in
  behaviour.
- HTTP adapters, where the fakes **assert the request** rather than answer anything they are sent:
  request 1 carries token A, the reply is `401`, the replay carries token B, and the source sees
  exactly one ask and one replace. No replace happens for a `403`, for a `401` to an anonymous
  CalDAV request, after a transport failure, or for a body that cannot be cloned. A refused origin
  receives zero requests. Content URLs never carry `Authorization` (the existing tests, kept).
- Across tenants: two subjects on one endpoint, where every request's `Authorization` belongs to
  its own subject. The IMAP registry never hands subject A's socket to B (`MockStream` `LOGIN`
  transcripts). A username change inside one pool is refused. Keying the cache by origin alone
  must turn these tests red.

Live (`AGENTS.md`: any new request shape is run against a real server):

- **IMAP**: Dovecot (its static passdb lets a test source present a wrong password, then the right
  one through `replace`, which proves the re-dial and the second `LOGIN`) and Stalwart.
  `live_imap_oauth.rs` keeps covering both SASL token mechanisms (throwaway Gmail account).
- **SMTP**: Stalwart submission on 465 and 587, wrong then right.
- **JMAP**: Stalwart, wrong Basic then right. This proves the replayed request and that the `401`
  offered `Basic` (re-framing and replacement must not both fire). A real bearer refresh needs a
  token from Stalwart's own OAuth endpoint, and the harness cannot mint one yet (open question 9).
- **CalDAV/CardDAV**: Stalwart, SabreDAV and Nextcloud, wrong then right on an origin already
  authenticated after discovery, plus a full discovery with `permits` limited to the harness host,
  showing nothing regresses.
- **Graph and Google**: these cannot run in a local harness. They are verified with the test
  Microsoft account and the throwaway Google account, with tokens from `tools/graph-oauth` and
  `tools/google-oauth`: present a garbage or revoked token first, observe the `401`, and succeed
  through `replace`. Each `401` envelope is captured and scrubbed (the existing `UNAUTHORIZED`
  fixtures already have the right shape). If the owner cannot run them, the PR names them as
  unverified.

## Decisions (2026-11-23)

- **Transfer hosts get administrative credentials.** Migrations at scale run on master or application-level access, with per-user credentials as the fallback where that is not possible: an IMAP authorization identity (`AUTHENTICATE PLAIN` with an `authzid`, Dovecot master users), Microsoft Graph application permissions, Google domain-wide delegation. `Credential` gains the variant this needs, proven live on Dovecot before it ships. This settles open question 4.
- **Session-advertised JMAP origins stay trusted** (see "Host binding"). Nothing here changes that.
- **Capabilities are reached through accessors on `Provider`.** A new capability is its own trait, reached through a `Provider` method that defaults to `None` (for example `fn mail_rules(&self) -> Option<&dyn MailRules> { None }`). It is additive, works through `Box<dyn Provider>`, and is the first step of the capability-trait split. It supersedes the `providers.md` rule that rejects sub-traits; that doc changes with the first slice that adds one.

## Open questions

1. Should `Credential`'s `Debug` keep the username, which helps support for the sync host, or drop
   it, which protects privacy on multi-tenant hosts? This note drops it.
2. Should the subject be a separate `CredentialSubject` newtype or the existing `AccountId`? This
   note says separate, because a store-less host may reuse account ids across tenants.
3. Which deadline applies to an ask to the source, and is a timeout `Retryable`? The proposal is
   the dial deadline and `Retryable`.
4. Admin impersonation for transfer hosts (IMAP `AUTHENTICATE PLAIN` with `authzid`, or Dovecot
   master users): add a variant now or wait for a need? This is a new request shape and needs a
   live Dovecot test.
5. Does a host need to tell "the source failed" apart from "the server refused", beyond the class?
   Today the class already separates broker-down (`Retryable`) from consent-gone (`Authentication`).
6. Should credentials over plaintext HTTP be allowed only for loopback origins, rather than left to
   `permits`?
7. Who owns eviction for the IMAP pool registry and the per-subject caches in the direct host:
   engine defaults with host overrides, or entirely the host?
8. Should `CredentialRequest` also name the protocol (IMAP, SMTP or HTTP) for hosts with separate
   SMTP credentials? The origin already tells them apart by host and port.
9. Can the Stalwart harness mint OAuth access tokens, so that bearer replacement runs live locally
   for JMAP and CalDAV?
