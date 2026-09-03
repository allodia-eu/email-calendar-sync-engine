//! Unit tests for the OAuth SASL mechanisms.
//!
//! The two initial-response vectors are the ones the specifications publish, decoded
//! and re-encoded byte for byte: RFC 7628 §4.1's `OAUTHBEARER` example and Google's
//! `XOAUTH2` example. A hand-written expectation would only prove the code agrees with
//! itself — these prove it agrees with the servers.

use super::*;

/// RFC 7628 §4.1: the IMAP success exchange's initial client response, verbatim.
const RFC7628_INITIAL: &str = "bixhPXVzZXJAZXhhbXBsZS5jb20sAWhvc3Q9c2VydmVyLmV4YW1wbGUuY29tAXBvcnQ9MTQzAWF1dGg9QmVhcmVyIHZGOWRmdDRxbVRjMk52YjNSbGNrQmhiSFJoZG1semRHRXVZMjl0Q2c9PQEB";

/// The token from that same example.
const RFC7628_TOKEN: &str = "vF9dft4qmTc2Nvb3RlckBhbHRhdmlzdGEuY29tCg==";

/// Google's published `XOAUTH2` example, base64 of
/// `user=someuser@example.com^Aauth=Bearer ya29.…^A^A`.
const XOAUTH2_INITIAL: &str = "dXNlcj1zb21ldXNlckBleGFtcGxlLmNvbQFhdXRoPUJlYXJlciB5YTI5LnZGOWRmdDRxbVRjMk52YjNSbGNrQmhkSFJoZG1semRHRXVZMjl0Q2cBAQ==";

#[test]
fn oauthbearer_builds_the_rfc_7628_initial_response() {
    let built = Mechanism::OAuthBearer
        .initial_response(
            "user@example.com",
            RFC7628_TOKEN,
            "server.example.com",
            Some(143),
        )
        .expect("clean credential");
    assert_eq!(built, RFC7628_INITIAL);
}

#[test]
fn xoauth2_builds_googles_initial_response() {
    let built = Mechanism::XOAuth2
        .initial_response(
            "someuser@example.com",
            "ya29.vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg",
            // XOAUTH2 carries neither, so both are ignored.
            "imap.example.com",
            Some(993),
        )
        .expect("clean credential");
    assert_eq!(built, XOAUTH2_INITIAL);
}

#[test]
fn oauthbearer_omits_the_port_when_the_dial_address_carried_none() {
    // RFC 7628 §3.1 makes `port` optional for a bearer token (only the OAuth 1.0a
    // signature mechanisms require it), so an unparsable address drops the pair rather
    // than inventing a number.
    let built = Mechanism::OAuthBearer
        .initial_response("user@example.com", "tok", "server.example.com", None)
        .expect("clean credential");
    let decoded = crate::base64::decode(&built).expect("base64");
    assert_eq!(
        String::from_utf8_lossy(&decoded),
        "n,a=user@example.com,\x01host=server.example.com\x01auth=Bearer tok\x01\x01"
    );
}

#[test]
fn a_credential_carrying_a_frame_byte_is_refused_before_encoding() {
    // The bytes that matter: SOH is the key/value separator itself (a token holding one
    // could append its own `auth=` pair), and CR/LF/NUL would break out of the command
    // line. Each is rejected on each component, on both mechanisms.
    for mechanism in [Mechanism::OAuthBearer, Mechanism::XOAuth2] {
        for hostile in ["a\x01b", "a\rb", "a\nb", "a\0b"] {
            assert!(
                mechanism
                    .initial_response(hostile, "tok", "h", Some(1))
                    .is_err(),
                "{mechanism:?} accepted {hostile:?} as a username"
            );
            assert!(
                mechanism
                    .initial_response("u", hostile, "h", Some(1))
                    .is_err(),
                "{mechanism:?} accepted {hostile:?} as a token"
            );
        }
        // The host reaches only the OAUTHBEARER blob, but it is screened either way so
        // the guard cannot rot if XOAUTH2 ever grows a field.
        assert!(
            mechanism
                .initial_response("u", "tok", "h\x01port=1", Some(1))
                .is_err(),
            "{mechanism:?} accepted a forged host"
        );
    }
    // A clean credential still encodes.
    assert!(
        Mechanism::XOAuth2
            .initial_response("u@example.com", "tok", "h", Some(1))
            .is_ok()
    );
}

/// Gmail's real pre-auth `CAPABILITY`, captured from `imap.gmail.com:993`.
const GMAIL_CAPABILITY: &str = "IMAP4rev1 UNSELECT IDLE NAMESPACE QUOTA ID XLIST CHILDREN \
     X-GM-EXT-1 XYZZY SASL-IR AUTH=XOAUTH2 AUTH=PLAIN AUTH=PLAIN-CLIENTTOKEN AUTH=OAUTHBEARER";

