# Design note: one provider-neutral `CredentialSource`

Status: accepted; steps 1 to 5 of "Phasing" have landed (`engine_provider::credential`,
IMAP/SMTP on it, the IMAP authorization identity, JMAP on `engine_http::AuthenticatedClient`,
CalDAV, CardDAV and Talk on the same client, and Microsoft Graph and the Google APIs on it too,
with Google's IMAP and API halves bound to one credential subject). Every protocol path now asks. The direction is principle 7 of
`north-star.md`: **credentials are asked for, not held.** The owner settled every open
question on 2026-11-23 ("Decisions", at the end), and the body below states each decision where
it applies. One `CredentialSource` is a "Next" step in
`north-star.md` ("Direction"); the capability-trait split comes after it and the query seam
(`north-star.md`, "Key Decisions").

## Problem

Five of the six protocol paths take a credential once, at construction, and keep it for the life
of the client. Only IMAP/SMTP asks a source each time it dials. So every host that uses OAuth wraps
the HTTP adapters to rebuild them when a token changes, and none of the HTTP adapters can recover
from a token the server refuses before it expires. Each adapter also has its own credential type
(`provider_imap::Credentials`, `provider_jmap::Credentials`, `provider_caldav::Credentials`, a bare
`String` for Graph and Google), and only CalDAV lets the host limit which hosts receive the
credential.

The engine serves three kinds of host (`north-star.md`, "Hosts"), and none of them is well served
by a credential the adapter holds:

| | Sync host (cross-platform app) | Transfer host (migration tool) | Direct host (store-less webmail) |
|---|---|---|---|
| Where secrets live | OS keychain; refresh token in keychain | Token broker, per mailbox pair | The user's OIDC session, exchanged for a backend token per request |
| Credential kinds | Password, app password, OAuth (refreshed) | Administrative first: Graph application permissions, Google domain-wide delegation, an IMAP authorization identity (Dovecot master user); per-user password or OAuth as fallback | Bearer from token exchange; sometimes a password the backend issues |
| Principals per process | A few accounts | Many mailboxes, a few at a time per worker | Thousands of users at once |
| Adapter lifetime | Hours to days | One migration job (minutes to hours) | One user request, or a short idle window |
| Refresh | Proactive, before expiry; single-flight per account | The broker mints short-lived tokens; the worker never sees a refresh token | Per request; the session expires with the user's login |
| After a refusal | Refresh once and retry; then "sign in again" | Ask the broker once; then mark the pair as needing consent | Ask once; then send the user to log in again |
| Which hosts may receive it | The account's servers, plus whatever discovery reaches | Only the audience the broker minted the token for | Only the configured backend origins |
| Pooling | Per account, long-lived | Per mailbox, for one job | Per user, short idle timeout, process-wide ceiling |
| What it cannot afford | Rebuilding adapters every hour (JMAP re-runs session discovery) | A token for one tenant reaching another tenant's server | One adapter, one HTTP client and one TLS config per user |

## Survey of the adapters

Read from `origin/main` at `468c841`, and checked again at `d84cc61`: none of the credential
paths below changed in between. The IMAP import added since (#375, #376; `import.rs`) borrows a
pooled session like every other IMAP path, so it asks the source on each dial too. "Construction"
means the secret is copied into the client when it is built and never asked for again.

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
  is going, and over which protocol.
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
/// Its own type, not `AccountId`: a store-less host may reuse account ids across tenants.
/// `Debug` says named or minted, never the name, which is often personal.
pub struct CredentialSubject(Subject);
enum Subject { Named(Box<str>), Unique(u64) } // private: `new(name)`, or minted by
                                              // `FixedCredential::unique`, never equal to a name

/// Where a credential is about to go: scheme, lowercase host, effective port. For IMAP and SMTP
/// the host is the TLS server name, not the dial address (a loopback fixture maps them).
pub struct Origin { /* scheme, host, port */ }

/// The protocol a credential is about to be presented over.
#[non_exhaustive]
pub enum CredentialProtocol { Imap, Smtp, ManageSieve, Http }

#[non_exhaustive]
pub struct CredentialRequest<'a> {
    pub protocol: CredentialProtocol,
    pub origin: &'a Origin,
}
```

`CredentialRequest` names the protocol as well as the origin. The origin alone usually tells IMAP
from SMTP by host and port, but a host with separate SMTP credentials should not have to infer the
protocol from a port number. Most sources ignore both fields.

There are no SASL-specific variants. `XOAUTH2` and `OAUTHBEARER` carry the same username and
token, and the adapter picks the mechanism from what the server advertises (`sasl.rs`). A caller
that could pin the mechanism would be saying which provider it talks to.

**Administrative credentials for transfer hosts.** A migration at scale signs in with
administrative access where the provider offers it, and with each user's own credential only
where it does not. Two of the four administrative kinds fit the variants above. A Graph token
with application permissions is a `Bearer` the broker mints, and the adapter names the mailbox in
the URL (`GraphClient::for_mailbox` with `MailboxPrincipal::user`, rooted at `/users/{address}`).
A Google domain-wide-delegation token is a `Bearer` minted for one user, presented to Gmail IMAP
with that user's address as `username`. The other two are one shape: an IMAP authorization
identity, sent as `AUTHENTICATE PLAIN` with an `authzid` (RFC 4616), which is also how a Dovecot
master user signs in as another user. `Credential` gains one variant for it, carrying the
administrator's username and secret and the mailbox to act as. It is a new request shape, so it
ships only after a live Dovecot test with a master user proves it ("Phasing", step 2).
`#[non_exhaustive]` lets it arrive without breaking anyone. As built it is
`Credential::ActingAs { username, password, acting_as }` (`Credential::acting_as`). **The engine
cannot verify that the server honoured the authorization identity**: IMAP has no way to ask whose
session opened, and a server that ignores it signs the administrator in to its own mailbox
without an error (Stalwart does, `spec-deviations.md`, STALWART-9). A host uses the variant
only against servers it has verified honour it.

`Debug` on `Credential` prints the variant and `username: <present>`, never the username itself.
Today's IMAP, JMAP and CalDAV types print it. A host serving thousands of tenants must not have
addresses appear in a `{:?}`, and a sync host that wants one in a support log can log its own
account record.

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
says "no consent for this mailbox", is `Authentication`. An unreachable token endpoint or broker is
`Retryable`. A `429` from the token endpoint is `RateLimited`. "No credential for this origin" is
`Permanent`. The adapter passes the source's error through with its class unchanged (IMAP already
does, through `ImapError::Credential`). A server refusing a credential that was just replaced is
`Authentication`. The class is all a host needs to tell "the source failed" from "the server
refused": a broker that is down is `Retryable`, and consent that is gone is `Authentication`,
which is also what a server's refusal is and needs the same answer (sign in or consent again). No
separate variant or flag is added for it.

