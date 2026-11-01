//! The exact bytes a detached signature covers (RFC 3156 §5, RFC 2046 §5.1.1).

mod support;

use engine_core::e2e::{
    CertificateId, ChainStatus, GoodSignature, SenderBinding, SignatureVerdict, Summary,
};
use support::{crlf, walk_all};

const SIGNED: &str = "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"s\"\n\
\n\
preamble, not signed\n\
--s\n\
Content-Type: text/plain\n\
\n\
hello\n\
\n\
--s\n\
Content-Type: application/pgp-signature\n\
\n\
SIGNATURE\n\
--s--\n\
epilogue, not signed\n";

#[test]
fn the_crlf_before_the_boundary_is_not_signed() {
    // "hello\r\n\r\n--s": the last CRLF belongs to the delimiter; the one before it
    // is content.
    let walked = walk_all(&crlf(SIGNED));
    let signatures = walked.detached_signatures();
    assert_eq!(signatures.len(), 1);
    assert_eq!(
        &*signatures[0].signed(),
        b"Content-Type: text/plain\r\n\r\nhello\r\n",
    );
    assert_eq!(signatures[0].signature(), b"SIGNATURE");
}

#[test]
fn lf_only_transport_is_canonicalised_to_crlf() {
    // RFC 3156 §5: line endings are converted to CRLF before verification.
    let walked = walk_all(SIGNED.as_bytes());
    let signatures = walked.detached_signatures();
    assert_eq!(
        &*signatures[0].signed(),
        b"Content-Type: text/plain\r\n\r\nhello\r\n",
    );
}

#[test]
fn existing_crlf_is_not_doubled_and_stray_cr_is_kept() {
    let raw = SIGNED.replace("hello\n", "he\rllo\n");
    let walked = walk_all(&crlf(&raw));
    assert_eq!(
        &*walked.detached_signatures()[0].signed(),
        b"Content-Type: text/plain\r\n\r\nhe\rllo\r\n",
    );
}

#[test]
fn a_part_without_headers_starts_with_its_blank_line() {
    let raw = SIGNED.replace("Content-Type: text/plain\n\nhello", "\nhello");
    let walked = walk_all(&crlf(&raw));
    assert_eq!(&*walked.detached_signatures()[0].signed(), b"\r\nhello\r\n");
}

#[test]
fn the_signature_part_is_transfer_decoded() {
    let raw = SIGNED.replace(
        "Content-Type: application/pgp-signature\n\nSIGNATURE",
        "Content-Type: application/pgp-signature\nContent-Transfer-Encoding: base64\n\nU0lHTkFUVVJF",
    );
    let walked = walk_all(&crlf(&raw));
    assert_eq!(walked.detached_signatures()[0].signature(), b"SIGNATURE");
}

#[test]
fn recorded_verdicts_reach_the_summary() {
    let mut walked = walk_all(&crlf(SIGNED));
    let layer = walked.detached_signatures()[0].layer();
    walked.record_verdicts(
        layer,
        vec![SignatureVerdict::Good(GoodSignature {
            signer: CertificateId::open_pgp(&[1; 20]).expect("v4"),
            created: "2026-09-25T10:00:00Z".parse().expect("time"),
            sender: SenderBinding::Matches,
            chain: ChainStatus::NotApplicable,
            warnings: Vec::new(),
        })],
    );
    assert_eq!(walked.summary(), Some(Summary::Verified));
}

#[test]
fn every_detached_layer_offers_its_signature_in_order() {
    let inner = SIGNED.replace("\"s\"", "\"t\"").replace("--s", "--t");
    let outer = SIGNED.replace("Content-Type: text/plain\n\nhello\n", &inner);
    let walked = walk_all(&crlf(&outer));
    let signatures = walked.detached_signatures();
    assert_eq!(signatures.len(), 2);
    assert!(
        signatures[0]
            .signed()
            .starts_with(b"Content-Type: multipart/signed")
    );
    assert_eq!(
        &*signatures[1].signed(),
        b"Content-Type: text/plain\r\n\r\nhello\r\n",
    );
    assert_ne!(signatures[0].layer(), signatures[1].layer());
}
