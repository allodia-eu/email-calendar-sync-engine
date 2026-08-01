#!/bin/bash
# Bring up a single-mailbox Gromox for the MAPI/HTTP spike. Idempotent: safe to
# re-run against a warm volume. Touches a marker file once the MAPI/HTTP
# endpoint actually answers, so the healthcheck gates on readiness rather than
# on a sleep.
set -euo pipefail

MYSQL_HOST=${MYSQL_HOST:-gromox-db}
MYSQL_USER=${MYSQL_USER:-gromox}
MYSQL_PASS=${MYSQL_PASS:-gromox}
MYSQL_DB=${MYSQL_DB:-gromox}
DOMAIN=${DOMAIN:-spike.test}
MBOX_USER=${MBOX_USER:-alice@spike.test}
MBOX_PASS=${MBOX_PASS:-alicepass}
MARKER=/var/lib/gromox/.mapi-ready

log() { echo "[harness] $*"; }

rm -f "$MARKER"

log "waiting for mariadb at $MYSQL_HOST"
for _ in $(seq 1 60); do
  mysql -h "$MYSQL_HOST" -u "$MYSQL_USER" -p"$MYSQL_PASS" -e 'SELECT 1' "$MYSQL_DB" >/dev/null 2>&1 && break
  sleep 2
done
mysql -h "$MYSQL_HOST" -u "$MYSQL_USER" -p"$MYSQL_PASS" -e 'SELECT 1' "$MYSQL_DB" >/dev/null

# --- gromox <-> mysql wiring -------------------------------------------------
mkdir -p /etc/gromox
cat > /etc/gromox/mysql_adaptor.cfg <<EOF
mysql_username=$MYSQL_USER
mysql_password=$MYSQL_PASS
mysql_dbname=$MYSQL_DB
mysql_host=$MYSQL_HOST
EOF

# Serve plaintext HTTP on 80 only. The spike deliberately skips TLS: MAPI/HTTP
# auth here is HTTP Basic on loopback, and TLS is not what is being measured.
cat > /etc/gromox/http.cfg <<EOF
http_listen=[::]:80
http_auth_basic=yes
EOF

# exmdb is gromox's store IPC. Configured here so the setting is visible, but
# note the KNOWN LIMITATION: gromox binds [::1]:5000 regardless of this value
# while its own exmdb_client resolves `localhost` to 127.0.0.1, so out-of-process
# tools (gromox-eml2mt | gromox-mt2exm) cannot seed mail. MAPI/HTTP itself is
# unaffected — gromox-http reaches the store in-process. See tools/mapi-spike/HANDOFF.md.
cat > /etc/gromox/exmdb_provider.cfg <<EOF
exmdb_listen=::1:5000
EOF

mkdir -p /etc/grommunio-admin-api/conf.d
cat > /etc/grommunio-admin-api/conf.d/database.yaml <<EOF
DB:
  host: '$MYSQL_HOST'
  user: '$MYSQL_USER'
  pass: '$MYSQL_PASS'
  database: '$MYSQL_DB'
EOF

# --- schema ------------------------------------------------------------------
if ! mysql -h "$MYSQL_HOST" -u "$MYSQL_USER" -p"$MYSQL_PASS" -D "$MYSQL_DB" \
     -N -e "SELECT COUNT(*) FROM information_schema.tables \
            WHERE table_schema='$MYSQL_DB' AND table_type='BASE TABLE'" | grep -qv '^0$'; then
  log "creating gromox schema"
  gromox-dbop -C
else
  log "schema present"
fi

# --- domain + mailbox --------------------------------------------------------
# grommunio-admin errors on an already-existing object rather than no-opping, so
# each `|| true` makes a re-run against a warm volume converge instead of
# aborting the container. `user create` provisions the store by default
# (--no-maildir opts out); the password is a separate `passwd` call.
log "provisioning domain $DOMAIN and mailbox $MBOX_USER"
grommunio-admin domain create "$DOMAIN" -u 10 2>&1 | tail -3 || true
grommunio-admin user create "$MBOX_USER" 2>&1 | tail -3 || true
grommunio-admin passwd "$MBOX_USER" -p "$MBOX_PASS" 2>&1 | tail -3 || true

log "users now known to gromox:"
grommunio-admin user query username maildir 2>&1 | tail -5 || true

# --- run ---------------------------------------------------------------------
# The daemons live in libexec, not on PATH.
log "starting gromox http daemon"
/usr/libexec/gromox/http &
HTTP_PID=$!

log "waiting for /mapi/emsmdb/ to answer"
for _ in $(seq 1 45); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -u "$MBOX_USER:$MBOX_PASS" \
    -X POST -H 'Content-Type: application/mapi-http' -H 'X-RequestType: PING' \
    -H 'X-RequestId: {00000000-0000-0000-0000-000000000001}:1' \
    -H 'X-ClientApplication: Outlook/15.00.0000.0000' \
    --data-binary '' http://127.0.0.1/mapi/emsmdb/ 2>/dev/null) || code=000
  # 000 is "no answer yet". Anything else means the listener is up and spoke
  # HTTP — including 401/500, which are still evidence for CP0.
  if [ "$code" != "000" ]; then
    log "endpoint answering (HTTP $code)"
    touch "$MARKER"
    break
  fi
  sleep 2
done

wait "$HTTP_PID"