**Each ask has a deadline.** An ask to the source (`credential` or `replace`) runs under the
deadline of the step it belongs to: the dial deadline on IMAP and SMTP, where the ask is part of
the dial, and the request's deadline on HTTP, where it is part of the request (`deadlines.md`,
which gains the line for it in the slice that ships it). A timeout is `Retryable`, so a hung token
endpoint or broker cannot hang a sync, a transfer or a user's request.

**Replacement after refusal** is one engine helper, generalized from `provider_imap::with_renewal`:
ask, attempt, and on a refusal ask `replace` once and attempt again. A second refusal is returned
as it came. What counts as a refusal is decided by each adapter, and the rule is narrow:

- IMAP: `ImapError::Auth` from `LOGIN`/`AUTHENTICATE` (not `[LIMIT]`, not temporary codes, not a
  server offering no mechanism the credential can use). The retry uses a **new connection**, as
  today.
- SMTP: `535` at `AUTH`, before `MAIL FROM`, so a send is never repeated. Other `AUTH` failures
  (`504`, `530`, `534`, `538`, `432`) keep the `Authentication` class as
  `ImapError::AuthUnsupported`, and ask for no replacement.
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
check `permits`, or to replay. (As built in step 3, the funnel is
`engine_http::AuthenticatedClient`, which also builds the client it sends on; see "Phasing".)

### Host binding

There are two layers, and the source can only narrow:

