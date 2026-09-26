#!/usr/bin/env bash
# Regenerates the GnuPG certificates and signatures in this directory. Committed so
# the bytes can be traced to a recipe; rerunning produces new keys (and new
# fingerprints), so any test that names a fingerprint must be updated with them.
#
# Made with GnuPG 2.4.8. Every timestamp is pinned with --faked-system-time so the
# tests can evaluate the certificates at known instants:
#   created   2025-01-01T00:00:00Z
#   signed    2025-02-01T00:00:00Z  (signatures over signed-entity.txt)
#   revoked   2025-06-01T00:00:00Z
#   expiring  2025-01-01 + 1 year
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
trap 'gpgconf --kill all; rm -rf "$GNUPGHOME"' EXIT

created='20250101T000000!'
signed='20250201T000000!'
later='20250601T000000!'
entity="$here/signed-entity.txt"
mkdir -p "$here/signatures"

gen() { # name uid algo usage expire
  gpg --batch --faked-system-time "$created" --passphrase '' \
      --quick-gen-key "$2" "$3" "$4" "$5"
}
fpr() { gpg --with-colons --fingerprint "$1" | awk -F: '/^fpr/ {print $10; exit}'; }
sign() { # name uid when [extra gpg options]
  local name="$1" uid="$2" when="$3"
  shift 3
  gpg --batch --yes --faked-system-time "$when" --pinentry-mode loopback --passphrase '' \
      "$@" --local-user "$uid" --armor --detach-sign \
      --output "$here/signatures/$name.asc" "$entity"
}

# The signed MIME entity, exactly as a multipart/signed first part covers it.
printf 'Content-Type: text/plain; charset=utf-8\r\n\r\nSigned by GnuPG.\r\n' > "$entity"

# An ordinary modern v4 key: Ed25519 primary, Curve25519 encryption subkey, no expiry.
gen alice 'Alice <alice@example.org>' ed25519 cert,sign never
gpg --batch --faked-system-time "$created" --passphrase '' \
    --quick-add-key "$(fpr alice@example.org)" cv25519 encr never
gpg --armor --export alice@example.org > "$here/alice.asc"
sign alice alice@example.org "$signed"

# RSA 2048: accepted with a warning (openpgp.md).
gen rsa 'Robin <robin@example.org>' rsa2048 default never
gpg --armor --export robin@example.org > "$here/rsa2048.asc"
sign rsa2048 robin@example.org "$signed"

# Expires one year after creation.
gen expiring 'Eve <eve@example.org>' ed25519 default 1y
gpg --armor --export eve@example.org > "$here/expiring.asc"
sign expiring eve@example.org "$signed"

# Revoked as compromised (hard) and as superseded (soft), half a year after creation,
# each having signed before its revocation.
revoke() { # name uid code
  gen "$1" "$2" ed25519 default never
  sign "$1" "$2" "$signed"
  # --gen-revoke wants a terminal; --edit-key's revkey takes scripted answers:
  # confirm, reason code, an empty description line, confirm, save.
  printf 'revkey\ny\n%s\n\ny\nsave\n' "$3" \
    | gpg --batch --yes --faked-system-time "$later" --command-fd 0 --status-fd 2 \
          --pinentry-mode loopback --passphrase '' --edit-key "$(fpr "$2")"
  gpg --armor --export "$2" > "$here/$1.asc"
}
revoke revoked-compromised 'Mallory <mallory@example.org>' 1   # 1 = compromised
revoke revoked-superseded 'Sam <sam@example.org>' 2            # 2 = superseded

# SHA-1 self-signatures either side of the 2023-02-01 cut-off (openpgp.md). Only RSA
# can carry one: Ed25519 and ECDSA refuse hashes shorter than 256 bits.
sha1() { # name uid when
  gpg --batch --faked-system-time "$3" --passphrase '' --cert-digest-algo SHA1 \
      --quick-gen-key "$2" rsa3072 default never
  gpg --armor --export "$2" > "$here/$1.asc"
}
sha1 sha1-self-signature-2020 'Old <old@example.org>' '20200101T000000!'
sha1 sha1-self-signature-2024 'New <new@example.org>' '20240101T000000!'
# A message signature on SHA-1, by the 2020 key, in 2020: never validates (§9.5).
sign sha1-message old@example.org '20200601T000000!' --digest-algo SHA1

# Refused outright: RSA under 2048 bits (RFC 9580 §12.4) and DSA (§12.5).
gen rsa1024 'Tiny <tiny@example.org>' rsa1024 default never
gpg --armor --export tiny@example.org > "$here/rsa1024.asc"
sign rsa1024 tiny@example.org "$signed"
gen dsa 'Dora <dora@example.org>' dsa2048 default never
gpg --armor --export dora@example.org > "$here/dsa.asc"