/// Yahoo's real pre-auth `CAPABILITY`, captured from `imap.mail.yahoo.com:993`.
///
/// Worth having verbatim because it **contradicts Yahoo's own documentation**, which
/// presents `AUTH=OAUTHBEARER` as the mechanism for its IMAP. The server advertises
/// `AUTH=XOAUTH2` as well. A fix built on the doc rather than on the bytes would have
/// left the mechanism preference resting on a server behaviour that does not exist.
const YAHOO_CAPABILITY: &str = "IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=XOAUTH2 AUTH=OAUTHBEARER ID \
     MOVE NAMESPACE XYMHIGHESTMODSEQ UIDPLUS LITERAL+ CHILDREN UNSELECT X-MSG-EXT OBJECTID \
     IDLE ENABLE UIDONLY X-UIDONLY LIST-EXTENDED LIST-STATUS SPECIAL-USE PARTIAL \
     APPENDLIMIT=41697280";

/// The `AUTH=`-stripped mechanism names in a capability line, as the transport hands
/// them to [`select`].
fn mechanisms(capability: &str) -> Vec<&str> {
    capability
        .split_whitespace()
        .filter_map(|atom| atom.strip_prefix("AUTH="))
        .collect()
}

#[test]
fn the_standard_mechanism_is_preferred_and_the_vendor_one_is_the_fallback() {
    // Both mechanisms, so the RFC 7628 one wins — and with it the live proof, since
    // this is the branch that would otherwise never run against a real server.
    assert_eq!(
        select(["PLAIN", "XOAUTH2", "OAUTHBEARER"]),
        Some(Mechanism::OAuthBearer)
    );
    // Microsoft 365 documents only the vendor mechanism: the fallback exists for it.
    assert_eq!(select(["PLAIN", "XOAUTH2"]), Some(Mechanism::XOAuth2));
    // Mechanism names are protocol atoms, so case never decides.
    assert_eq!(select(["xoauth2"]), Some(Mechanism::XOAuth2));
    // A password-only server offers neither, and the caller must say so rather than
    // guess a mechanism the server never named.
    assert_eq!(select(["PLAIN", "LOGIN", "CRAM-MD5"]), None);
    assert_eq!(select([]), None);
}

#[test]
fn the_two_real_servers_both_offer_both_and_both_settle_on_the_standard() {
    // Against the **observed** capability lines, not invented ones. This is what the
    // preference order actually rests on: if either server dropped `AUTH=OAUTHBEARER`,
    // the mechanism our live tests exercise would silently become the other one, and
    // this test is what would notice.
    for (server, capability) in [("Gmail", GMAIL_CAPABILITY), ("Yahoo", YAHOO_CAPABILITY)] {
        let offered = mechanisms(capability);
        assert!(
            offered.contains(&"XOAUTH2") && offered.contains(&"OAUTHBEARER"),
            "{server} no longer offers both: {offered:?}"
        );
        assert_eq!(
            select(offered.iter().copied()),
            Some(Mechanism::OAuthBearer),
            "{server} must settle on the mechanism the live tests prove"
        );
        // Both also advertise SASL-IR, so both take the credential inline — the
        // one-round-trip path, not the prompted fallback.
        assert!(
            capability.split_whitespace().any(|atom| atom == "SASL-IR"),
            "{server} no longer advertises SASL-IR"
        );
    }
}

#[test]
fn each_mechanism_acknowledges_an_error_challenge_the_way_its_spec_says() {
    // RFC 7628 §3.2.3: a single %x01. Google's XOAUTH2: an empty line.
    assert_eq!(Mechanism::OAuthBearer.cancel_response(), "AQ==");
    assert_eq!(
        crate::base64::decode(Mechanism::OAuthBearer.cancel_response()).as_deref(),
        Some(&b"\x01"[..])
    );
    assert_eq!(Mechanism::XOAuth2.cancel_response(), "");
}

#[test]
fn a_challenge_is_decoded_into_the_reason_the_token_was_refused() {
    // RFC 7628 §4.3's challenge, verbatim — the only place a server says *why*.
    let challenge = "eyJzdGF0dXMiOiJpbnZhbGlkX3Rva2VuIiwic2NvcGUiOiJleGFtcGxlX3Njb3BlIiwib3Blb\
                     mlkLWNvbmZpZ3VyYXRpb24iOiJodHRwczovL2V4YW1wbGUuY29tLy53ZWxsLWtub3duL29wZW5\
                     pZC1jb25maWd1cmF0aW9uIn0=";
    let described = describe_challenge(challenge);
    assert!(
        described.contains("invalid_token") && described.contains("example_scope"),
        "challenge not decoded: {described}"
    );
}

#[test]
fn a_hostile_challenge_cannot_forge_a_log_line_or_grow_without_bound() {
    // Control characters are flattened, so a decoded challenge cannot inject newlines
    // into whatever the host writes the error to…
    let framed = crate::base64::encode(b"one\r\nCRITICAL: two");
    let described = describe_challenge(&framed);
    assert!(!described.contains('\n') && !described.contains('\r'));
    assert!(described.contains("CRITICAL: two"));

    // …and the text a server controls is capped.
    let huge = crate::base64::encode(&vec![b'x'; 4096]);
    assert_eq!(describe_challenge(&huge).len(), MAX_CHALLENGE_DETAIL);

    // Undecodable base64 is reported verbatim rather than swallowed: a server that
    // answers with prose is still telling us something.
    assert_eq!(describe_challenge("not base64!"), "not base64!");
    // An empty challenge contributes nothing.
    assert_eq!(describe_challenge("   "), "");
}
