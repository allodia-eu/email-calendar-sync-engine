#!/bin/sh
# Deterministic content seeder for the Stalwart test harness.
#
# Called by entrypoint.sh once the server is a full, bootstrapped server with the
# test accounts present. Pure curl over the real wire protocols the engine's
# clients will use (steps 4-5): IMAP for mail, CalDAV for calendars, and
# CardDAV for contacts. No Rust
# provider client and no extra binary.
#
# Transport (Stalwart defaults, v0.16 and v1 alike): IMAP is implicit-TLS on 993 — `-k` accepts the
# server's self-signed test certificate (this never touches a host trust store).
# CalDAV/CardDAV ride the plaintext HTTP listener on 8080.
#
# Idempotent: managed mailboxes are cleared before append and CalDAV PUT is
# idempotent by request URI, so a re-run converges to the same state. Asserting
# side: never assert on server-assigned ids (IMAP UID, DAV ETag) — fixtures
# carry content the harness controls (subjects, Message-IDs, iCalendar UIDs).
set -eu

SEED_DIR="${SEED_DIR:-/harness/seed}"
MAIL_DIR="$SEED_DIR/mail"
CAL_DIR="$SEED_DIR/calendar"
CONTACT_DIR="$SEED_DIR/contacts"

ALICE="alice@test.local"
ALICE_PW="${HARNESS_ALICE_PW:-harness-alice-pw}"
BOB="bob@test.local"
BOB_PW="${HARNESS_BOB_PW:-harness-bob-pw}"
DAVE="dave@test.local"
DAVE_PW="${HARNESS_DAVE_PW:-harness-dave-pw}"
SUPPORT="support@test.local"

# The group mailbox, as alice reaches it: under the `Shared Folders` namespace Stalwart
# advertises (RFC 2342). The space is percent-encoded because the path rides a curl URL.
SUPPORT_SHARED="Shared%20Folders/support@test.local"

IMAPS="imaps://127.0.0.1:993"
HTTP="http://127.0.0.1:8080"
CAL_COLLECTION="$HTTP/dav/cal/$ALICE/default"
WORK_COLLECTION="$HTTP/dav/cal/$ALICE/work"
CONTACT_COLLECTION="$HTTP/dav/card/$ALICE/default"
DAVE_SHARED_COLLECTION="$HTTP/dav/cal/$DAVE/shared"

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

# The same round trip as `imap_cmd`, authenticated as **bob** — needed only to grant alice
# an ACL on bob's own INBOX, which nobody but its owner can do.
imap_cmd_bob() { # mailbox  command
  curl -sk --user "$BOB:$BOB_PW" --url "$IMAPS/$1" --request "$2"
}

imap_clear() { # mailbox
  imap_cmd "$1" "STORE 1:* +FLAGS (\\Deleted)" >/dev/null 2>&1 || true
  imap_cmd "$1" "EXPUNGE" >/dev/null 2>&1 || true
}

put_calendar() { # file  uid  [collection]
  put_calendar_as "$ALICE:$ALICE_PW" "$@"
}

# PUTs a calendar object as `user:password`. Anything but 201/204 is a real failure: a share
# that quietly failed to seed would read, later, as the engine failing to see it.
put_calendar_as() { # user:password  file  uid  collection
  _status=$(sed 's/$/\r/' "$2" | curl -sk -o /dev/null -w '%{http_code}' --user "$1" \
    -X PUT -H 'Content-Type: text/calendar; charset=utf-8' \
    --data-binary @- "${4:-$CAL_COLLECTION}/$3.ics")
  case "$_status" in
    200 | 201 | 204) : ;;
    *)
      log "PUT ${4:-$CAL_COLLECTION}/$3.ics failed: HTTP $_status"
      exit 1
      ;;
  esac
}

make_calendar() { # collection  display name
  make_calendar_as "$ALICE:$ALICE_PW" "$@"
}

# One JMAP method call as `user:password`, against the user's own session.
jmap_as() { # user:password  method-call
  curl -s --user "$1" -H 'Content-Type: application/json' -X POST "$HTTP/jmap/" \
    -d "{\"using\":[\"urn:ietf:params:jmap:core\",\"urn:ietf:params:jmap:calendars\"],\"methodCalls\":[$2]}"
}

# The user's own account id for calendars, read off the session's `primaryAccounts`.
calendar_account_id() { # user:password
  curl -s --user "$1" "$HTTP/jmap/session" \
    | grep -oE '"primaryAccounts":\{[^}]*\}' \
    | grep -oE '"urn:ietf:params:jmap:calendars":"[^"]+"' | cut -d'"' -f4
}