1. **Engine rules, which no source can loosen.** A URL named by content never receives the
   credential (`same_origin`, the anonymous byte fetch). A redirect that leaves TLS for another
   host is refused (`redirect_target`). Only origins the account's own server led discovery to are
   trusted (`CredentialScope`). IMAP and SMTP never authenticate before TLS (no STARTTLS downgrade;
   `Plaintext` SMTP sends no `AUTH`). **No credential goes over plaintext HTTP, except to a
   loopback origin**: the name `localhost` or a loopback address (`127.0.0.0/8`, `::1`). An
   `http://` origin anywhere else, whether configured, reached by discovery or advertised by a
   JMAP session, gets no credential, and the request fails as `Permanent` before anything is sent.
   This is an engine rule rather than a `permits` default, so no host can forget it. Every live
   harness serves on `127.0.0.1`, so none of them is affected.

   **The loopback exception holds only for an account configured on plaintext loopback**
   (`engine_http::AuthenticatedClient` is built for the configured origin). This narrows decision 5
   below, which allowed any loopback origin. An account configured over `https` never sends its
   credential over `http://`, loopback included: a session served over `https` may advertise an
   `http://127.0.0.1:…` `eventSourceUrl`, `downloadUrl` or `uploadUrl` (kept verbatim where its
   origin differs from the API's, and always under `TrustAdvertised`), and whatever listens on
   the worker's own loopback, which a multi-tenant host shares with every other process, would
   read the credential in cleartext. It is the rule `redirect_target` already applies to a hop
   off TLS. The exception exists for harnesses and servers on the same machine, and those are
   configured on loopback themselves.

   **No proxy sees a plaintext credential.** reqwest reads `HTTP_PROXY` and `ALL_PROXY` from the
   environment when a client is built and applies them to loopback too, unless `NO_PROXY` lists
   it (reqwest 0.13.5, `async_impl/client.rs`, `auto_sys_proxy`; the matcher is hyper-util
   0.1.20's `client/proxy/matcher.rs`, whose `intercept` skips only what `NO_PROXY` names). So
   `engine_http::AuthenticatedClient::new` builds its client with `no_proxy()`
   when the configured origin is plaintext loopback, the one case where a credential goes in
   cleartext and a case that never needs a proxy, and keeps the environment's proxies for every
   other account, so a host behind a corporate proxy still reaches its servers over `https`.
   `engine_http::client` is unchanged; the authenticated client is built on it (through a private
   `client_for`, which nothing outside `engine-http` needs).
2. **`CredentialSource::permits(origin)`, the host's policy.** It is checked for the configured
   origin before anything is sent, and for every origin discovery adds. A refusal fails the
   connect, and nothing is sent. It replaces `provider_caldav::CredentialHosts`, which keeps
   working as a deprecated shim combined with `permits` (both must agree). An adapter that must
   decide before it sends whether an origin may ever receive the credential (CalDAV discovery,
   before it moves to an origin) asks `AuthenticatedClient::admits`, which gives the plaintext
   rule and `permits` as one answer, and asks the source for no credential. The origin includes the
   scheme and port, not only the host, so a host can refuse a nonstandard port, or plaintext even
   on loopback.

The default stays "permit". That keeps today's behaviour for every host that sets nothing, apart
from the plaintext rule above. Trusting the origins a JMAP session advertises is deliberate, not a
gap: RFC 8620 requires every request to be authenticated, downloads and uploads included (§6.1,
§6.2), and puts no origin restriction on the session's `apiUrl`, `downloadUrl`, `uploadUrl` or
`eventSourceUrl` (§2). A server the user trusts names those origins, and Fastmail serves
`downloadUrl` from a separate cookie-less origin that needs the credential (`jmap.md`, the session
bullet). TLS stays the floor (§8.1), with loopback the only exception. `permits` lets a host
narrow further; the engine refuses a session-advertised origin only under the rules in layer 1. A
host holding OAuth tokens should override `permits`: the sync host from the token's issuer, the
transfer host from the audience the broker minted for, and the direct host from its configured
backend origins.

### Direct host: per-request credentials without rebuilding adapters

The source is the per-request part. Everything expensive moves into a shared **endpoint**, one per
backend per process, and the per-user provider becomes a cheap handle. The endpoints are what
`engine-direct`, the planned store-less facade (`north-star.md`), builds on:

```rust
// Once per process per backend: HTTP client, TLS config, retry policy, per-subject caches.
let jmap = JmapEndpoint::new(base_url, &tls, &retry, &user_agent)?;
// Per user request: no I/O when the subject's session is cached.
let provider = jmap.bind(Arc::new(SessionSource::from(&oidc_session)));
```

- **HTTP connections are shared; authenticated state is not.** One `reqwest` client per endpoint
  is safe across users because `Authorization` travels on each request and there is no cookie store.
  A test should pin the absence of a cookie store. Keyed by `CredentialSubject`: the JMAP session
  (`apiUrl`, `primaryAccounts`, capabilities), the CalDAV trusted and answered origin sets and the
  discovered home, JMAP's latched scheme, and the `RequestGate`. Never keyed by origin alone: two
  users of one JMAP server have different sessions.
- **IMAP sockets are authenticated**, so the endpoint holds a pool registry keyed by `(origin,
  subject)`. Each pool has the existing per-subject ceiling, and the registry adds a process-wide
  ceiling. A pool remembers the username its first dial signed in with (for the
  authorization-identity variant, the mailbox acted as, not the administrator), and a later
  credential naming another fails the dial (`InvalidState`). That cheaply catches a host bug that
  gives one user's subject another user's credential. This check landed in step 2, on today's
  per-account pool (`provider_imap::credentials::PresentedIdentity`); a dial whose credential was
  refused fixes nothing.
- **Eviction has engine defaults that the host can override.** The per-subject caches are bounded
  in size and evicted when idle, and the IMAP pool registry has a process-wide ceiling and an idle
  timeout. The engine sets defaults, so a host that sets nothing is still bounded, and a host
  overrides them when it builds the endpoint, because only it knows how many users it serves and
  how long they stay.
- **Graph and Google have no session**, so `bind` there is just "share the client". This also fixes
  the sync host's one-client-per-folder fan-out (`GraphProvider::new` takes the client by value).
- `connect` keeps working for sync and transfer hosts: it is `endpoint.bind(source)` followed by
  the first request.

Two alternatives were rejected:

- **An ambient credential in a task-local.** It is invisible in signatures, it is lost in spawned
  tasks (watch loops, pool dials), and a pool dial made on behalf of user A from user B's task
  would present B's credential. That is a cross-tenant bug with no type to stop it.
- **A credential argument on every `Provider` method.** It breaks every method on every adapter
  and collides with the capability-trait split, which will reshape those same signatures.

New capabilities are reached the same way as import: through an accessor on `Provider` that
defaults to `None`, as `Provider::mail_importer` does (#375; `providers.md`, "Import";
`north-star.md`, "Key Decisions"). Nothing in this note adds one; the endpoints and `bind` change
how a provider is built, not how a capability is reached.

## Migration per adapter

| Adapter | Change | Compatibility |
|---|---|---|
| `engine-provider` | New `credential.rs` (types, trait, `FixedCredential`, replacement helper); adds the `zeroize` dependency (already in the tree through rustls; MIT/Apache-2.0) | Additive |
| `engine-http` | `AuthenticatedClient` (`send_authenticated` in the plan), which also enforces the plaintext rule ("Host binding") | Additive API. **Behaviour change:** a credential is no longer sent to an `http://` origin other than loopback |
| `provider-imap` | `ImapConfig` holds `Arc<dyn engine_provider::CredentialSource>`; `with_renewal` becomes the shared helper; `dial`/`filing`/`smtp_auth` match on `Credential` | **Breaking**: (1) implementers of `provider_imap::CredentialSource` (`credentials`→`credential(request)`, `renew`→`replace(refused, request)`, new `subject`), and callers of `source.credentials()`/`renew()` too; (2) matches on `Credentials::OAuth2` (now `Credential::Bearer`); (3) `Credentials::username()` returns `Option<&str>`; (4) `Password.password` is a `Secret`, which breaks struct literals and reads of the field as a `String`; (5) a bare `Credentials` is no longer a source (`Arc::new(credentials)`); (6) `Debug` of `ImapConfig` and the credential no longer prints the username; (7) the shims are `#[deprecated]`, so a caller building with `-D warnings` fails until it moves to the new names. Callers otherwise keep compiling: a deprecated `Credentials` alias with its `password`/`oauth2` constructors, and `ImapConfig::new` taking a fixed credential. Not breaking (`ImapError` is `#[non_exhaustive]`): the new `ImapError::AuthUnsupported`, which some failures that were `Auth` now are, with the same class |
| `provider-jmap` | `Transport` asks per request through an `AuthenticatedClient`; re-framing reads `Credential`; `JmapConfig::from_credential_source`; later `JmapEndpoint` | `JmapConfig::new(base, provider_jmap::Credentials, ua)` kept and converted into a `FixedCredential` with a unique subject. **Behaviour change:** a `401` can now cause one replayed request when the source replaces |
| `provider-caldav` / CardDAV / Talk | `DavClient` and `TalkMeetings` send on an `AuthenticatedClient`; `CredentialScope` reads the credential's scheme; `permits` merged with `CredentialHosts`; `from_credential_source` on `CalDavConfig`, `CardDavConfig` and `TalkMeetings` | **Breaking:** the `pub credentials` field of `CalDavConfig` and `CardDavConfig` is gone (the source is private, as JMAP's is), so code reading or assigning it breaks; neither struct could be built as a literal before (private fields). `new` and `TalkMeetings::new` keep taking `provider_caldav::Credentials`, as a fixed credential under a unique subject. `CredentialHosts` and `with_credential_hosts` are deprecated, not removed, so a caller building with `-D warnings` fails until it moves to `permits`. **Behaviour changes:** no credential over plaintext except loopback-from-loopback (a plaintext server elsewhere fails the connect as `Permanent` before anything is sent); one replacement and replay on a `401` the source can answer; the configs' `Debug` no longer prints the username |
| `provider-graph` | `HttpTransport` sends on an `AuthenticatedClient`; `GraphClient::connect_with_source` and `with_base_and_source`; `with_principal` public; `GraphClient` a cheap clone sharing one transport (`Arc`), so `GraphProvider` shares the client | Additive: `connect(token, …)`, `for_mailbox` and `with_base` kept, wrapping a fixed bearer under a unique subject; `GraphProvider::new(GraphClient, …)` keeps its signature. `GraphError::Credential` is new (`#[non_exhaustive]`). **Behaviour changes:** one replacement and replay on a `401` with a `Bearer` challenge; no redirect followed on an authenticated request (a `3xx` is returned as it came); no token over plaintext except loopback-from-loopback (`with_base` to an `http://` base off loopback now fails each request as `Permanent`); `with_base` refuses a base that is not an absolute URL at construction |
| `provider-google` | Same as Graph (`GoogleClient::connect_with_source`, `with_base_and_source`, cheap clone); `GmailProvider::with_imap` checks that the IMAP reader signs in as the client's subject (`GmailMail::credential_subject`, new in `provider-imap`) | **Breaking:** `with_imap` returns `ProviderResult<Self>`, and refuses (`InvalidState`) a reader that signs in as another credential subject, which includes every pairing of `GoogleClient::connect(token)` with `ImapConfig::new(credential)`: each is a fixed credential under a subject of its own. `connect(token, …)` and `with_base` are kept. `GoogleError::Credential` is new. **Behaviour changes:** as Graph's |

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
  left TLS, and never over plaintext HTTP except to a loopback origin from an account configured
  on loopback, which the live harnesses
  and the offline fakes need. That is an engine rule, not a `permits` default ("Host binding"),
  so a host cannot loosen it and need not remember it. A host can still refuse plaintext on
  loopback through `permits`.
- **Lockout.** Password sources never replace by default, and each operation asks for at most one
  replacement. IMAP's `[LIMIT]` and Dovecot's connection-limit-shown-as-`AUTHENTICATIONFAILED`
  (`pool.rs`) keep their current handling.

## Phasing

1. **Slice 1: the neutral types, adopted by IMAP/SMTP.** *Landed.* `engine-provider::credential`
   plus the replacement helper, with `provider-imap` moved onto them. IMAP goes first because it
   already has exactly these semantics: the slice proves the neutral trait against existing tests
   and live servers, and changes no request shape. The sync host re-implements
   `OAuthCredentialSource`. As built: the helper is `engine_provider::with_credential(source,
   request, ask_limit, is_refusal, attempt)`, which also checks `permits` before the first ask
   (a refusal is `Permanent`, and nothing is attempted); a source's own failure reaches the
   adapter as a `CredentialFailure`, which the adapter converts into its error with the class
   kept. A bearer token is built with `Credential::bearer(token)`, or `bearer_for(username,
   token)` where SASL needs the account. IMAP and SMTP origins use RFC 8314's service names as
   the scheme: `imaps`/`imap` (`STARTTLS`), `submissions`/`submission` (`STARTTLS`).
2. **The authorization-identity variant for IMAP** (`AUTHENTICATE PLAIN` with an `authzid`), for
   transfer hosts. It is a new request shape, so the live Dovecot master-user test comes first and
   the variant ships only once that passes. *Landed.* As built: `Credential::ActingAs` (`Debug`
   shows only that the administrator and the account are present); one RFC 4616 builder,
   `provider_imap::sasl::plain_response`, which SMTP `AUTH PLAIN` uses too; IMAP signs in with
   `AUTHENTICATE PLAIN` (`SASL-IR` where advertised, the `+` prompt otherwise), refusals classed
   as for `LOGIN`, and a plain password keeps `LOGIN`. SMTP refuses the variant before dialling
   (`Permanent`): submitting as another account is a different grant, and no server has been
   seen to take it. The pool check from "Direct host" landed with it, per account: the first dial
   that signs in fixes the account its connections belong to
   (`Credential::authorization_identity`: for this variant, the one acted as), and a later
   credential naming another fails its dial as `InvalidState` before anything is opened. A
   replacement must name the account the credential it replaces named, and the comparison is
   exact. Proven live on both Dovecot services with a master user
   (`provider-imap/tests/live_imap_acting_as.rs`); Stalwart was found to ignore the authorization
   identity (its impersonation takes `target%admin` as the username, which is a `Password`), and
   that is pinned with its control. See "Types" for the hazard this leaves to the host.
3. **JMAP**, together with `engine-http::send_authenticated` and the plaintext rule. This is the
   first per-request HTTP ask, and it deletes `RefreshingJmapProvider`. *Landed.* As built, the
   funnel is a client rather than a function: `AuthenticatedClient::new(tls, user_agent,
   configured, source, retry)` builds its own `reqwest` client for the account's configured
   origin, one that follows no redirect and, for an account on plaintext loopback, sees no
   proxy; `request`/`get`/`post` build requests, and `send(request, exchange, present, judge)`
   sends each on that client whatever client built it. So neither rule depends on an adapter
   building its client right: a function taking any request would have trusted the caller's
   redirect and proxy settings. The design's `build`
   is split in two: the client keeps the unauthenticated request and clones it for each
   presentation, and `present` only adds the credential (and may refuse a kind it cannot send,
   which ends the request before anything goes out: JMAP refuses `ActingAs`, since HTTP has no
   authorization identity). The design's `is_refusal` became `judge`, which reads only a `401`
   and answers `Unauthorized::{Refused, Reframe, Final}`: `Reframe` is JMAP's Basic/Bearer
   re-framing, which the funnel performs once per credential with the same credential, so
   re-framing and replacement cannot both answer one `401` and an adapter cannot forget the
   order. The ask runs under `Deadlines::reply`, the bound on an answer owed (nothing has been
   sent, so a submission's ask is not ambiguous either). Before `replace` is asked, up to 64 KiB
   of the refused reply's body is read, for no longer than five seconds, so the connection is
   free for the replay; the read is best effort, so a body cut off, too large or trickling
   leaves a refusal and never a failed exchange (a `401` says the request was not processed,
   so a submission refused is never "may have been sent"). The funnel is built on
   `engine_provider::with_credential`, so `permits`, the single replacement and the source's
   classes are the IMAP path's; the plaintext rule fails as `Permanent` through
   `CredentialFailure::refused_origin`. Every JMAP request goes through it: discovery and each
   redirect hop, method calls, blob download and upload, and the push stream, so each origin
   is checked as it is used. `JmapConfig::from_credential_source` is new; `JmapConfig::new`
   wraps its `Credentials` in `FixedCredential::unique`. Proven live on Stalwart
   (`provider-jmap/tests/live_credential_source.rs`): a wrong password replaced (Stalwart's
   `401` offers `Bearer` and `Basic` in two headers, so the password is refused as it stands
   and not re-framed), and a revoked bearer token replaced by a fresh one, both minted through
   Stalwart's own OAuth endpoints on the existing harness (RFC 7591 registration and an
   authorization code with PKCE); no second server was needed for JMAP. The sync host's
   `RefreshingJmapProvider` can now go; that is its change to make. The Basic header is still built by
   reqwest's `basic_auth` (marked sensitive, not zeroed), so its `username:password` input is
   one of the copies "Security review" places out of reach.
4. **CalDAV/CardDAV**: `permits` absorbs `CredentialHosts`, and the scope is keyed by subject.
   *Landed.* As built: `DavClient` holds an `AuthenticatedClient` built for the configured
   origin, and every request goes out on its client. CalDAV keeps its challenge-first rule
   (`CredentialScope`): a request to an origin that has not asked goes without the credential,
   and a `401` from a trusted origin is answered through `AuthenticatedClient::send`, whose
   `present` reads the credential's scheme against the challenge already received; an origin
   that has asked gets every later request through `send` directly. A challenge no credential
   HTTP carries can answer (only `Digest`, `Negotiate` and the like) is
   `UnsupportedAuthentication` before the source is asked, so a broker that is down cannot turn
   it into a retry; one only for the other kind's scheme (Basic, to a token) is the same verdict
   once the source has answered, with nothing sent. The configured server itself is trusted only
   once admitted, like every other origin. Either way a `401` to a request that
   carried the credential, offering its scheme or none (YAHOO-1), is `Refused`, and anything else
   `Final`. Discovery trusts an origin only when `AuthenticatedClient::admits` and the deprecated
   `CredentialHosts` both let it through, and it asks that of its start before anything is sent,
   so a configured server the credential may not reach is never dialled; the old "no plaintext
   after TLS" rule is the plaintext rule's special case. The scope belongs to one connection and
   the connection to one source for its life, so it serves one subject; the endpoints of step 6
   will key it by subject explicitly. Nextcloud Talk (`TalkMeetings`) sends on the same
   client, so no HTTP path in the crate holds a credential. `CalDavConfig`, `CardDavConfig` and
   `TalkMeetings` gain `from_credential_source`; their `new` wraps `provider_caldav::Credentials`
   in `FixedCredential::unique`. An administrator acting as another account is refused before
   it is sent (`Permanent`), as on JMAP. The `WWW-Authenticate` reading JMAP and CalDAV each
   had is now shared in `engine_http`: `challenge_schemes` and `offered_schemes` read the
   headers (lossily, so a realm that is not ASCII no longer hides a scheme from JMAP),
   `asks_for` is the rule for "this `401` asks for that scheme, or names none", and
   `credential_scheme` is the scheme each kind of credential goes under. Proven live
   (`provider-caldav/tests/live_credential_source.rs`) on Stalwart, SabreDAV and Nextcloud: a
   wrong password at connect replaced; a password that stops working after connect, on an origin
   that already answered, refused as `Authentication` without a replacement and replaced with
   one; CardDAV likewise on Stalwart and Nextcloud (the SabreDAV harness serves no address
   books); `permits` limited to the harness host discovering in full, and permitting nothing
   failing the connect as `Permanent` without an ask; a Stalwart OAuth token, revoked, refused
   and replaced by a fresh one (minted with Stalwart's `calendars` scope: a `mail` token passes
   discovery and is refused `403` at the calendar home); and Talk on Nextcloud replacing a wrong
   password. With the replacement disabled, every one of those but the `permits` test fails.
5. **Graph and Google API**: the same bearer funnel. Google's IMAP and API halves share one source.
   This deletes the token halves of the Graph and Gmail wrappers. Graph application permissions
   and Google domain-wide delegation need nothing beyond this slice. *Landed.* As built: both
   transports send every authenticated request on an `AuthenticatedClient` built for the
   client's base (`connect_with_source`, `with_base_and_source`; `connect`, `for_mailbox` and
   `with_base` wrap a fixed bearer under a unique subject), and keep a plain client for content
   URLs, which never ask. The presentation and the reading of a `401` CalDAV had are now
   `engine_http`'s, shared: `present_credential` (Basic for a password, Bearer for a token),
   `present_bearer` (for an API that signs in with a token alone: a password is refused as
   `Permanent` before anything is sent, through `CredentialFailure::unpresentable`) and
   `judge_by_scheme`. Graph's judge departs in one case: a `401` with no challenge to the
   directory's delta (`/users/delta`) is final, because Graph answers the directory of a
   personal account that way with a good token (MICROSOFT-2 in `spec-deviations.md`); without
   it every pass over that source would cost a token refresh. Only that read was observed, so
   every other Graph request keeps the shared rule, and a token refused without a challenge
   elsewhere would still be replaced. Google's refusal carries a
   `Bearer` challenge, so the shared rule stands. `GraphClient` and `GoogleClient` are cheap
   clones over one shared transport, so `GraphProvider::new` keeps its signature and a host
   builds one client per account rather than one per folder; `GraphClient::with_principal` is
   public, for a token with application permissions to reach any mailbox. The Google binding
   is checked rather than documented: `GmailProvider::with_imap` compares the IMAP reader's
   subject (`GmailMail::credential_subject`, which `ImapAccount` records from its config's
   source) with the client's, and refuses a mismatch as `InvalidState`, so an account cannot
   end up with another account's reader, or with two fixed credentials under subjects of their
   own. It proves one subject, not one source object: two sources naming the same subject are
   the same account's by the subject contract, and a host passes one source to both so that a
   replacement serves both. The source
   answers the token with the account's address (`Credential::bearer_for`), which IMAP's SASL
   needs and HTTP ignores. Proven live, read-only (`provider-graph/tests/live_credential_source.rs`,
   `provider-google/tests/live_credential_source.rs`): on a personal Microsoft account and a
   work account, a garbage token refused with Graph's `Bearer` challenge, replaced, and the
   folders listed, with a control whose replacement is garbage too ending `Authentication`
   after one replacement; on the personal account the directory's challenge-less `401` asking
   for no replacement, and the same request with a garbage token asking for one; on the Google
   test account the labels listed after a replacement, the same control, and the shared
   source in both orders, the IMAP half's XOAUTH2 refusal replacing the token the API half
   then presents without a refusal and the reverse, and an IMAP half signing in as another
   subject refused. With the replacement disabled, each of those but the challenge-less `401` and the
   binding fails; with Graph's departure removed, the challenge-less test fails; with the
   subject check removed, the binding test fails.
