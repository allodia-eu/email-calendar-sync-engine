#!/usr/bin/env bash
#
# Stalwart live protocol harness — the single source of truth shared by CI
# (`.github/workflows/ci.yml`) and local dev.
#
# The `provider-*` offline suites drive fakes that serve canned bytes regardless
# of the request, so a wrong *command/request shape* (e.g. an unparenthesized
# `UID FETCH` item list) can pass every offline test yet break against a real
# server. This wraps the Docker harness + the gated live test suites so that
# check is a one-liner in both places, and so the endpoints/credentials live in
# exactly one file.
#
# Usage:
#   scripts/ci/stalwart-live.sh up                    # start + self-seed (compose up --wait)
#   scripts/ci/stalwart-live.sh smoke                 # connectivity smoke suite
#   scripts/ci/stalwart-live.sh test <crate> [args…]  # one crate's live tests (args are a filter)
#   scripts/ci/stalwart-live.sh providers             # jmap + imap + caldav live suites
#   scripts/ci/stalwart-live.sh all                   # up → smoke → providers (full local pass)
#   scripts/ci/stalwart-live.sh logs                  # dump server logs (for a failure)
#   scripts/ci/stalwart-live.sh down                  # stop + wipe volumes
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HARNESS_DIR="$REPO_ROOT/docker/stalwart"

# The harness endpoints/credentials — loopback, throwaway creds (mirrors the
# `docker/stalwart` seed). `:-` lets a caller override, but the defaults are what
# the seeded server exposes, so CI and local dev need set nothing.
export STALWART_HTTP_ADDR="${STALWART_HTTP_ADDR:-127.0.0.1:18080}"
export STALWART_IMAP_ADDR="${STALWART_IMAP_ADDR:-127.0.0.1:11993}" # implicit-TLS
export STALWART_SMTP_ADDR="${STALWART_SMTP_ADDR:-127.0.0.1:11025}"
export STALWART_ACCOUNT="${STALWART_ACCOUNT:-alice@test.local}"
export STALWART_PASSWORD="${STALWART_PASSWORD:-harness-alice-pw}"

usage() { sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

cmd="${1:-}"
[ "$#" -gt 0 ] && shift || true

case "$cmd" in
  up)
    # The service self-bootstraps + seeds in its entrypoint and only reports
    # healthy once the shared dataset is ready, so `--wait` is real readiness.
    (cd "$HARNESS_DIR" && docker compose up -d --wait)
    ;;
  down)
    (cd "$HARNESS_DIR" && docker compose down -v)
    ;;
  logs)
    cd "$HARNESS_DIR"
    docker compose logs --no-color
    docker compose exec -T stalwart sh -c 'cat /var/log/stalwart/* 2>/dev/null' || true
    ;;
  smoke)
    (cd "$REPO_ROOT" && cargo test -p stalwart-harness --test smoke -- --nocapture)
    ;;
  test)
    [ "$#" -ge 1 ] || { echo "error: 'test' needs a crate name" >&2; exit 2; }
    crate="$1"; shift
    (cd "$REPO_ROOT" && cargo test -p "$crate" --all-features "$@" -- --nocapture)
    ;;
  providers)
    "${BASH_SOURCE[0]}" test provider-jmap
    "${BASH_SOURCE[0]}" test provider-imap
    "${BASH_SOURCE[0]}" test provider-caldav
    ;;
  all)
    "${BASH_SOURCE[0]}" up
    "${BASH_SOURCE[0]}" smoke
    "${BASH_SOURCE[0]}" providers
    ;;
  ""|-h|--help|help)
    usage
    [ "$cmd" = "" ] && exit 1 || exit 0
    ;;
  *)
    echo "error: unknown command '$cmd'" >&2
    usage >&2
    exit 1
    ;;
esac
