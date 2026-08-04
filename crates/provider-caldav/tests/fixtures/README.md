# CalDAV/CardDAV response fixtures

Captured responses from the two harness servers, replayed through the fake
`DavExecutor` so the offline suite is driven by what a real server actually sent
rather than by what we expected it to send. Scrub anything identifying before
committing: these come from throwaway harness accounts only
(`alice@test.local`), never from a real mailbox.

| Fixture | Server | What it pins |
| --- | --- | --- |
| `principal.xml` | Stalwart | The lenient discovery shape — `calendar-home-set` returned directly at the well-known path. |
| `calendar-home.xml` | Stalwart | The calendar-home listing, and the writable `DAV:current-user-privilege-set`. |
| `calendar-home-sabredav.xml` | SabreDAV | The same listing with **two** answers to "what may I do here": Alice's own calendar and Bob's read-only share. |
| `sync-initial.xml` / `sync-noop.xml` | Stalwart | The RFC 6578 `sync-collection` snapshot, and the held-token empty delta. |
| `options-dav-stalwart.txt` | Stalwart | The `DAV:` response header of an `OPTIONS` on the calendar home — **with** `calendar-auto-schedule` (RFC 6638 §2). |
| `options-dav-sabredav.txt` | SabreDAV | The same header **without** it. |

## The two `OPTIONS` fixtures are the discriminating pair

`Capabilities::calendar_scheduling` is discovered, so it needs a server that
advertises RFC 6638 and one that does not. The `.txt` files hold one header value
each, verbatim, because that header *is* the whole response — an `OPTIONS` returns
no body.

The SabreDAV negative is a property of **this harness's configuration**, not of
SabreDAV: `docker/sabredav/server.php` loads `Sabre\CalDAV\Plugin` and deliberately
not `Sabre\CalDAV\Schedule\Plugin`, so it serves calendar *access* only. That is the
point — it is the plain-CalDAV deployment the capability exists to detect, and the
one where an inbound invitation arrives as mail and never reaches the calendar.
Loading the scheduling plugin there would delete the only negative case we have.
