# OpenPGP Guidance

Read before touching `engine-openpgp`, or anything that reads, verifies, decrypts, generates or
composes OpenPGP. Read `e2e.md` first: this crate is one mechanism behind that neutral layer, and
nothing it does may leak OpenPGP into a type a host sees.

## What the crate is

`engine-openpgp` implements OpenPGP (RFC 9580) framed as PGP/MIME (RFC 3156). It is pure: no I/O,
no async, no store. Secret keys are handed in per call and never kept.

It sits on **rPGP** (the `pgp` crate), pinned exactly (`=0.20.0`) because 0.x releases break.
rPGP is pure Rust with no `unsafe`, was audited in 2024, and has shipped in Delta Chat for years.
Default features are off: the only default, `bzip2`, adds a codec for a MAY-level compression
algorithm (RFC 9580 §9.4) that nothing current sends. `draft-pqc` stays off (see "Not now").

rPGP parses packets and does the cryptography. **What it does not do is certificate
semantics**: which self-signature counts, whether a key is expired or revoked at a given time,
which subkey may be used for what, which preferences apply. That layer is ours, and it is where
most conformance work sits. When rPGP and this document disagree about a policy, this document
wins and the difference is enforced in our code, with a test.

Rejected: Sequoia (LGPL). Its production crypto backends are C or C++ that must be
cross-compiled for iOS and Android; its only pure-Rust backend is experimental, variable-time and
unaudited.

## Conformance table

Every normative statement in RFC 9580 (221 paragraphs carrying MUST, SHOULD, REQUIRED or
RECOMMENDED) and the RFC 3156 statements about OpenPGP are rows in
`crates/engine-openpgp/tests/conformance/*.json`. Each row has a **scope**:

- `library`: rPGP must do it; we pin it with a test so an upgrade that stops doing it fails.
- `policy`: our code decides; the decision is below or in the row's `note`.
- `not-applicable`: with the reason.

A row that applies is `covered` (naming at least one test) or `planned` (naming none yet). A test
in the crate loads the tables and fails when a named test function does not exist anywhere under
`crates/`, when a covered row names nothing, or when a planned row claims a test. The rows were
seeded by the extraction recipe in `openpgp-conformance.md`; the counts live there.

## Policy

These are the decisions RFC 9580 leaves to the implementation. Each has, or will have, tests that
name its row.

### Hash algorithms

| Context | MD5 | SHA-1, RIPEMD-160 | SHA2-*, SHA3-* |
|---|---|---|---|
| A message signature | refused | refused | accepted |
| A self-signature (binding, direct key, revocation) | refused | accepted if made **before 2023-02-01**, with `Warning::DeprecatedHash`; refused after | accepted |

§9.5: "MUST NOT validate any recent signature that depends on MD5, SHA-1, or RIPEMD-160", and
SHOULD NOT validate an old one unless it "predates known weakness … and the implementation is
confident that the message has been in the secure custody of the user the whole time". Mail sits
in a server's mailbox, which is not the reader's custody, so **no message signature on a weak hash
ever validates**, whatever its date.

Self-signatures are different in kind. An attacker exploiting a SHA-1 collision needs the
*victim* to sign one of the two colliding documents; in a self-signature the only signer is the
key's owner, so what is left is second-preimage resistance, which SHA-1 still has. Refusing them
all would make much of the deployed v4 key base unusable, because GnuPG made SHA-1
self-signatures by default for years. The cut-off date is Sequoia's for the same argument,
recorded here rather than invented, and after it a SHA-1 self-signature is refused like any other
weak signature.

### Public-key algorithms

| Algorithm | Sign / verify | Encrypt to | Decrypt with |
|---|---|---|---|
| Ed25519, Ed448, Ed25519Legacy (v4 only) | yes | — | — |
| X25519, X448, Curve25519Legacy ECDH (v4 only) | — | yes | yes |
| ECDSA / ECDH over NIST P-256, P-384, P-521 | yes | yes | yes |
| RSA ≥ 3072 bits | yes | yes | yes |
| RSA 2048–3071 bits | yes, with `Warning::WeakPublicKey` | yes, with the warning | yes, with the warning |
| RSA < 2048 bits | **refused** | **refused** | **refused** |
| DSA | **refused** (§12.5 MUST NOT) | — | — |
| Elgamal (16) | — | **refused** (§12.6 MUST NOT) | yes, with `Warning::DeprecatedPublicKeyAlgorithm` |
| Elgamal sign (20), RSA sign-only / encrypt-only (2, 3) | refused | refused | refused |
| Brainpool curves | unsupported by rPGP: `Unsupported` | | |

