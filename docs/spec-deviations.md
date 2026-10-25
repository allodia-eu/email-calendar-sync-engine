# Where providers depart from the specifications

The authoritative record of every place a server this engine talks to departs from the RFC that
governs it, how the engine copes, and whether the provider has been told. Code that exists because
of one of these entries says so and links here, so a workaround can be found from its cause and
removed once the provider fixes it.

An entry is a departure we observed against the provider's production server, with the date we
last saw it. Behaviour an RFC permits but that clients commonly get wrong is listed separately, at
the end, because the engine has to handle it just the same and nobody needs to be told.

Each entry names the requirement it breaks, quoted as the RFC words it (MUST, SHOULD, or a defined
response), so the claim can be checked without trusting this page.

## How to add or update an entry

- Reproduce it against the real server first, with `dav-cli raw` or the request the engine sends.
  An entry is never written from documentation or hearsay.
- Give it the next id for that provider (`YAHOO-6`), and keep ids stable: code comments cite them.
- Fill in **Reported** when the provider is told: the date and the channel (support ticket,
  developer forum, security contact), never a person's name. Record their answer, and when a fix
  lands, move the entry to **Resolved** with the date it stopped reproducing, and remove the
  workaround it names.

## Yahoo

Yahoo serves CalDAV at `caldav.calendar.yahoo.com` and CardDAV at `carddav.address.yahoo.com`, two
servers that behave differently. Last observed October 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| YAHOO-1 | CalDAV: answers a request without credentials with `401` and no `WWW-Authenticate` header. (The CardDAV server sends a `Basic` challenge.) | RFC 9110 §15.5.2: a `401` response MUST send a `WWW-Authenticate` header field containing at least one challenge. | A bare `401` from an origin the credential may go to is read as a request for the credential (`credential_scope.rs`). | Not yet |
| YAHOO-2 | CalDAV: answers a `sync-collection` REPORT with an empty `207` and no `sync-token`. The report is not in the collection's `supported-report-set`. | RFC 3253 §3.6 makes the report being supported a precondition (`DAV:supported-report`), and §1.6 says a failed precondition MUST be answered `403` or `409`. RFC 6578 §3.2: the response MUST be a multistatus that MUST contain one `DAV:sync-token`. | An answer without a sync-token is read as a server without the report, and the collection is synced through its CTag (`ctag_sync.rs`). | Not yet |
| YAHOO-3 | CardDAV: answers a `sync-collection` REPORT with `400` and an empty body. | RFC 3253 §3.6 and §1.6, as YAHOO-2: `403` or `409` for an unsupported report, not `400`. | A `400` to a collection's first `sync-collection` is read as a refusal of the report (`ctag_sync::refuses_sync_collection`). | Not yet |
| YAHOO-4 | CardDAV: answers every `addressbook-query` REPORT with `400`, with an empty filter or a property filter alike. | RFC 6352 §3: a CardDAV server MUST support all address book reports defined in Section 8, which includes `addressbook-query` (§8.6). | The CTag fallback lists members with a `PROPFIND` and fetches them with `addressbook-multiget`, never `addressbook-query` (`ctag_sync.rs`). | Not yet |
| YAHOO-5 | CardDAV: answers a `PROPFIND` of the principal asking only for `current-user-principal` with `400`; the same request asking for `addressbook-home-set` as well succeeds. | RFC 4918 §9.1: a `PROPFIND` for named properties is answered with each property's value or, for one the resource lacks, a `404` in its `propstat`; `current-user-principal` is defined by RFC 5397 §3. | Discovery asks for both properties in one request. | Not yet |
| YAHOO-6 | CardDAV: answers a valid `addressbook-multiget` with `500` when the request has no `User-Agent` header. | RFC 9110 §10.1.5 makes `User-Agent` a SHOULD for the client, never a condition of service; a `500` says the server failed a valid request (RFC 9110 §15.6.1). | Every request carries a `User-Agent` (`engine_http::client`). | Not yet |
| YAHOO-7 | CalDAV: stores a `PUT` resource under a name of its own, derived from the event's `UID`, rather than at the request URI, which then answers `404`. | RFC 9110 §9.3.4: `PUT` requests that the state of the target resource be created or replaced, and a `201` says it was created; the target resource is the request URI. | Not handled: a created event is found under the new name only by the next sync. | Not yet |
| YAHOO-8 | CalDAV: lists that name in a multistatus `href` with the `%` of `%40` encoded again (`uid%2540example.ics`), and refuses a request to that href with `400` "Ambiguous double encoding". The resource answers at `%40`. | RFC 4918 §8.3: an `href` in a multistatus is a URI reference (`Simple-ref`) naming the resource. Decoded per RFC 3986, `%2540` names `uid%40example.ics`, a different resource from the one stored as `uid@example.ics`. | Not handled: an update or delete of such an event fails. Reads work, because a multiget names the href in its body. | Not yet |

## Google

Google Calendar's CalDAV endpoint. Last observed October 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| GOOGLE-1 | Answers a request without credentials with `401` and no `WWW-Authenticate` header. | RFC 9110 §15.5.2, as YAHOO-1. | As YAHOO-1. | Not yet |

## AOL

AOL's CalDAV endpoint. Last observed October 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| AOL-1 | Answers a request without credentials with `401` and no `WWW-Authenticate` header. | RFC 9110 §15.5.2, as YAHOO-1. | As YAHOO-1. | Not yet |

## Resolved

None yet.

## Permitted, and easy to get wrong

Not departures: each is behaviour the RFC allows, listed because a client that assumes the common
case breaks on it.

| Server | What it does | Why it is allowed | How the engine copes |
|---|---|---|---|
| iCloud | Answers `sync-collection` with each changed member's `getetag` only, never its `calendar-data` or `address-data`. | RFC 6578 §3.2 lets a server return fewer properties than were asked for. | The bodies are fetched with a multiget (`multiget::fill_bodies`). |
| iCloud | Serves the calendar and contacts homes on numbered hosts (`pNN-caldav.icloud.com`), not on the host discovery started at. | RFC 4791 §6.2.1 and RFC 6352 §7.1.1 name the home by URL, which may be on any host. | The credential follows to a home's origin only once it asks (`credential_scope.rs`). |
| Fastmail | Sends the `DAV` response header as several field lines (two on the address-book home, four on the calendar home), with `addressbook` and `calendar-auto-schedule` after the first. | RFC 9110 §5.3: repeated field lines are one comma-separated list. | All lines are read (`dav_client.rs`). |
| Yahoo (CalDAV) | Returns no `ETag` on a `PUT` that creates an event. | RFC 4791 §5.3.4: when the stored resource differs from the request body, a strong `ETag` MUST NOT be returned, and Yahoo rewrites what it stores. | The write receipt carries no ETag, and the next sync supplies one. |