# Creates a calendar collection with a display name. 201 = created; 405 = it already exists
# (a re-run). Anything else is a real failure.
make_calendar_as() { # user:password  collection  display name
  _status=$(curl -s -o /dev/null -w '%{http_code}' --user "$1" \
    -X MKCALENDAR -H 'Content-Type: application/xml; charset=utf-8' \
    --data-binary "<?xml version=\"1.0\" encoding=\"utf-8\"?><C:mkcalendar xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:set><D:prop><D:displayname>$3</D:displayname></D:prop></D:set></C:mkcalendar>" \
    "$2/")
  case "$_status" in
    201 | 405) : ;;
    *)
      log "MKCALENDAR $2/ failed: HTTP $_status"
      exit 1
      ;;
  esac
}

put_contact() { # file  uid
  sed 's/$/\r/' "$1" | curl -sk --user "$ALICE:$ALICE_PW" \
    -X PUT -H 'Content-Type: text/vcard; charset=utf-8' \
    --data-binary @- "$CONTACT_COLLECTION/$2.vcf"
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
# Two levels under Archive, so the folder list is a tree rather than a flat namespace. Two
# rather than one: a single level proves only that a name was split at its delimiter, while the
# display name a host shows is the last segment of the *whole* path. Both stay empty, so they
# add rows to the mailbox list without touching a count anything asserts on.
imap_cmd INBOX "CREATE Archive/Nested" >/dev/null 2>&1 || true
imap_cmd INBOX "CREATE Archive/Nested/Deeper" >/dev/null 2>&1 || true
# QResync is a dedicated, otherwise-untouched mailbox the CONDSTORE/QRESYNC delta
# test mutates in isolation (it re-flags one message and expunges another), so it
# never disturbs the count-asserted INBOX/Archive/Projects.
imap_cmd INBOX "CREATE QResync" >/dev/null 2>&1 || true
# Idle is a second dedicated mailbox the IMAP IDLE (RFC 2177) push test watches and
# flag-toggles in isolation, so its mutations never disturb the count-asserted
# mailboxes either.
imap_cmd INBOX "CREATE Idle" >/dev/null 2>&1 || true
# JmapState and JmapStateArchive are a dedicated pair the JMAP state-delta test mutates in
# isolation: it flags a message and moves it between the two, which is the shape an
# `Email/changes` `updated` id takes. Isolated for the same reason as QResync — the test moves
# a message *out* of a mailbox, and the shared ones are count-asserted.
imap_cmd INBOX "CREATE JmapState" >/dev/null 2>&1 || true
imap_cmd INBOX "CREATE JmapStateArchive" >/dev/null 2>&1 || true
# JmapSent is the home of the sent-observation test's own message. That test moves it into
# the account's real Sent collection — the one mailbox it cannot provision, because the role
# is the server's to assign — and back again, so its message must belong to nothing else.
imap_cmd INBOX "CREATE JmapSent" >/dev/null 2>&1 || true
# Reported holds the report suites' own two messages — one per test, because cargo runs the
# tests in a binary concurrently and CI passes no `--test-threads=1`. A report *files* the
# message, so it needs a mailbox of its own for the JmapState reason; it needs two messages
# so the junk test and the phishing test cannot read each other's keywords.
imap_cmd INBOX "CREATE Reported" >/dev/null 2>&1 || true
# A non-ASCII mailbox name, seeded here *and* on the Dovecot harness under the same
# display name. The two servers put completely different bytes on the wire for it —
# modified UTF-7 on rev1, UTF-8 on rev2 — so a mailbox that comes back with one identity
# on both is the proof that the encoding stops at the transport. The literal below is the
# modified-UTF-7 form, which is what this rev1-shaped CREATE has to send.
imap_cmd INBOX "CREATE &ANw-berweisungen" >/dev/null 2>&1 || true

log "clearing managed mailboxes for an idempotent re-seed"
imap_clear INBOX
imap_clear Archive
imap_clear Projects
imap_clear QResync
imap_clear Idle
imap_clear JmapState
imap_clear JmapStateArchive
imap_clear JmapSent
imap_clear Reported

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

# APPENDed, never COPYed from INBOX: JMAP models a message in several mailboxes as ONE Email
# with several mailboxIds, so an IMAP COPY inside one account adds a mailbox to the existing
# object rather than minting a second one. The state-delta test MOVEs this message, which
# replaces its mailboxIds — on a copy that would drag the shared baseline out of INBOX and
# Archive and break every count asserted there.
log "seeding the dedicated JmapState mailbox (its own message) for the JMAP state-delta test"
imap_append "$MAIL_DIR/08-jmap-state.eml" JmapState

log "seeding the dedicated JmapSent mailbox (its own message) for the sent-observation test"
imap_append "$MAIL_DIR/09-jmap-sent.eml" JmapSent

log "seeding the dedicated Reported mailbox (a message per test) for the report suites"
imap_append "$MAIL_DIR/10-report-junk.eml" Reported
imap_append "$MAIL_DIR/11-report-phishing.eml" Reported

log "seeding the dedicated Idle mailbox (one message) for the IMAP IDLE push test"
imap_cmd INBOX "COPY 1 Idle" >/dev/null

log "moving a message from INBOX into Projects (single membership)"
imap_append "$MAIL_DIR/07-moved.eml" INBOX # seq 9
if ! imap_cmd INBOX "MOVE 9 Projects" >/dev/null 2>&1; then
  imap_cmd INBOX "COPY 9 Projects" >/dev/null
  imap_cmd INBOX "STORE 9 +FLAGS (\\Deleted)" >/dev/null
  imap_cmd INBOX "EXPUNGE" >/dev/null
fi

# ---- The shared-mailbox fixture (RFC 2342 namespaces + RFC 4314 ACL) ----
#
# Two stores alice can open besides her own, differing on purpose in their *rights*:
# she is a member of the `support` group, so she holds every right on its folders, while
# bob grants her only `lr` (lookup + read) on his INBOX. Stalwart reports both as
# `isReadOnly: false` in alice's JMAP session, so nothing at the account level tells them
# apart — only the per-folder rights do (docs/agent-guidance/stalwart-harness.md).
log "granting alice read-only (lr) access to bob's INBOX"
imap_cmd_bob INBOX "SETACL \"INBOX\" \"$ALICE\" lr" >/dev/null

log "seeding the support group mailbox"
imap_clear "$SUPPORT_SHARED/INBOX"
imap_append "$MAIL_DIR/12-shared.eml" "$SUPPORT_SHARED/INBOX"

log "putting calendar fixtures into the default calendar"
put_calendar "$CAL_DIR/one-off.ics" oneoff-2001
put_calendar "$CAL_DIR/recurring-weekly.ics" weekly-2002
put_calendar "$CAL_DIR/meeting-attendees.ics" meeting-2003
put_calendar "$CAL_DIR/virtual-location.ics" virtual-2004
put_calendar "$CAL_DIR/all-day.ics" allday-2005
put_calendar "$CAL_DIR/floating.ics" floating-2006

log "creating the second calendar, Work, and putting its fixtures"
make_calendar "$WORK_COLLECTION" Work
put_calendar "$CAL_DIR/work/planning.ics" work-2101 "$WORK_COLLECTION"
put_calendar "$CAL_DIR/work/offsite.ics" work-2102 "$WORK_COLLECTION"

# ---- The shared-calendar fixture: calendars in a store that is not alice's ----
#
# Alice can reach three stores besides her own, and between them they cover every shape a
# shared store comes in (docs/agent-guidance/stalwart-harness.md):
#
#   support (group) — mail and calendar, every right on both
#   bob             — mail alone: `lr` on his INBOX (above); his calendar home answers 403
#   dave            — calendar alone: one calendar, read-only; his mail answers nothing
#
# Carol shares nothing, so she is the store alice cannot reach at all.
log "putting the support group's calendar fixture (written as alice, a member)"
put_calendar "$CAL_DIR/shared/support-rota.ics" support-2301 \
  "$HTTP/dav/cal/$SUPPORT/default"

log "creating dave's shared calendar and its fixture"
make_calendar_as "$DAVE:$DAVE_PW" "$DAVE_SHARED_COLLECTION" "Dave Shared"
put_calendar_as "$DAVE:$DAVE_PW" "$CAL_DIR/shared/dave-shared.ics" dave-shared-2201 \
  "$DAVE_SHARED_COLLECTION"

# The share itself is JMAP's (draft-ietf-jmap-calendars `shareWith`), made as dave: only an
# owner can share, and it is keyed by the grantee's principal id, which on Stalwart is her
# account id. Ids are server-assigned, so both are looked up rather than hard-coded.
log "sharing dave's calendar with alice, read-only"
ALICE_ID=$(calendar_account_id "$ALICE:$ALICE_PW")
DAVE_ID=$(calendar_account_id "$DAVE:$DAVE_PW")
DAVE_SHARED_ID=$(jmap_as "$DAVE:$DAVE_PW" \
  "[\"Calendar/get\",{\"accountId\":\"$DAVE_ID\",\"properties\":[\"name\",\"id\"]},\"c0\"]" \
  | grep -oE '"name":"Dave Shared","id":"[^"]+"' | cut -d'"' -f8)
[ -n "$ALICE_ID" ] && [ -n "$DAVE_ID" ] && [ -n "$DAVE_SHARED_ID" ] || {
  log "could not resolve the ids for dave's share (alice=$ALICE_ID dave=$DAVE_ID calendar=$DAVE_SHARED_ID)"
  exit 1
}
_resp=$(jmap_as "$DAVE:$DAVE_PW" \
  "[\"Calendar/set\",{\"accountId\":\"$DAVE_ID\",\"update\":{\"$DAVE_SHARED_ID\":{\"shareWith\":{\"$ALICE_ID\":{\"mayReadFreeBusy\":true,\"mayReadItems\":true}}}}},\"c0\"]")
printf '%s' "$_resp" | grep -q '"updated"' || {
  log "sharing dave's calendar failed: $_resp"
  exit 1
}

log "putting contact fixtures into the default address book"
put_contact "$CONTACT_DIR/contact-3001.vcf" contact-3001
put_contact "$CONTACT_DIR/group-3002.vcf" group-3002

log "content seed complete"
