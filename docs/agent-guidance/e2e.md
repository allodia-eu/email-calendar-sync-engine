# End-to-End Protection Guidance

Read before touching `engine_core::e2e`, the `engine-e2e` crate, or anything that signs,
encrypts, verifies or decrypts mail.

## The model: RFC 9787, for every mechanism

RFC 9787 (Informational, 2025) is the neutral description of protected mail, and this layer
follows its vocabulary exactly:

- A **cryptographic layer** is a MIME substructure that protects the subtree inside it (§4.1).
  S/MIME spells layers as `multipart/signed`, `signed-data`, `enveloped-data` and
  `authEnveloped-data` (§4.1.1); PGP/MIME as `multipart/signed` and `multipart/encrypted`
  (§4.1.2). Compression is **not** a layer (§4.1.1.5).
- The **envelope** is the largest contiguous run of layers from the message root (§4.2). The
  **payload** is the first part inside it that is not a layer (§4.3).
- A layer found inside the payload is **errant** (§4.5) and never contributes to the message's
  status. A mailing list that wraps a signed message with a footer produces exactly that.
- The **summary** is the one status a message carries (§3.1, §3.2): Unprotected, Verified,
  Confidential, or Encrypted But Unverified.

The engine never assumes OpenPGP above the mechanism line. Every type a host sees names the
*problem*, and S/MIME is expected to join as a second mechanism without new neutral types.

## Where things live

| Crate | Holds | Depends on crypto? |
|---|---|---|
| `engine-core` (`e2e` module) | `Mechanism`, `CertificateId`, `CertificateFacts` (+ `CertificateStatus`, `Revocation`, `RevocationReach`, `CertificateProblem`), `LayerKind`, `SignatureVerdict` (+ `SenderBinding`, `ChainStatus`, `UnusableReason`, `IssuerHint`), `DecryptionOutcome` (+ `Integrity`, `Warning`), `Summary` and its derivation | No |
| `engine-e2e` | The layer walk, `LayerRecogniser`, `Envelope`, `Walked`, the exact bytes a detached signature covers | No |
| a mechanism crate: `engine-openpgp` (`openpgp.md`) | Recognition of its layers, verification, decryption, composition, certificate semantics | Yes |

Host-visible facts sit in `engine-core` so a store or a host never has to depend on the walk.

## The walk

`engine_e2e::walk(raw, recognisers)` goes from the root inwards, asking each registered
`LayerRecogniser` about each part and taking the first answer. A recogniser looks at structure
only and answers with a `LayerShape`:

- `DetachedSignature`: `multipart/signed`. The walk itself checks the RFC 1847 arity (exactly two
  children) and goes straight on into child 0, because a detached signature's protected part is
  readable.
