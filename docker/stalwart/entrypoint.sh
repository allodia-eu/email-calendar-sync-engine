#!/bin/sh
# Self-bootstrapping entrypoint for the deterministic Stalwart test harness.
#
# Stalwart v0.16 has no declarative config file: a fresh server boots into
# "bootstrap mode" and is configured through its JMAP management API, after
# which it must restart to come up as a full server. This wrapper drives that
# sequence non-interactively and deterministically, then seeds the shared
# dataset, so `docker compose up` yields an identical, ready server every time:
#
#   1. start the server (bootstrap mode if the store is empty),
#   2. complete setup via `x:Bootstrap/set` (no ACME/auto-TLS), restart to full,
#   3. create the test accounts via `x:Account/set` (idempotent),
#   4. create the `support` **group** mailbox and put alice in it, so the session
#      alice authenticates with carries a shared, non-personal account,
#   5. seed mail (IMAP over TLS), calendars (CalDAV), and contacts (CardDAV),
#   6. write a readiness marker and run the server in the foreground.
#
# It is idempotent: on a re-run against an already-bootstrapped data volume it
# skips bootstrap and skips existing accounts, and the content seeder clears
# before it appends. See docs/agent-guidance/stalwart-harness.md.
set -eu

CONFIG="${STALWART_CONFIG:-/etc/stalwart/config.json}"
HTTP="http://127.0.0.1:8080"
ADMIN_USER="admin"
ADMIN_PW="${HARNESS_ADMIN_PW:-harness-admin-pw}"
MARKER="/var/lib/stalwart/.harness-ready"

log() { printf '[harness] %s\n' "$1"; }

# One JMAP management call. $1 is the methodCalls array body (without brackets).
jmap() {
  curl -s -u "$ADMIN_USER:$ADMIN_PW" -H 'Content-Type: application/json' \
    -X POST "$HTTP/jmap" \
    -d "{\"using\":[\"urn:ietf:params:jmap:core\",\"urn:stalwart:jmap\"],\"methodCalls\":[$1]}"
}

start_server() {
  stalwart --config "$CONFIG" &
  SRV=$!
}

stop_server() {
  kill "$SRV" 2>/dev/null || true
  wait "$SRV" 2>/dev/null || true
}

wait_http() {
  i=0
  until curl -sf "$HTTP/healthz/live" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -gt 90 ] && {
      log "server HTTP never became ready"
      return 1
    }
    sleep 1
  done
}

# Bootstrap mode exposes the singleton Bootstrap object; normal mode 404s it.
in_bootstrap_mode() {
  jmap '["x:Bootstrap/get",{"ids":null},"c0"]' | grep -q '"serverHostname"'
}

# Id of the auto-created default domain (the only domain), parsed from the
# single-line JSON. No jq/python in the image, so this stays grep/sed.
domain_id() {
  jmap '["x:Domain/get",{"ids":null,"properties":["name"]},"c0"]' \
    | grep -oE '"id":"[^"]+"' | head -1 | cut -d'"' -f4
}

account_exists() {
  jmap '["x:Account/get",{"ids":null,"properties":["name"]},"c0"]' \
    | grep -q "\"name\":\"$1\""
}

# Id of a principal by local name. Asking for only `name` flattens each entry to
# `{"name":"alice","id":"c"}` (no nested credentials object), so one grep per entry
# is safe without jq. The ids are server-assigned and NOT stable across a fresh
# bootstrap, which is exactly why callers look them up instead of hardcoding them.
account_id() { # local-name
  jmap '["x:Account/get",{"ids":null,"properties":["name"]},"c0"]' \
    | grep -oE "\{\"name\":\"$1\",\"id\":\"[^\"]+\"\}" \
    | grep -oE '"id":"[^"]+"' | cut -d'"' -f4
}

