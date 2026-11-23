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
servers that behave differently, and IMAP at `imap.mail.yahoo.com`, which advertises
`MESSAGELIMIT=1000`, `PARTIAL` and `UIDONLY` but neither CONDSTORE nor QRESYNC. Last observed
October 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| YAHOO-1 | CalDAV: answers a request without credentials with `401` and no `WWW-Authenticate` header. (The CardDAV server sends a `Basic` challenge.) | RFC 9110 §15.5.2: a `401` response MUST send a `WWW-Authenticate` header field containing at least one challenge. | A bare `401` from an origin the credential may go to is read as a request for the credential (`credential_scope.rs`). | Not yet |
| YAHOO-2 | CalDAV: answers a `sync-collection` REPORT with an empty `207` and no `sync-token`. The report is not in the collection's `supported-report-set`. | RFC 3253 §3.6 makes the report being supported a precondition (`DAV:supported-report`), and §1.6 says a failed precondition MUST be answered `403` or `409`. RFC 6578 §3.2: the response MUST be a multistatus that MUST contain one `DAV:sync-token`. | An answer without a sync-token is read as a server without the report, and the collection is synced through its CTag (`ctag_sync.rs`). | Not yet |
| YAHOO-3 | CardDAV: answers a `sync-collection` REPORT with `400` and an empty body. | RFC 3253 §3.6 and §1.6, as YAHOO-2: `403` or `409` for an unsupported report, not `400`. | A `400` to a collection's first `sync-collection` is read as a refusal of the report (`ctag_sync::refuses_sync_collection`). | Not yet |
| YAHOO-4 | CardDAV: answers every `addressbook-query` REPORT with `400`, with an empty filter or a property filter alike. | RFC 6352 §3: a CardDAV server MUST support all address book reports defined in Section 8, which includes `addressbook-query` (§8.6). | The CTag fallback lists members with a `PROPFIND` and fetches them with `addressbook-multiget`, never `addressbook-query` (`ctag_sync.rs`). | Not yet |
| YAHOO-5 | CardDAV: answers a `PROPFIND` of the principal asking only for `current-user-principal` with `400`; the same request asking for `addressbook-home-set` as well succeeds. | RFC 4918 §9.1: a `PROPFIND` for named properties is answered with each property's value or, for one the resource lacks, a `404` in its `propstat`; `current-user-principal` is defined by RFC 5397 §3. | Discovery asks for both properties in one request. | Not yet |
| YAHOO-6 | CardDAV: answers a valid `addressbook-multiget` with `500` when the request has no `User-Agent` header. | RFC 9110 §10.1.5 makes `User-Agent` a SHOULD for the client, never a condition of service; a `500` says the server failed a valid request (RFC 9110 §15.6.1). | Every request carries the host's `User-Agent`, which every HTTP provider's constructor requires (`engine_http::UserAgent`). | Not yet |
| YAHOO-7 | CalDAV: stores a `PUT` resource under a name of its own, derived from the event's `UID`, rather than at the request URI, which then answers `404`. | RFC 9110 §9.3.4: `PUT` requests that the state of the target resource be created or replaced, and a `201` says it was created; the target resource is the request URI. | A new event is named `{UID}.ics` already, the name Yahoo gives it, so the write reaches the resource Yahoo stores. The exception is a `UID` whose name would pass 255 bytes, named by its hash so Nextcloud can hold it (`href::resource_name`); on Yahoo that write lands under Yahoo's name and the minted one answers `404` until the next sync lists it. The next sync lists it under Yahoo's spelling (YAHOO-8), so the stored event changes key once. | Not yet |
| YAHOO-8 | CalDAV: lists that name in a multistatus `href` with the `%` of `%40` encoded again (`uid%2540example.ics`), and refuses a request to that href with `400` "Ambiguous double encoding". The resource answers at `%40`. | RFC 4918 §8.3: an `href` in a multistatus is a URI reference (`Simple-ref`) naming the resource. Decoded per RFC 3986, `%2540` names `uid%40example.ics`, a different resource from the one stored as `uid@example.ics`. | A `400` to a URL with a doubly encoded octet is asked once more with it encoded once (`href::single_encoded`, `DavClient::exchange`), for a read or a guarded write; an unguarded write is not, since on a server that meant the `%25` the once-encoded URL is another resource. Reads never needed it: a multiget names the href in its body, where Yahoo accepts it. | Not yet |
| YAHOO-9 | Both servers: perform a `DELETE` whose `If-Match` names an ETag the resource no longer has, and answer `204`. A `PUT` with the same stale `If-Match` is refused with `412`. | RFC 9110 §13.1.1: an origin server MUST NOT perform the requested method if a received `If-Match` condition evaluates to false. | Before every guarded delete, on every server, the resource's current ETag (or `schedule-tag`, for a delete guarded by one) is read, and a different one is answered `412` without sending the delete (`delete_guard.rs`). A change landing between the read and the delete still goes on such a server. | Not yet |
| YAHOO-10 | CalDAV: adds an `ORGANIZER` (the account itself, with `SENT-BY`) to an event created without one and with no attendees. | RFC 5545 §3.8.4.3: `ORGANIZER` MUST NOT be specified in a component that is not group-scheduled but only on a single user's calendar. | Read as stored: the account appears as the organiser of its own appointments. | Not yet |
| YAHOO-11 | CalDAV: stores the event with `STATUS:CONFIRMED` twice in one `VEVENT`. | RFC 5545 §3.6.1: `status` is among the event properties that MUST NOT occur more than once. | The parser reads the event; both values are the same. | Not yet |
| YAHOO-12 | IMAP: answers a `UID SEARCH SINCE` matching more messages than its `MESSAGELIMIT` with the 1000 newest by date and a plain `OK`, with no response code. On an inbox of 2578, `SINCE` 183 days, 365 days and ten years back each return exactly 1000. | RFC 9738 §3.1: when a `SEARCH` would operate on more messages than the limit, the server MUST return the `MESSAGELIMIT` response code, so the client can repeat the operation for the rest. | A search returning exactly as many matches as the limit is read as cut off, and the window is decided from each message's `INTERNALDATE` instead, fetched a limit at a time (`transport_search.rs`). | Not yet |
| YAHOO-13 | IMAP: ignores `UID SEARCH RETURN (PARTIAL 1:10)`, answering a plain `SEARCH` with every match up to the limit, and refuses any partial range not starting at 1 with `BAD [CLIENTBUG]` ("Valid partial range starts with 1 and is below MESSAGELIMIT"). It honours `PARTIAL` on `FETCH`. | RFC 9394 §3.1: the `PARTIAL` return option causes the server to provide the results in an `ESEARCH` response; §4 makes a range such as `1001:1010` valid, and the server returns the results that are in it. | A search is never paged. The `INTERNALDATE` reads of YAHOO-12 are paged with `PARTIAL` on `UID FETCH` (`transport_search.rs`). | Not yet |
| YAHOO-14 | IMAP: answers `ENABLE CONDSTORE` with `* ENABLED CONDSTORE`, though `CONDSTORE` is not advertised, and then refuses `UID FETCH … (CHANGEDSINCE 1)` with `NO [CANNOT]`. | RFC 7162 §3.1: CONDSTORE adds the `CHANGEDSINCE` `FETCH` modifier, and after a CONDSTORE enabling command the server MUST include mod-sequence data in all subsequent untagged `FETCH` responses. | CONDSTORE is only used through QRESYNC, which Yahoo does not advertise, so its folders are reconciled by reading every held message's flags (`resync.rs`). | Not yet |
| YAHOO-15 | IMAP: holding `IDLE` on a folder, reports changes only on a fixed two-minute cycle counted from the `IDLE` command, so a new message waits until the next tick: up to two minutes, about one on average, whoever sent it. Measured with plain `IDLE` on INBOX and a second connection finding each message by `SEARCH`: one from an outside sender landed at 14 s and was reported at 121.5 s; self-sent ones landed at 107.5 s, 126.9 s and 8 s and were reported at 121.6 s, 241.6 s and about 119 s. A `NOOP` reports a new message within one or two 20-second turns. It also ends a quiet `IDLE` with `* BYE` after about five minutes. | RFC 2177 §3: while idling, the server sends untagged `EXISTS` and `EXPUNGE` responses as the mailbox changes, so that a client need not poll. | Not worked around: the watch idles as on any server, so new mail shows at Yahoo's next tick, and the host reconnects after each five-minute `BYE`. Polling with `NOOP` between short turns of `IDLE` would show it within about half a minute, at the cost of a command every few seconds per watched folder per user, which is the load `IDLE` exists to spare a server; it was tried and withdrawn. | Not yet |