**Deviation, RSA 2048–3071.** §12.4 says SHOULD NOT encrypt, sign or verify with these. They are
common in deployed keys, and refusing them would stop users corresponding with people who cannot
change their key on our schedule. They work, and every use carries a warning the host shows.

### Key flags

| Use | Required flag (RFC 9580 §5.2.3.29) |
|---|---|
| Verifying a signature | 0x02 sign, on the key that made it |
| Encrypting to someone else | 0x04 encrypt communications |
| Encrypting a draft to oneself | 0x04 or 0x08 (encrypt storage) |

§5.2.3.29 leaves "communications" against "storage" to the implementation; a draft is data at
rest the sender will read later, which is what 0x08 describes, so either flag serves it. A key
whose binding carries **no** Key Flags subpacket has no usage: every key made since RFC 2440 states
its flags, and guessing from the algorithm is how a signing key ends up encrypting.

### Self-signatures and revocation

- The **most recent valid** self-signature at the evaluation time wins (§5.2.3.10); a newer one
  that does not verify is ignored, not fatal.
- A v6 key is unusable without a valid **Direct Key** signature (§5.2.3.10). A v4 key's
  key-level facts (flags, expiry, preferences) come from its primary User ID's self-signature, by
  the v4 convention §5.2.3.10 describes, falling back to a Direct Key signature.
- The primary User ID is, among those flagged primary, the one with the most recent
  self-signature (§5.2.3.27); with none flagged, the most recent overall.
- A signing-capable subkey needs a valid embedded 0x19 back-signature (§5.2.1.8, §10.1.5).
- Preferences, flags and expiry are read **only from hashed subpackets** (§13.13). When a hashed
  subpacket repeats, rPGP takes the first; §5.2.3.9 permits any scheme, and that is ours.
- Key Expiration Time counts from the key's creation, not the signature's.
- **Revocation is hard or soft** (§5.2.3.31). *Superseded* (1), *retired* (3) and *User ID no
  longer valid* (32) are soft: signatures made before the revocation stay valid. Everything else is
  hard, including *compromised* (2), *no reason* (0), a missing reason and an unknown one: every
  signature the key ever made is suspect.
- Designated revokers (Revocation Key, §5.2.3.23) are deprecated and ignored.

### Addresses

A User ID binds an address when it is `Name <addr-spec>` or a bare addr-spec. Addresses compare
after **Autocrypt Level 1 canonicalisation**: the domain converted to IDNA ASCII and lowercased,
the local part lowercased. This is deliberately looser than the engine's contact identity
(`contacts.md`, local part exact): a binding check that disagrees with the sender's own client
about case would tell a reader that a genuine signature came from someone else, and Autocrypt,
WKD (which lowercases the local part to build its lookup) and Thunderbird all compare this way.

### Resource limits (on data being read)

| Limit | Value | Why |
|---|---|---|
| Layers of compression in one message | 2 | §13.14; real mail uses one. |
| Decompressed size | 100 × the compressed input, at least 1 MiB | Thunderbird's ratio; the floor keeps small, highly repetitive text readable. |
| Argon2 memory when unlocking or decrypting | 512 MiB by default, host-configurable | rPGP allows 2 GiB, which a phone cannot give. RFC 9580's A.12 vectors need 2 GiB and run in their own job. |

### Output

- **Armour**: never a CRC24 (§6.1 forbids it on v6 objects and discourages it on v4; the interop
  corpus decides whether any v4 target needs one), never a `Version` header (§6.2.2.1).
- **Padding** in SEIPDv2: one Padding packet, last inside the encryption, sized so the padded
  length follows the Padmé rule (at most 12% overhead, leaking O(log log n) bits of the length).
  §5.14 leaves the size open.
- **Literal Data**: format `b`, empty file name, zero date (§5.9). Nothing is compressed.
- **Randomness**: the OS CSPRNG (§13.10).

## Generation profiles

The engine offers two and **has no default**; the caller chooses.

| | Interoperable v4 profile | RFC 9580 v6 profile |
|---|---|---|
| Primary key | EdDSALegacy / Ed25519Legacy (22), flags certify + sign | Ed25519 (27), flags certify + sign |
| Encryption subkey | ECDH / Curve25519Legacy (18), KDF and KEK per §11.5.1, flags encrypt communications + storage | X25519 (25), flags encrypt communications + storage |
| Where preferences sit | The User ID self-signature (0x13), the v4 convention | A Direct Key signature (0x1F); the User ID is still certified, so sender binding works |
| Shape | Exactly five packets: the Autocrypt Level 1 shape | §10.1.1 |
| Signatures | v4; Issuer Fingerprint and a matching Issuer Key ID | v6 with Table 23 salts; Issuer Fingerprint; no Issuer Key ID (§5.2.3.12) |
| Features | {SEIPDv1, SEIPDv2} | {SEIPDv1, SEIPDv2} |