6. **Endpoints and `bind` for the direct host**: shared HTTP clients, per-subject caches, the IMAP
   pool registry with a process ceiling, and the eviction defaults with their host overrides.

Then, in the order of `north-star.md` ("Direction"): the query seam, `engine-direct` over the
endpoints, and the capability-trait split, which can assume one credential seam.

## Test plan

Offline (each new check is proven by reverting the fix and watching it fail):

- `engine-provider`: `Secret` and `Credential` `Debug` never contain the secret or the address;
  `Secret` is `ZeroizeOnDrop` (a trait-bound assertion). The replacement helper asks once, replaces
  at most once, returns the second refusal as it came, passes source error classes through, and a
  fixed credential never replaces. A source that never answers fails the ask as `Retryable` once
  the deadline passes (paused clock). IMAP and SMTP asks name their protocol; HTTP asks name
  `Http`.
- IMAP/SMTP: `dial_renewal_tests.rs` and `filing_smtp_server_tests.rs` carry over unchanged in
  behaviour.
- HTTP adapters, where the fakes **assert the request** rather than answer anything they are sent:
  request 1 carries token A, the reply is `401`, the replay carries token B, and the source sees
  exactly one ask and one replace. No replace happens for a `403`, for a `401` to an anonymous
  CalDAV request, after a transport failure, or for a body that cannot be cloned. A refused origin
  receives zero requests. An `http://` origin that is not loopback receives zero requests and
  fails as `Permanent`, whether configured, discovered or session-advertised; one on `127.0.0.1`
  still works. Content URLs never carry `Authorization` (the existing tests, kept).
