#!/bin/sh
# Deterministic content seeder for the Stalwart test harness.
#
# Called by entrypoint.sh once the server is a full, bootstrapped server with the
# test accounts present. Pure curl over the real wire protocols the engine's
# clients will use (steps 4-5): IMAP for mail, CalDAV for calendars. No Rust
# provider client and no extra binary.
#
# Transport (v0.16 defaults): IMAP is implicit-TLS on 993 — `-k` accepts the
# server's self-signed test certificate (this never touches a host trust store).
# CalDAV rides the plaintext HTTP listener on 8080.
#
# Idempotent: managed mailboxes are cleared before append and CalDAV PUT is
# idempotent by request URI, so a re-run converges to the same state. Asserting
# side: never assert on server-assigned ids (IMAP UID, DAV ETag) — fixtures
# carry content the harness controls (subjects, Message-IDs, iCalendar UIDs).
set -eu

SEED_DIR="${SEED_DIR:-/harness/seed}"
MAIL_DIR="$SEED_DIR/mail"
CAL_DIR="$SEED_DIR/calendar"

ALICE="alice@test.local"
ALICE_PW="${HARNESS_ALICE_PW:-harness-alice-pw}"

IMAPS="imaps://127.0.0.1:993"
HTTP="http://127.0.0.1:8080"
CAL_COLLECTION="$HTTP/dav/cal/$ALICE/default"

log() { printf '[seed] %s\n' "$1"; }

# curl over implicit-TLS IMAP, accepting the self-signed test cert.
imap() { curl -sk --user "$ALICE:$ALICE_PW" "$@"; }

imap_append() { # file  mailbox
  # curl's IMAP APPEND needs the literal size upfront, so it cannot stream from
  # a pipe ("Cannot APPEND with unknown input file size"); stage to a temp file
  # whose size curl can stat.
  _tmp=$(mktemp)
  sed 's/$/\r/' "$1" >"$_tmp"
  _rc=0
  imap --url "$IMAPS/$2" --upload-file "$_tmp" || _rc=$?
  rm -f "$_tmp"
  return "$_rc"
}

imap_cmd() { # mailbox  command
  imap --url "$IMAPS/$1" --request "$2"
}

imap_clear() { # mailbox
  imap_cmd "$1" "STORE 1:* +FLAGS (\\Deleted)" >/dev/null 2>&1 || true
  imap_cmd "$1" "EXPUNGE" >/dev/null 2>&1 || true
}

put_calendar() { # file  uid
  sed 's/$/\r/' "$1" | curl -sk --user "$ALICE:$ALICE_PW" \
    -X PUT -H 'Content-Type: text/calendar; charset=utf-8' \
    --data-binary @- "$CAL_COLLECTION/$2.ics"
}

log "waiting for IMAP to accept a login for $ALICE"
i=0
until imap_cmd INBOX "NOOP" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -gt 60 ] && {
    log "IMAP never became ready"
    exit 1
  }
  sleep 1
done

log "ensuring mailboxes exist"
imap_cmd INBOX "CREATE Archive" >/dev/null 2>&1 || true
imap_cmd INBOX "CREATE Projects" >/dev/null 2>&1 || true
# QResync is a dedicated, otherwise-untouched mailbox the CONDSTORE/QRESYNC delta
# test mutates in isolation (it re-flags one message and expunges another), so it
# never disturbs the count-asserted INBOX/Archive/Projects.
imap_cmd INBOX "CREATE QResync" >/dev/null 2>&1 || true

log "clearing managed mailboxes for an idempotent re-seed"
imap_clear INBOX
imap_clear Archive
imap_clear Projects
imap_clear QResync

# INBOX was just cleared, so appends land at deterministic sequence numbers
# (Stalwart's SEARCH does not match on a HEADER Message-ID, so we rely on append
# order rather than searching). The trailing comments are those sequence numbers.
log "appending mail fixtures to INBOX"
imap_append "$MAIL_DIR/01-plain.eml" INBOX        # seq 1
imap_append "$MAIL_DIR/02-dup-msgid-a.eml" INBOX  # seq 2
imap_append "$MAIL_DIR/02-dup-msgid-b.eml" INBOX  # seq 3
imap_append "$MAIL_DIR/03-no-msgid.eml" INBOX     # seq 4
imap_append "$MAIL_DIR/04-attachment.eml" INBOX   # seq 5
imap_append "$MAIL_DIR/05-flagged.eml" INBOX      # seq 6
imap_append "$MAIL_DIR/06-thread-root.eml" INBOX  # seq 7
imap_append "$MAIL_DIR/06-thread-reply.eml" INBOX # seq 8

log "setting flags + custom keyword on the flagged fixture (seq 6)"
imap_cmd INBOX "STORE 6 +FLAGS (\\Seen \\Flagged harness)" >/dev/null

log "copying the baseline message (seq 1) into Archive (two memberships)"
imap_cmd INBOX "COPY 1 Archive" >/dev/null

log "seeding the dedicated QResync mailbox (three messages) for the QRESYNC delta test"
imap_cmd INBOX "COPY 1:3 QResync" >/dev/null

log "moving a message from INBOX into Projects (single membership)"
imap_append "$MAIL_DIR/07-moved.eml" INBOX # seq 9
if ! imap_cmd INBOX "MOVE 9 Projects" >/dev/null 2>&1; then
  imap_cmd INBOX "COPY 9 Projects" >/dev/null
  imap_cmd INBOX "STORE 9 +FLAGS (\\Deleted)" >/dev/null
  imap_cmd INBOX "EXPUNGE" >/dev/null
fi

log "putting calendar fixtures into the default calendar"
put_calendar "$CAL_DIR/one-off.ics" oneoff-2001
put_calendar "$CAL_DIR/recurring-weekly.ics" weekly-2002
put_calendar "$CAL_DIR/meeting-attendees.ics" meeting-2003
put_calendar "$CAL_DIR/virtual-location.ics" virtual-2004
put_calendar "$CAL_DIR/all-day.ics" allday-2005
put_calendar "$CAL_DIR/floating.ics" floating-2006

log "content seed complete"
