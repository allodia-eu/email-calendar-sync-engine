#!/bin/sh
# Seed the harness calendar: create the `default` collection, then PUT the shared
# calendar fixtures over CalDAV (the same six .ics the Stalwart harness uses, so
# both fixtures validate one dataset). Idempotent: MKCALENDAR is ignored if the
# collection already exists, and PUT is keyed by URL, so a re-run converges.
set -eu

ADDR="127.0.0.1:8080"
AUTH="${HARNESS_USER}:${HARNESS_PASS}"
# URL-encode the '@' in the account name for the collection path.
USER_ENC=$(printf '%s' "$HARNESS_USER" | sed 's/@/%40/g')
CAL="http://${ADDR}/calendars/${USER_ENC}/default"

# Create the calendar collection. 201 = created; 405 = already exists (idempotent
# re-run). Any other status (401 auth, 5xx, …) is a real failure — surface it
# instead of masking it with `|| true`.
status=$(curl -s -o /dev/null -w '%{http_code}' -u "$AUTH" -X MKCALENDAR "${CAL}/")
case "$status" in
    201 | 405) : ;;
    *)
        echo "MKCALENDAR ${CAL}/ failed: HTTP ${status}" >&2
        exit 1
        ;;
esac

count=0
for ics in seed/calendar/*.ics; do
    name=$(basename "$ics")
    curl -fsS -u "$AUTH" -X PUT \
        -H "Content-Type: text/calendar; charset=utf-8" \
        --data-binary @"$ics" "${CAL}/${name}" >/dev/null
    count=$((count + 1))
done
echo "seeded ${count} calendar objects into ${CAL}/"