- Across tenants: two subjects on one endpoint, where every request's `Authorization` belongs to
  its own subject. The IMAP registry never hands subject A's socket to B (`MockStream` `LOGIN`
  transcripts). A username change inside one pool is refused. Keying the cache by origin alone
  must turn these tests red. The eviction defaults evict, and a host override replaces them.

Live (`AGENTS.md`: any new request shape is run against a real server):

- **IMAP**: Dovecot (its static passdb lets a test source present a wrong password, then the right
  one through `replace`, which proves the re-dial and the second `LOGIN`) and Stalwart.
  `live_imap_oauth.rs` keeps covering both SASL token mechanisms (throwaway Gmail account).
- **IMAP authorization identity**: Dovecot with a master user configured. The administrator signs
  in as a test user with `AUTHENTICATE PLAIN` and an `authzid`, and the session selects that
  user's mailbox, not the administrator's. A wrong `authzid` is refused. This runs before the
  variant ships ("Phasing", step 2). As run: the harness's static passdb knows every name, so a
  "wrong" `authzid` is one the signing-in account may not act as (an account that is not a master
  user), beside a wrong master password; both are `NO [AUTHENTICATIONFAILED]`. An `authzid`
  naming no account cannot be shown on this harness.
- **SMTP**: Stalwart submission on 465 and 587, wrong then right.
- **JMAP**: Stalwart, wrong Basic then right. This proves the replayed request and that the `401`
  offered `Basic` (re-framing and replacement must not both fire). As run: Stalwart's `401`
  carries two challenges, `Bearer …` and then `Basic realm="Stalwart Server"`, in separate
  headers; a tool that prints only the first header shows `Bearer` alone.