Both: preferred ciphers [AES-256, AES-128]; AEAD [AES-256/OCB, AES-128/OCB]; hashes [SHA2-512,
SHA2-256]; compression [Uncompressed] stated explicitly (§12.3.1); Signature Creation Time hashed
and critical; Key Flags and Key Expiration Time critical.

**Deviation, the v4 profile.** RFC 9580 §5.5.2: implementations "SHOULD create keys with version 6
format … SHOULD NOT generate a version 4 key". The v4 profile exists for interoperability with
deployed implementations that cannot read v6 keys or SEIPDv2 (among them the Thunderbird and
GnuPG releases current when this was written, and Autocrypt Level 1, which cannot carry a v6
key). A host opts into it knowingly. The condition that ends the need for it: the deployed
implementations a host's users correspond with read v6 keys and SEIPDv2.

## Secret-key protection

| Situation | Protection |
|---|---|
| Locked at rest with a high-entropy secret the host keeps | S2K usage 253 (AEAD, AES-256/OCB) with Salted S2K. §3.7.1 permits Salted S2K only "when string is high entropy", such as one the implementation generated; it keeps unlocking cheap for background work. |
| Exported with a user passphrase, v6 key | Usage 253 with Argon2, RFC 9106's second recommended option (t=3, p=4, m=64 MiB). |
| Exported with a user passphrase, v4 key | Usage 254 (CFB) with Iterated and Salted S2K at the maximum count. GnuPG and Thunderbird cannot read Argon2 or AEAD-protected keys, and §3.7.2 permits this "if Argon2 is not available" with a high count and a strong passphrase, so the passphrase's strength is checked. |

Never usage 255, never Simple S2K, never Argon2 without usage 253 (§3.7.2.1).

## Certificates

`Certificate::parse` reads exactly one transferable public key, armoured or binary (a keyring is
`NotOne`; version 3 and unknown versions are `Unsupported`). `Certificate::at(time)` evaluates it
at one instant, because nearly every answer depends on when: a signature made in March is checked
against the certificate as it stood in March. The evaluation applies the rules under "Policy" and
gives:

- `status()`: `Usable`, `NotYetValid`, `Expired`, `Revoked` (with its reach) or `Unusable`
  (no valid self-signature, or a refused algorithm). A retroactive revocation is in force at every
  instant, including before it was made.
- `binds(address)`: the `SenderBinding` a signature verdict carries.
- `signing_keys()` and `encryption_keys(purpose)`: the component keys that may be used, each with
  its own fingerprint and expiry. Empty unless the certificate is usable.
- `facts()`: the same, as `engine_core::e2e::CertificateFacts` for a host or a store.

User IDs follow their own rule: the latest statement about a User ID holds, so a certification
made after a revocation binds the address again, whatever the revocation's reason.

## PGP/MIME recognition

`PgpMime` is the crate's `engine_e2e::LayerRecogniser`:

- `multipart/signed` whose `protocol` is `application/pgp-signature` (compared without regard to
  case; quoting is invisible once parsed) is a detached signing layer. A second part labelled
  anything but `application/pgp-signature` makes it **malformed**. The walk checks the two-part
  arity for every mechanism. **`micalg` is ignored on read**: the signature packet names its
  hash inside what is verified, so a wrong `micalg` cannot mislead a reader. Composing sets it.
- `multipart/encrypted` whose `protocol` is `application/pgp-encrypted` is an encryption layer
  whose object is part 2, **if** part 1 is `application/pgp-encrypted` and part 2 is
  `application/octet-stream` and there are exactly two; otherwise it is malformed. The control
  part's `Version: 1` body is not checked: nothing in it is used.
- Either type with another protocol, or none, is not ours, so it stays an ordinary part unless
  another mechanism claims it.
- **Inline** protection is an armour header line, at the start of a line with only whitespace
  after it (§6.2.1): `-----BEGIN PGP MESSAGE-----` is inline encryption,
  `-----BEGIN PGP SIGNED MESSAGE-----` an inline signature. A quoted one (`> -----BEGIN …`) is
  not at the start of a line and is not found. Inline signatures are never validated
  (RFC 9787 §6.2.3.1).

## Not now

- **Post-quantum OpenPGP (RFC 9980).** rPGP has it behind `draft-pqc`, but its `ml-dsa`
  dependency carries an open timing advisory (RUSTSEC-2025-0144). Keep algorithms data-driven and
  revisit when that dependency is stable.
- **RSA decryption timing.** rPGP's `rsa` dependency is subject to the Marvin attack
  (RUSTSEC-2023-0071). It matters because deployed keys are often RSA (Thunderbird's default is
  RSA 3072). An accepted, tracked risk: update when `rsa` fixes it.
