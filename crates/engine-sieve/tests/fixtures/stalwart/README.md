# Scripts Stalwart wrote for its own reply

The `reply_*.sieve` files are the exact bytes Stalwart stores for a JMAP `VacationResponse`
switched on, one file per window shape: none, a start only, an end only, both. A host reading
scripts over ManageSieve tells the server's own reply script from a user's by this shape
(`dialect::scripts::plainly_server_reply`). They are kept as observed, CRLF included
(`.gitattributes` marks them `-text`).

## How they were made

On 2026-11-24, on the Stalwart harness (`docker/stalwart`, `stalwartlabs/stalwart:v1.0.2`): for
each shape, `VacationResponse/set` with `isEnabled: true`, subject `Away`, text body `I am away.`,
and `fromDate`/`toDate` as the shape says (`2030-01-02T09:00:00Z`, `2030-01-09T17:30:00Z`), then
ManageSieve `GETSCRIPT "vacation"`, the literal written as received. The reply was switched off and
the script deleted afterwards.