- **Bearer replacement on JMAP and CalDAV**: Stalwart's own OAuth endpoint mints the tokens. The
  sync host's repository runs a Stalwart OAuth harness; the engine reuses or ports that approach,
  presents an expired or revoked token first, and succeeds with the token `replace` mints. As run
  for JMAP: the engine's existing harness serves the OAuth endpoints on its loopback port, so
  the live suite registers a public client, signs in through Stalwart's login form with an
  authorization code and PKCE (`provider-jmap/tests/stalwart_oauth/`), revokes one token and
  presents it, and succeeds with a second. A second Stalwart is needed only where an issuer must
  be reachable by discovery, which presenting a token does not need.
- **CalDAV/CardDAV**: Stalwart, SabreDAV and Nextcloud, wrong then right on an origin already
  authenticated after discovery, plus a full discovery with `permits` limited to the harness host,
  showing nothing regresses. As run: see "Phasing", step 4.
- **Graph and Google**: these cannot run in a local harness. They are verified with the test
  Microsoft account and the throwaway Google account, with tokens from the `tools/*-oauth` token
  tools (`tools/graph-oauth`, `tools/google-oauth`): present a garbage or revoked token first,
  observe the `401`, and succeed through `replace`. Each `401` envelope is captured and scrubbed
  (the existing `UNAUTHORIZED` fixtures already have the right shape). If the owner cannot run
  them, the PR names them as unverified. As run: see "Phasing", step 5; the challenges are
  `error/unauthorized_challenge.txt` in each crate's fixtures. A token with Graph application
  permissions was not run: it is a bearer token like any other, on the same path.

