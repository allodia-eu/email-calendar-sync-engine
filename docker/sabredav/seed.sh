#!/bin/sh
# Seed the harness calendars: create the `default` and `work` collections, then PUT the
# shared calendar fixtures over CalDAV (the same .ics the Stalwart harness uses, so both
# fixtures validate one dataset). Idempotent: MKCALENDAR is ignored if the
# collection already exists, and PUT is keyed by URL, so a re-run converges.
set -eu

ADDR="127.0.0.1:8080"
AUTH="${HARNESS_USER}:${HARNESS_PASS}"
# URL-encode the '@' in the account name for the collection path.
USER_ENC=$(printf '%s' "$HARNESS_USER" | sed 's/@/%40/g')
CAL="http://${ADDR}/calendars/${USER_ENC}/default"

# Create a calendar collection. 201 = created; 405 = already exists (idempotent
# re-run). Any other status (401 auth, 5xx, …) is a real failure — surface it
# instead of masking it with `|| true`.
make_calendar() { # collection
    status=$(curl -s -o /dev/null -w '%{http_code}' -u "$AUTH" -X MKCALENDAR "$1/")
    case "$status" in
        201 | 405) : ;;
        *)
            echo "MKCALENDAR $1/ failed: HTTP ${status}" >&2
            exit 1
            ;;
    esac
}

# PUTs every fixture in a directory into a collection.
put_all() { # directory  collection
    count=0
    for ics in "$1"/*.ics; do
        name=$(basename "$ics")
        curl -fsS -u "$AUTH" -X PUT \
            -H "Content-Type: text/calendar; charset=utf-8" \
            --data-binary @"$ics" "$2/${name}" >/dev/null
        count=$((count + 1))
    done
    echo "seeded ${count} calendar objects into $2/"
}

WORK="http://${ADDR}/calendars/${USER_ENC}/work"
make_calendar "$CAL"
make_calendar "$WORK"
put_all seed/calendar "$CAL"
put_all seed/calendar/work "$WORK"