ensure_account() { # local-name  description  password
  if account_exists "$1"; then
    log "account $1 already present"
    return 0
  fi
  resp=$(jmap "[\"x:Account/set\",{\"create\":{\"x\":{\"@type\":\"User\",\"name\":\"$1\",\"domainId\":\"$DOMAIN_ID\",\"description\":\"$2\",\"credentials\":{\"0\":{\"@type\":\"Password\",\"secret\":\"$3\"}},\"roles\":{\"@type\":\"User\"}}}},\"c0\"]")
  if ! printf '%s' "$resp" | grep -q '"created"'; then
    log "FAILED to create account $1: $resp"
    return 1
  fi
  log "created account $1"
}

# A **group** principal: a mailbox with no credentials of its own, which every member
# then sees as a non-personal account in their JMAP session
# (`accounts.<id>.isPersonal:false`, RFC 8620 §1.6.2) and as
# `Shared Folders/<address>/…` in their IMAP `LIST` (RFC 2342 shared namespace). This
# is the vendor-neutral analogue of a Microsoft 365 shared mailbox, and the fixture
# the engine's shared-mailbox discovery is tested against
# (docs/agent-guidance/stalwart-harness.md).
ensure_group() { # local-name  description
  if account_exists "$1"; then
    log "group $1 already present"
    return 0
  fi
  resp=$(jmap "[\"x:Account/set\",{\"create\":{\"g\":{\"@type\":\"Group\",\"name\":\"$1\",\"domainId\":\"$DOMAIN_ID\",\"description\":\"$2\"}}},\"c0\"]")
  if ! printf '%s' "$resp" | grep -q '"created"'; then
    log "FAILED to create group $1: $resp"
    return 1
  fi
  log "created group $1"
}

# Membership is recorded on the **member**, not the group: `memberGroupIds` is a
# set-valued map on the user principal. Idempotent — re-setting the same map is a
# no-op update.
ensure_group_member() { # member-local-name  group-local-name
  member_id="$(account_id "$1")"
  group_id="$(account_id "$2")"
  [ -n "$member_id" ] && [ -n "$group_id" ] || {
    log "FAILED to resolve ids for member $1 / group $2"
    return 1
  }
  resp=$(jmap "[\"x:Account/set\",{\"update\":{\"$member_id\":{\"memberGroupIds\":{\"$group_id\":true}}}},\"c0\"]")
  if ! printf '%s' "$resp" | grep -q '"updated"'; then
    log "FAILED to add $1 to group $2: $resp"
    return 1
  fi
  log "$1 is a member of group $2"
}

listener_exists() { # name
  jmap '["x:NetworkListener/get",{"ids":null,"properties":["name"]},"c0"]' \
    | grep -q "\"name\":\"$1\""
}

# Add a STARTTLS listener (`useTls` with `tlsImplicit:false`) if absent. Stalwart
# supports STARTTLS on 143/587 but recommends implicit TLS (993/465), so a fresh
# bootstrap comes up with 993/465 only; we add 143/587 to exercise the engine's STARTTLS
# transports (which real, older servers require). Sets CREATED_LISTENER=1 when it creates
# one, so the caller restarts once to bind the new socket (a fresh bootstrap; a warm start
# already has them in its config).
ensure_starttls_listener() { # create-key  name  bind  protocol
  if listener_exists "$2"; then
    log "listener $2 already present"
    return 0
  fi
  resp=$(jmap "[\"x:NetworkListener/set\",{\"create\":{\"$1\":{\"name\":\"$2\",\"bind\":{\"$3\":true},\"protocol\":\"$4\",\"useTls\":true,\"tlsImplicit\":false}}},\"c0\"]")
  if ! printf '%s' "$resp" | grep -q '"created"'; then
    log "FAILED to create listener $2: $resp"
    return 1
  fi
  CREATED_LISTENER=1
  log "created STARTTLS listener $2 ($3, $4)"
}