## Google

Google Calendar's CalDAV endpoint. Last observed October 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| GOOGLE-1 | Answers a request without credentials with `401` and no `WWW-Authenticate` header. | RFC 9110 §15.5.2, as YAHOO-1. | As YAHOO-1. | Not yet |
| GOOGLE-2 | **Unsurveyed**: whether an import over CalDAV stays silent. No credential for the Google test account was available when calendar import landed, so it is not known whether the endpoint advertises `calendar-auto-schedule`, lists a scheduling inbox or outbox, or honours `SCHEDULE-AGENT=CLIENT`. | RFC 6638 §2 and §7.1, as above. | Not singled out: the engine does not tell a Google account apart by host. The import accessor's rule decides on what the principal says (`None` where scheduling collections exist unadvertised); a live survey is to settle it before a host imports into Google over CalDAV. | n/a |

## AOL

AOL's CalDAV endpoint. Last observed October 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| AOL-1 | Answers a request without credentials with `401` and no `WWW-Authenticate` header. | RFC 9110 §15.5.2, as YAHOO-1. | As YAHOO-1. | Not yet |

## Microsoft

Exchange Online, through Microsoft Graph, on a personal Outlook.com account. Last observed
November 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| MICROSOFT-1 | Reads a message's importance header as unreadable, and so the message as normal importance, when the token has whitespace after it (`Importance: low␠␠`) or a comment beside it (`Importance: High (urgent)`). Whitespace before the token, a fold before it, or none at all reads as expected. Observed by creating drafts from MIME and reading `importance` back (`provider-graph/tests/fixtures/mail/importance_header_shapes.json`). | RFC 2156 §5.3 defines `Importance` as a structured field whose value is a token, and RFC 5322 §3.2.2 lets CFWS, folding whitespace and comments, appear around the tokens of a structured field. | The engine's rule follows the RFC (`Importance::parse`), so such a message reads its stated importance on every other transport. On a Graph account Exchange's reading stands, since the engine reads the property Exchange derived; `importance_shapes_tests.rs` pins exactly these shapes as the difference. | Not yet |