- `EmbeddedSignature(Object)` and `Encryption(Object)`: the protected part is sealed inside an
  object (the part's own body, or a named child). The walk **pauses** and returns
  `Walk::EmbeddedSignature` or `Walk::Encrypted` with the object's content-transfer-decoded bytes.

The pause is the seam that keeps secret keys out of the engine: the *caller* opens the object with
whatever keys it holds and resumes with `decrypted(entity, decryption, inner_verdicts)` /
`unwrapped(entity, verdicts)`, or ends the walk with `failed(..)` or `stop()`. It is a value, not a
callback, so the caller may open asynchronously (a host-held key on a smartcard, an OS identity)
without this crate knowing. An encryption layer whose content carries a signature becomes
`LayerKind::SignedAndEncrypted` (PGP/MIME's combined form, RFC 3156 §6.2).

Each opened entity becomes a new **source**; every `PartRef` names its source (0 is the message,
then one per opened layer) and the child path inside it.

When the walk reaches the payload it scans the payload's subtree, depth first, for:

- **errant layers** — recorded, and not entered: their subtree is shown in isolation;
- **inline protection** in `text/plain` parts (RFC 9787 §6.2.3) — recorded as a finding, never
  walked or validated;
- **malformed layers** — a part that announced itself as a layer and broke that layer's structure
  (a `multipart/signed` with one part, a `multipart/encrypted` whose control part is missing).
  These are treated as ordinary parts and recorded, so a host can say "damaged" instead of
  silently rendering ciphertext. A malformed part at the root makes the message itself the
  payload.

An encapsulated `message/rfc822` is never entered: a forwarded message has its own envelope.

### Detached signature bytes

`Walked::detached_signatures()` returns, per detached layer, the exact signed bytes:

- the protected part from its first header byte to the end of its body, **excluding** the CRLF
  that belongs to the following boundary delimiter (RFC 2046 §5.1.1, RFC 3156 §5's note);
- with every bare LF converted to CRLF (RFC 3156 §5), because mail that crossed an LF-only store
  arrives that way and the signer signed CRLF. Existing CRLF is not doubled; a lone CR is kept.

The caller verifies and records the verdicts with `Walked::record_verdicts(layer, verdicts)`; for
OpenPGP the verdicts come from `engine_openpgp::Verifier::detached` (`openpgp.md`).
Verification is not a pause step: the payload is readable without it, and classifying a message
at sync time must not need any key.

## The summary rule

`Summary::derive` (in `engine-core`) reads the envelope's layers, outermost first:

- A signature counts only when it is **inside every encryption layer**. A signature outside the
  encryption signed the ciphertext, which does not tie its signer to what the reader sees
  (RFC 9787 §5.2).
- It counts only when its verdict is `Good` **and** its certificate binds the sender
  (`SenderBinding::Matches`): "a valid signature from the apparent sender" (§3.1).
- Encrypted and counted → Confidential; encrypted and not → Encrypted But Unverified; not
  encrypted and counted → Verified; otherwise Unprotected. A cleartext message with a bad
  signature is Unprotected (§3.1, §6.4).
- `Walked::summary()` returns `None` while the payload is sealed (not opened, failed, or past the
  depth limit). Whether a sealed message is Confidential depends on a signature nobody has seen,
  so the engine does not guess.

Verdicts are **facts**. `Good` from a certificate the user never accepted is still only a fact
about the mathematics; acceptance and trust are the host's decision.

## Invariants

- **Errant layers never count** toward the summary.
- **Nothing sealed is summarised.**
- **Hostile input never panics.** Unparseable input becomes a payload with no envelope; nesting
  stops at `MAX_LAYERS` (8) with `SealReason::TooDeep`. RFC 9787 sets no limit; nothing
  legitimate comes close.
- **Decrypted content is never printed.** `Walked`, `EncryptedLayer` and
  `EmbeddedSignatureLayer` redact their `Debug` to lengths.
- **A failed decryption releases nothing** (RFC 9580 §13.7): `DecryptionOutcome::Failed` carries
  no bytes, and the walk ends with the payload sealed.
- **A `CertificateId` is a full fingerprint**, never a key ID: 20 octets (OpenPGP v4) or 32 (v6).
  Version 3 keys (16-octet MD5 fingerprints) are not accepted. A key ID travels as
  `IssuerHint::Handle`. The length is checked on deserialisation too.

## Room left for S/MIME

The neutral layer must not assume OpenPGP. Each row is a place where it could, and what it does
instead:

| S/MIME difference | What the neutral layer allows for |
|---|---|
| Encryption is one `application/pkcs7-mime` part; signing is detached or embedded; layers nest in any order (RFC 8551 §3.7) | `Object::Body`, `LayerShape::EmbeddedSignature`, and a walk that records any order up to `MAX_LAYERS` |
| Trust comes from a PKIX chain (RFC 8550); OpenPGP has none | `GoodSignature::chain`, `ChainStatus::NotApplicable` for OpenPGP |
| Unauthenticated CBC `enveloped-data` is still common (EFAIL) | `Decryption::integrity` (`Authenticated` / `Unauthenticated`); the host decides what to show |
| The signer must match From (MUST in RFC 8550 §6, MAY in RFC 9580) | `SenderBinding` on every good signature, and the summary requires `Matches` |
| Certificates are identified differently | `CertificateId` carries its `Mechanism`; a new mechanism adds a constructor with its own length rule |

## Tests

The walk is tested before any real mechanism exists, through a **fake mechanism**
(`crates/engine-e2e/tests/support/`) that recognises PGP/MIME and S/MIME structures by content
type and "decrypts" by handing the object back. It covers every shape in RFC 9787 §4.1.1 and
§4.1.2, the multilayer example (§4.4.2), mailing-list wrapping (§4.5.1), the baroque example
(§4.5.2), compression not being a layer, encrypt-then-sign ordering, forwarded messages, the
signed-byte range and CRLF canonicalisation, the depth limit, malformed layers, inline findings,
and a hostile-input corpus (truncations and byte soup). The summary rule has its own table of
tests in `engine-core`.