# Raise every inbound rate limiter out of the way (`x:MtaInboundThrottle`, one of the
# store-backed settings objects — v0.16 has no config file for these).
#
# Stalwart ships two enabled by default: 5/second per sender IP, and **25/hour per (sender
# domain, recipient)**. Both make a test's outcome depend on how many times the suite has
# already run, which is the opposite of what a deterministic fixture is for.
#
# The second one is the proven biter. RFC 6638 auto-scheduling mails *both* parties of every
# invitation, so the CalDAV scheduling suite spent that 25-message budget in about four runs
# — and past it Stalwart abandons the **whole** iTIP delivery, the attendee's calendar copy
# included, silently and with nothing in the log. Every scheduling test then timed out while
# the organizer's PUT still returned 201, which reads exactly like a code regression.
#
# Idempotent: a warm start just re-sets the same values. Takes effect without a restart.
relax_inbound_throttles() {
  ids=$(jmap '["x:MtaInboundThrottle/get",{"ids":null,"properties":["description"]},"c0"]' \
    | grep -oE '"id":"[^"]+"' | cut -d'"' -f4)
  if [ -z "$ids" ]; then
    log "no inbound throttles reported; nothing to relax"
    return 0
  fi
  for id in $ids; do
    resp=$(jmap "[\"x:MtaInboundThrottle/set\",{\"update\":{\"$id\":{\"rate\":{\"count\":1000000,\"period\":1000}}}},\"c0\"]")
    if ! printf '%s' "$resp" | grep -q '"updated"'; then
      log "FAILED to relax inbound throttle $id: $resp"
      return 1
    fi
  done
  log "relaxed inbound rate limiters: $(printf '%s' "$ids" | wc -w | tr -d ' ') throttle(s)"
}

trap 'stop_server; exit 0' TERM INT

rm -f "$MARKER"
log "starting Stalwart"
start_server
wait_http

if in_bootstrap_mode; then
  log "completing first-run bootstrap (internal directory, no ACME/auto-TLS)"
  jmap '["x:Bootstrap/set",{"update":{"singleton":{"requestTlsCertificate":false,"generateDkimKeys":false}}},"c0"]' >/dev/null
  log "restarting into full server"
  stop_server
  start_server
  wait_http
else
  log "store already bootstrapped; skipping setup"
fi

DOMAIN_ID="$(domain_id)"
[ -n "$DOMAIN_ID" ] || {
  log "could not resolve default domain id"
  exit 1
}
log "default domain id: $DOMAIN_ID"

ensure_account alice "Alice Tester" "${HARNESS_ALICE_PW:-harness-alice-pw}"
# Bob and Carol hold none of the seeded dataset, which is what makes them usable as the
# two parties of a scheduling run: RFC 6638 auto-scheduling has the server *mail* both the
# attendee (an invitation) and the organizer (the reply), and those arrive in the INBOX.
# Pointing that at Alice would break the exact seed counts the mail suites assert on, so
# the scheduling scenarios run entirely between these two scratch accounts.
ensure_account bob "Bob Tester" "${HARNESS_BOB_PW:-harness-bob-pw}"
ensure_account carol "Carol Tester" "${HARNESS_CAROL_PW:-harness-carol-pw}"

relax_inbound_throttles

# The shared-mailbox fixture: a credential-less group mailbox alice belongs to. The
# second half of the fixture — bob granting alice read-only ACL on *his* INBOX, which
# is what proves rights belong on the mailbox and not on the account — is an IMAP
# `SETACL` and lives in seed.sh with the rest of the IMAP seeding.
ensure_group support "Support Shared Mailbox"
ensure_group_member alice support

# STARTTLS listeners for the IMAP (143) and SMTP submission (587) transports the
# provider speaks in addition to implicit TLS. A newly created listener needs a server
# restart to bind its socket; a warm start already has them in its persisted config.
CREATED_LISTENER=0
ensure_starttls_listener imapstarttls imap "[::]:143" imap
ensure_starttls_listener submission submission "[::]:587" smtp
if [ "$CREATED_LISTENER" = 1 ]; then
  log "restarting to bind the new STARTTLS listeners"
  stop_server
  start_server
  wait_http
fi

log "seeding shared dataset"
SEED_DIR="${SEED_DIR:-/harness/seed}" /bin/sh /harness/seed.sh

touch "$MARKER"
log "harness ready"

wait "$SRV"