## Stalwart

Stalwart's JMAP server, as the engine's harness runs it (`docker/stalwart`). Last observed on
v1.0.2, November 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| STALWART-1 | JMAP: lists the same `accountCapabilities` (mail, calendars, contacts and more) on every account shared with the user, whatever was shared. An account shared for one INBOX answers `Calendar/get` with `accountNotSupportedByMethod`; one shared for one calendar answers `Mailbox/get` the same way. Pinned in `stalwart-harness/tests/smoke_shared_stores.rs`. | RFC 8620 §1.6.2: `accountCapabilities` is "the set of capability URIs for the methods supported in this account", and §3.6.2 defines `accountNotSupportedByMethod` as the error for an account that "does not support this method". | The shared-account list asks each claimed domain with a `get` for no objects, in one request, and lists a domain only where it answers (`provider-jmap/src/shared.rs`). A client bound to such an account still advertises the overclaimed domains; a host dials only those the list named. | Not yet |
| STALWART-2 | CalDAV and CardDAV: refuses an object it cannot store with `412` naming the precondition (`valid-calendar-object-resource` for an iCalendar object with no component or one that is not iCalendar, `supported-address-data` for a body that is not a vCard). A `PUT` carrying `If-None-Match: *` then reads like one that found its target taken. Observed on v1.0.2, November 2026 (`provider-caldav/tests/fixtures/import/stalwart-put-invalid.xml`). | RFC 3253 §1.6: a request that fails a precondition MUST be answered `403` if repeating it would not succeed, or `409` if it might after a change; RFC 4791 §5.3.2.1 and RFC 6352 §6.3.2.1 name these preconditions under that rule. | A `412` whose body names a CalDAV or CardDAV object precondition is the object's refusal (`Permanent`) on every write path, and only a bare one is a precondition conflict (`error.rs`, `status_class` and `write_refusal`). | Not yet |
| STALWART-3 | JMAP Sieve: destroys the script that runs the `VacationResponse` (named `vacation`) on `SieveScript/set` `destroy`, once it is inactive, answering `destroyed`. Updating its content is refused `forbidden` as required. Observed 2026-11-23 as a scratch account. | RFC 9661 §4: the server MUST NOT allow the VacationResponse Sieve script to be destroyed or have its content updated by the SieveScript/set method, and any such request MUST be rejected with a `forbidden` SetError. | The engine never destroys a script it did not create; the next `VacationResponse/set` makes a new one, so nothing is lost. The live suite `provider-jmap/tests/live_vacation.rs` relies on it only to leave the scratch account as it found it. Slice 2's rules writer must not depend on it either way (`docs/design/filters-and-out-of-office.md`). | Not yet |
| STALWART-4 | JMAP: runs a `VacationResponse` whose `toDate` is set as a Sieve script testing `currentdate :value "le" "iso8601" "<toDate>"`, so a message arriving at `toDate` itself, to the second, still gets the reply. Its `currentdate` runs in UTC (a probe filed a delivered message by zone under `+0000`), so the comparison is otherwise exact. Observed 2026-11-23 as a scratch account, from the script's bytes. | RFC 8621 §8, `toDate`: "If `isEnabled` is true, messages that arrive before this date-time (but on or after the `fromDate` if defined) should receive the user's vacation response." The end is exclusive. | The neutral window keeps RFC 8621's exclusive end (`VacationResponse::replies_at`), and the adapter writes `toDate` as given, so on Stalwart a reply runs one second longer than the window says. A renderer of its own (ManageSieve, slice 3) writes `"lt"`. | Not yet |
| STALWART-NEXT | JMAP Sieve: refuses an invalid script with the SetError type `invalidScript`, in `SieveScript/set`'s `notCreated`/`notUpdated` and in `SieveScript/validate`'s `error`. Its description quotes the script. Observed 2026-11-23 as a scratch account (an unterminated regular expression; an extension used without `require`). | RFC 9661 §2.4: a script that violates the Sieve grammar or requires an unsupported extension is rejected with the `invalidSieve` SetError; §2.6 answers `validate` with the same. | Both names are the same permanent refusal, `item_unavailable`, naming the type and never the description (`provider-jmap/src/rules.rs`, `refused_script`). | Not yet |
| STALWART-NEXT | JMAP Sieve: `SieveScript/validate` answers `blobNotFound` for a blob uploaded by `Blob/upload` earlier in the same request, named by its creation id (`#b1`) or by a result reference to it, though `SieveScript/set` in the same position resolves `#b1`. Observed 2026-11-23 as a scratch account. | RFC 8620 §5.3 and RFC 9404 §4: a creation id may be used in place of an id in a later method call of the same request; RFC 8620 §3.7: a result reference may stand for any argument. | The engine does not validate before storing: `SieveScript/set` refuses an invalid script whole (nothing stored, nothing activated, the state unmoved; proven live in `live_rules.rs`), so validate adds a round trip and no safety. The live suite validates in a request of its own. | Not yet |
| STALWART-NEXT | JMAP: ignores a result reference for `ifInState` (`#ifInState` naming an earlier call's `/newState`), neither resolving it nor answering `invalidResultReference`: the call runs unguarded, even when the call it names failed. Observed 2026-11-23 as a scratch account. | RFC 8620 §3.7: a result reference may stand for any argument, and when it cannot be resolved the method MUST be answered `invalidResultReference`. | Two dependent writes (the reply's words, then the rules script's include of it) go in two requests, the second guarded by the `newState` the first answered (`provider-jmap/src/vacation_include.rs`). | Not yet |
| STALWART-NEXT | JMAP Sieve: accepts a `require` naming an extension it does not know (`nonexistent-ext`, `processcalendar`, `vnd.dovecot.pipe`) and refuses only the commands that would use it; a known extension it does not support (`imapsieve`) is refused at the `require`. Observed 2026-11-23 as a scratch account, with `SieveScript/validate`. | RFC 5228 §3.2: if an extension named in `require` is not supported, the script MUST fail to compile (RFC 9661 §2.4: `invalidSieve`). | Nothing the engine writes depends on it: the dialect requires only extensions the account lists. The live proof that the server enforces its list uses `imapsieve`, the one probe that can fail (`live_rules.rs`). | Not yet |

## SabreDAV

SabreDAV's stock CalDAV server with its PDO backend, as the engine's fixture runs it
(`docker/sabredav`, 4.7.0). Deployments that build on that backend share what it does; Nextcloud,
which replaces the backend, does not. Last observed November 2026.

| Id | What the server does | What the RFC requires | How the engine copes | Reported |
|---|---|---|---|---|
| SABREDAV-1 | Stores a second calendar object whose `UID` an object in the same calendar already has, answering `201`, so the calendar shows the event twice. Observed with and without `If-None-Match: *` (`docs/design/calendar-and-contact-import.md`, "Survey"). | RFC 4791 §5.3.2.1 makes `CALDAV:no-uid-conflict` a precondition of a `PUT`: the `UID` MUST NOT be in use by another object in the collection. | An import looks the `UID` up with a `calendar-query` before every `PUT`, on every server, since nothing a server says tells this one apart (`calendar_import.rs`). Stalwart and Nextcloud refuse the second object and name the first, which the importer reads too. | Not yet |

## Resolved

None yet.

## Permitted, and easy to get wrong

Not departures: each is behaviour the RFC allows, listed because a client that assumes the common
case breaks on it.

| Server | What it does | Why it is allowed | How the engine copes |
|---|---|---|---|
| iCloud | Answers `sync-collection` with each changed member's `getetag` only, never its `calendar-data` or `address-data`. | RFC 6578 §3.2 lets a server return fewer properties than were asked for. | The bodies are fetched with a multiget (`multiget::fill_bodies`). |
| iCloud | Serves the calendar and contacts homes on numbered hosts (`pNN-caldav.icloud.com`), not on the host discovery started at. | RFC 4791 §6.2.1 and RFC 6352 §7.1.1 name the home by URL, which may be on any host. | The credential follows to a home's origin only once it asks (`credential_scope.rs`). |
| Stalwart (DAV) | Refuses a request over the account's concurrency cap (4 in flight, its JMAP `maxConcurrentRequests`) with `400` and RFC 8620's `urn:ietf:params:jmap:error:limit` problem details, not `429`. Observed on v0.16.24 and v1.0.2, November 2026. | RFC 9110 §15.5.1: `400` covers any error the server perceives as the client's. RFC 6585 §4 defines `429` for too many requests but requires no server to use it. | The body is read and the refusal waited out like a `429` (`provider-caldav/src/throttle.rs`); a refused write was measured as not applied, so the replay is safe. |
| Fastmail | Sends the `DAV` response header as several field lines (two on the address-book home, four on the calendar home), with `addressbook` and `calendar-auto-schedule` after the first. | RFC 9110 §5.3: repeated field lines are one comma-separated list. | All lines are read (`dav_client.rs`). |
| Any CalDAV server | Advertises no `calendar-auto-schedule` and lists no scheduling inbox or outbox: it does no scheduling. One that lists them without advertising scheduling may schedule without saying so. | RFC 6638 §2 has a scheduling server advertise the token; one that does not is not bound by §7.1 to honour `SCHEDULE-AGENT=CLIENT`. | `CalDavProvider::calendar_importer` is `Some` where scheduling is advertised or nothing can be sent, `None` where scheduling collections exist unadvertised or the principal was not read (`docs/design/calendar-and-contact-import.md`, "Where silence is promised"). |
| Yahoo (CalDAV) | Rewrites the event it stores: `Europe/Amsterdam` becomes `Europe/Brussels` (zones that share today's rules but not their history), a display alarm's text is emptied, `PRODID` is replaced and `X-YAHOO-` properties are added. So a `PUT` returns no `ETag`. | RFC 4791 does not require a server to store the event as sent; §5.3.4 then forbids a strong `ETag` in the response. | The write receipt carries no ETag and the next sync supplies one. |
