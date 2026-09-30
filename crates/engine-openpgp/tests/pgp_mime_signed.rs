//! A PGP/MIME signed message, end to end: walked by engine-e2e, verified here,
//! summarised by engine-core (RFC 3156 §5, RFC 9787 §3.1).

mod support;

use engine_core::e2e::Summary;
use engine_e2e::{LayerRecogniser, Walk, Walked};
use engine_openpgp::{Certificate, PgpMime, Verifier};
use support::{at, gnupg, gnupg_signature, signed_entity};

const RECOGNISERS: &[&dyn LayerRecogniser] = &[&PgpMime];

/// A multipart/signed message around GnuPG's entity and signature, as a MUA sends it.
fn message(entity: &[u8]) -> Vec<u8> {
    let mut raw = b"From: Alice <alice@example.org>\r\n\
        To: bob@example.org\r\n\
        Subject: signed\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: multipart/signed; micalg=pgp-sha512;\r\n \
        protocol=\"application/pgp-signature\"; boundary=\"sig\"\r\n\
        \r\n\
        --sig\r\n"
        .to_vec();
    raw.extend(entity);
    raw.extend(
        b"\r\n--sig\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\n\r\n",
    );
    raw.extend(gnupg_signature("alice"));
    raw.extend(b"\r\n--sig--\r\n");
    raw
}

fn walk(raw: &[u8]) -> Walked {
    match engine_e2e::walk(raw, RECOGNISERS) {
        Walk::Complete(walked) => walked,
        other => panic!("a signed message never pauses: {other:?}"),
    }
}

fn verify(walked: &mut Walked, sender: &str) {
    let certificates = [Certificate::parse(&gnupg("alice")).expect("parses")];
    let verifier = Verifier::new(&certificates, sender, at("2025-03-01"));
    let checks: Vec<_> = walked
        .detached_signatures()
        .iter()
        .map(|detached| {
            (
                detached.layer(),
                verifier.detached(&detached.signed(), detached.signature()),
            )
        })
        .collect();
    for (layer, verdicts) in checks {
        walked.record_verdicts(layer, verdicts);
    }
}

#[test]
fn a_signed_message_from_its_signer_is_verified() {
    let mut walked = walk(&message(&signed_entity()));
    assert_eq!(
        walked.summary(),
        Some(Summary::Unprotected),
        "nothing checked yet"
    );
    verify(&mut walked, "alice@example.org");
    assert_eq!(walked.summary(), Some(Summary::Verified));
}

#[test]
fn a_signed_message_from_someone_else_is_unprotected() {
    // RFC 9787 §3.1: Verified means a valid signature from the apparent sender.
    let mut walked = walk(&message(&signed_entity()));
    verify(&mut walked, "mallory@example.org");
    assert_eq!(walked.summary(), Some(Summary::Unprotected));
}

#[test]
fn a_message_altered_in_transit_is_unprotected() {
    let mut altered = signed_entity();
    let last = altered.len() - 3;
    altered[last] ^= 0x20;
    let mut walked = walk(&message(&altered));
    verify(&mut walked, "alice@example.org");
    assert_eq!(walked.summary(), Some(Summary::Unprotected));
}

#[test]
fn a_message_stored_with_bare_line_feeds_still_verifies() {
    // RFC 3156 §5: the signer signed CRLF; the walk restores it.
    let lf: Vec<u8> = message(&signed_entity())
        .into_iter()
        .filter(|&byte| byte != b'\r')
        .collect();
    let mut walked = walk(&lf);
    verify(&mut walked, "alice@example.org");
    assert_eq!(walked.summary(), Some(Summary::Verified));
}
