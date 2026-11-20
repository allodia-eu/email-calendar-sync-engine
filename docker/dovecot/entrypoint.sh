#!/bin/sh
# Seed a Dovecot harness service, then hold the server in the foreground. Both services run
# this same script against their own volume, so the two dialects start from one dataset.
#
# Seeding runs through `doveadm`, which needs the server's auth socket, so the server
# starts first and the seed waits for it. The messages are the same ones the Stalwart
# harness seeds (`../stalwart/seed/mail`), so one dataset validates both servers.
#
# The mailboxes are not created here: `harness.conf` declares them with
# `auto = subscribe`, so the first userdb lookup creates them. Only mail is added.
#
# The image is deliberately minimal — it has no `sleep`, `touch` or `mkdir`, so the
# readiness wait is a bounded retry of the probe itself and the marker is written with a
# shell redirection. Adding coreutils to get three commands would be a worse trade.
set -eu

MARKER="/srv/vmail/.dovecot-harness-ready"
ACCOUNT="${HARNESS_ACCOUNT:?HARNESS_ACCOUNT must be set}"
# The shared-mailbox fixture's two owners. Fixed rather than configurable: the live suites
# name them, and another value could only ever fail to resolve.
SHARED_OWNER="support@test.local"
READ_ONLY_OWNER="bob@test.local"

/dovecot/sbin/dovecot -F &
DOVECOT_PID=$!
trap 'kill -TERM "$DOVECOT_PID" 2>/dev/null || true' INT TERM

if [ ! -f "$MARKER" ]; then
    # The first successful lookup is also what creates the `auto = subscribe` mailboxes.
    attempt=0
    until doveadm mailbox list -u "$ACCOUNT" >/dev/null 2>&1; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 500 ]; then
            echo "dovecot did not accept a doveadm lookup; giving up" >&2
            exit 1
        fi
    done

    count=0
    for eml in /harness/seed/mail/*.eml; do
        # The report fixtures belong to the Reported mailbox below, and the shared-mailbox
        # message to the shared store's owner — none of them to this INBOX.
        case "$eml" in
            */10-report-junk.eml|*/11-report-phishing.eml|*/12-shared.eml) continue ;;
        esac
        doveadm save -u "$ACCOUNT" -m INBOX <"$eml"
        count=$((count + 1))
    done
    echo "seeded ${count} message(s) into INBOX for ${ACCOUNT}"

    # One message per report test: cargo runs a binary's tests concurrently, so sharing
    # one would let each test read the other's keywords.
    doveadm save -u "$ACCOUNT" -m Reported </harness/seed/mail/10-report-junk.eml
    doveadm save -u "$ACCOUNT" -m Reported </harness/seed/mail/11-report-phishing.eml
    echo "seeded 2 message(s) into Reported for ${ACCOUNT}"

    # One message outside the inbox, so the folder list has a mailbox that is neither
    # empty nor the inbox to report a count for.
    doveadm save -u "$ACCOUNT" -m Sent </harness/seed/mail/01-plain.eml
    echo "seeded 1 message into Sent for ${ACCOUNT}"

    # The shared-mailbox fixture (harness.conf → "Shared mailboxes"): two stores alice can
    # open besides her own, deliberately unequal in rights. `support@` shares its whole
    # mailbox with her — every right — and holds the one message a shared-store sync must
    # find; `bob@` shares his INBOX read-only. The static passdb lets any user in, so the
    # owners need no provisioning beyond this first lookup, which also creates their
    # `auto = subscribe` folders.
    doveadm save -u "$SHARED_OWNER" -m INBOX </harness/seed/mail/12-shared.eml
    for mailbox in INBOX Sent Drafts Trash Junk Archive "Überweisungen"; do
        # An `auto = subscribe` folder exists only virtually until something opens it, and
        # an ACL needs the real one: granting on it first answers `No local acl file path`.
        doveadm mailbox status -u "$SHARED_OWNER" messages "$mailbox" >/dev/null
        doveadm acl set -u "$SHARED_OWNER" "$mailbox" "user=$ACCOUNT" \
            lookup read write write-seen write-deleted insert post expunge create delete admin
    done
    doveadm mailbox status -u "$READ_ONLY_OWNER" messages INBOX >/dev/null
    doveadm acl set -u "$READ_ONLY_OWNER" INBOX "user=$ACCOUNT" lookup read
    echo "shared ${SHARED_OWNER} (every right) and ${READ_ONLY_OWNER}'s INBOX (lr) with ${ACCOUNT}"

    : >"$MARKER"
fi

wait "$DOVECOT_PID"
