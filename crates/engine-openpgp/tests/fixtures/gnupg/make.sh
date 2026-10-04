#!/usr/bin/env bash
# Regenerates the GnuPG certificates in this directory. Committed so the bytes can be
# traced to a recipe; rerunning produces new keys (and new fingerprints), so any test
# that names a fingerprint must be updated with them.
#
# Made with GnuPG 2.4.8. Every timestamp is pinned with --faked-system-time so the
# tests can evaluate the certificates at known instants:
#   created   2025-01-01T00:00:00Z
#   revoked   2025-06-01T00:00:00Z
#   expiring  2025-01-01 + 1 year
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
trap 'gpgconf --kill all; rm -rf "$GNUPGHOME"' EXIT

created='20250101T000000!'
later='20250601T000000!'

gen() { # name uid algo usage expire
  gpg --batch --faked-system-time "$created" --passphrase '' \
      --quick-gen-key "$2" "$3" "$4" "$5"
}
fpr() { gpg --with-colons --fingerprint "$1" | awk -F: '/^fpr/ {print $10; exit}'; }

# An ordinary modern v4 key: Ed25519 primary, Curve25519 encryption subkey, no expiry.
gen alice 'Alice <alice@example.org>' ed25519 cert,sign never
gpg --batch --faked-system-time "$created" --passphrase '' \
    --quick-add-key "$(fpr alice@example.org)" cv25519 encr never
gpg --armor --export alice@example.org > "$here/alice.asc"

# RSA 2048: accepted with a warning (openpgp.md).
gen rsa 'Robin <robin@example.org>' rsa2048 default never
gpg --armor --export robin@example.org > "$here/rsa2048.asc"

# Expires one year after creation.
gen expiring 'Eve <eve@example.org>' ed25519 default 1y
gpg --armor --export eve@example.org > "$here/expiring.asc"

# Revoked as compromised (hard) and as superseded (soft), half a year after creation.
revoke() { # uid code
  gen "$1" "$1" ed25519 default never
  # --gen-revoke wants a terminal; --edit-key's revkey takes scripted answers:
  # confirm, reason code, an empty description line, confirm, save.
  printf 'revkey\ny\n%s\n\ny\nsave\n' "$2" \
    | gpg --batch --yes --faked-system-time "$later" --command-fd 0 --status-fd 2 \
          --pinentry-mode loopback --passphrase '' --edit-key "$(fpr "$1")"
}
revoke 'Mallory <mallory@example.org>' 1   # 1 = compromised
gpg --armor --export mallory@example.org > "$here/revoked-compromised.asc"
revoke 'Sam <sam@example.org>' 2           # 2 = superseded
gpg --armor --export sam@example.org > "$here/revoked-superseded.asc"

# SHA-1 self-signatures either side of the 2023-02-01 cut-off (openpgp.md). Only RSA
# can carry one: Ed25519 and ECDSA refuse hashes shorter than 256 bits.
sha1() { # name uid when
  gpg --batch --faked-system-time "$3" --passphrase '' --cert-digest-algo SHA1 \
      --quick-gen-key "$2" rsa3072 default never
  gpg --armor --export "$2" > "$here/$1.asc"
}
sha1 sha1-self-signature-2020 'Old <old@example.org>' '20200101T000000!'
sha1 sha1-self-signature-2024 'New <new@example.org>' '20240101T000000!'

# Refused outright: RSA under 2048 bits (RFC 9580 §12.4) and DSA (§12.5).
gen rsa1024 'Tiny <tiny@example.org>' rsa1024 default never
gpg --armor --export tiny@example.org > "$here/rsa1024.asc"
gen dsa 'Dora <dora@example.org>' dsa2048 default never
gpg --armor --export dora@example.org > "$here/dsa.asc"
