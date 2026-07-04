#!/bin/sh
# PID 1 for the SabreDAV harness container: initialize the SQLite database from
# SabreDAV's own schema, seed one principal, start the PHP built-in server, seed
# the calendar over CalDAV, write a readiness marker, then run the server in the
# foreground. Re-running against an existing data volume skips init and re-seeds
# idempotently (a clean volume re-initializes).
set -eu

cd /app
DB="data/db.sqlite"
SCHEMA="sql"

if [ ! -f "$DB" ]; then
    mkdir -p data
    # SabreDAV's canonical schema, vendored under sql/ (its composer dist omits
    # examples/). Load exactly the tables we use.
    cat "$SCHEMA/sqlite.principals.sql" \
        "$SCHEMA/sqlite.calendars.sql" \
        "$SCHEMA/sqlite.propertystorage.sql" \
        "$SCHEMA/sqlite.locks.sql" | sqlite3 "$DB"
    # One principal for the harness account (calendars hang off it).
    sqlite3 "$DB" "INSERT INTO principals (uri, email, displayname) \
        VALUES ('principals/${HARNESS_USER}', '${HARNESS_USER}', 'Alice Tester');"
    echo "initialized SabreDAV database for ${HARNESS_USER}"
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
