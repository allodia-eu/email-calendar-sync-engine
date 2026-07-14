#!/bin/sh
# PID 1 for the SabreDAV harness container: initialize the SQLite database from
# SabreDAV's own schema, seed the principals and the read-only shared calendar,
# start the PHP built-in server, seed the calendar over CalDAV, write a readiness
# marker, then run the server in the foreground. Re-running against an existing data
# volume skips init and re-seeds idempotently (a clean volume re-initializes).
set -eu

cd /app
DB="data/db.sqlite"
SCHEMA="sql"

# The calendar Bob owns and shares with Alice read-only. A fixed id (SabreDAV
# autoincrements from 1, so this cannot collide with the `default` collection
# MKCALENDAR creates in seed.sh) keeps the two INSERTs below referentially stable
# without a last_insert_rowid() dance.
SHARED_CAL_ID=100

if [ ! -f "$DB" ]; then
    mkdir -p data
    # SabreDAV's canonical schema, vendored under sql/ (its composer dist omits
    # examples/). Load exactly the tables we use.
    cat "$SCHEMA/sqlite.principals.sql" \
        "$SCHEMA/sqlite.calendars.sql" \
        "$SCHEMA/sqlite.propertystorage.sql" \
        "$SCHEMA/sqlite.locks.sql" | sqlite3 "$DB"
    # Two principals: the harness account (its calendars hang off it), and Bob, who
    # owns the calendar Alice sees read-only.
    sqlite3 "$DB" "INSERT INTO principals (uri, email, displayname) \
        VALUES ('principals/${HARNESS_USER}', '${HARNESS_USER}', 'Alice Tester');"
    sqlite3 "$DB" "INSERT INTO principals (uri, email, displayname) \
        VALUES ('principals/bob@test.local', 'bob@test.local', 'Bob Tester');"

    # A calendar Bob owns (access 1) and shares with Alice **read-only** (access 2 —
    # see the `calendarinstances.access` comment in sql/sqlite.calendars.sql). It is
    # the fixture for `DAV:current-user-privilege-set`: SabreDAV computes Alice's
    # privileges on it as read + write-properties (SharedCalendar::getACL), i.e. WITH
    # NO `DAV:write`/`DAV:write-content`, so a client that asks gets the truth — a
    # collection it must not offer to write. Alice's own `default` collection, by
    # contrast, comes back writable. One server, both answers.
    sqlite3 "$DB" "INSERT INTO calendars (id, synctoken, components) \
        VALUES (${SHARED_CAL_ID}, 1, 'VEVENT');"
    sqlite3 "$DB" "INSERT INTO calendarinstances \
        (calendarid, principaluri, access, displayname, uri) \
        VALUES (${SHARED_CAL_ID}, 'principals/bob@test.local', 1, 'Bob''s calendar', 'default');"
    sqlite3 "$DB" "INSERT INTO calendarinstances \
        (calendarid, principaluri, access, displayname, uri, share_href, share_invitestatus) \
        VALUES (${SHARED_CAL_ID}, 'principals/${HARNESS_USER}', 2, 'Bob (read-only)', \
                'bob-readonly', 'mailto:${HARNESS_USER}', 2);"
    echo "initialized SabreDAV database for ${HARNESS_USER} (+ Bob's read-only share)"
fi

php -S 0.0.0.0:8080 server.php &
SERVER_PID=$!

# Wait until the server accepts requests (a 401 counts — it is responding).
i=0
until curl -s -o /dev/null "http://127.0.0.1:8080/"; do
    i=$((i + 1))
    if [ "$i" -gt 150 ]; then
        echo "SabreDAV did not start in time" >&2
        exit 1
    fi
    sleep 0.2
done

sh seed.sh
touch data/.sabredav-ready
echo "SabreDAV harness ready"

wait "$SERVER_PID"
