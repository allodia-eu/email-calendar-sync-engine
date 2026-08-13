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
        doveadm save -u "$ACCOUNT" -m INBOX <"$eml"
        count=$((count + 1))
    done
    echo "seeded ${count} message(s) into INBOX for ${ACCOUNT}"

    # One message outside the inbox, so the folder list has a mailbox that is neither
    # empty nor the inbox to report a count for.
    doveadm save -u "$ACCOUNT" -m Sent </harness/seed/mail/01-plain.eml
    echo "seeded 1 message into Sent for ${ACCOUNT}"

    : >"$MARKER"
fi

wait "$DOVECOT_PID"