## Decisions (2026-11-23)

The owner approved each of these. The body states them where they apply; this list is the
record.

1. **`Credential`'s `Debug` prints no username**, only that one is present, for privacy on
   multi-tenant hosts ("Types").
2. **The subject is its own `CredentialSubject` newtype, not `AccountId`.** A store-less host may
   reuse account ids across tenants ("Types", "Security review").
3. **An ask to the source runs under the dial deadline (or the request's, on HTTP), and a timeout
   is `Retryable`** ("The trait", "Each ask has a deadline").
4. **The failure class is enough** to tell "the source failed" (broker down: `Retryable`; consent
   gone: `Authentication`) from "the server refused". No extra variant ("The trait", "Error
   classes").
5. **The engine refuses credentials over plaintext HTTP except to loopback origins.** It is an
   engine rule, not left to `permits`. Narrowed when step 3 landed: only for an account whose
   configured origin is itself plaintext loopback ("Host binding"). The harnesses all run on loopback, so nothing breaks
   ("Host binding", "Security review").
6. **Eviction of the per-subject caches and the IMAP pool registry has engine defaults, which the
   host can override** ("Direct host").
7. **`CredentialRequest` names the protocol (IMAP, SMTP or HTTP)** as well as the origin, for
   hosts with separate SMTP credentials ("Types").
8. **Transfer hosts get administrative credentials**, with per-user credentials as the fallback:
   an IMAP authorization identity (`AUTHENTICATE PLAIN` with an `authzid`, Dovecot master users),
   Graph application permissions, Google domain-wide delegation. `Credential` gains the variant
   the IMAP authorization identity needs, proven live on Dovecot before it ships; the other two
   are bearer tokens and need no new variant ("Types", "Phasing", "Test plan").
9. **Session-advertised JMAP origins stay trusted** (RFC 8620 §6.1, §6.2, §2; Fastmail's separate
   download origin), subject to the engine rules in "Host binding".
10. **New capabilities are reached through accessors on `Provider`.** Each is its own trait,
    reached through a `Provider` method that defaults to `None`, so a host holding
    `Box<dyn Provider>` reaches it without knowing the adapter. `Provider::mail_importer` (#375)
    is the first; `north-star.md` ("Key Decisions") records the rule as the first step of the
    capability-trait split, superseding the rule against sub-traits.

Two points were settled by tests and existing infrastructure rather than by the owner. Bearer
replacement runs live on JMAP and CalDAV against Stalwart's OAuth endpoint, reusing or porting the
mail-calendar app's Stalwart OAuth harness; Graph and Google run against test accounts with tokens
from the `tools/*-oauth` tools ("Test plan").
