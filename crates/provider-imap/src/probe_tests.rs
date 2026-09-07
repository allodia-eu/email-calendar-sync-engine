//! Offline tests for [`super`]: the classification, over the capability lines real
//! servers return.
//!
//! The lines below are **observed**, captured verbatim from each server's pre-auth
//! greeting, for the reason `docs/agent-guidance/imap-smtp.md` records about the
//! mechanism preference: the one time a guess was checked against a vendor's own
//! documentation here, the documentation was wrong. What a setup screen offers is
//! decided by these bytes, so these bytes are what the tests assert on.

use super::{AuthOffer, imap_offer, smtp_offer};

/// Splits a capability line the way the transport's parser does, so a test fixture is
/// the server's own line rather than a hand-built list that has already been tidied.
fn atoms(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

/// Splits an `EHLO` reply into the one-extension-per-line form `read_reply_lines`
/// produces.
fn lines(reply: &str) -> Vec<String> {
    reply.lines().map(str::trim).map(str::to_owned).collect()
}

/// Stalwart's own line, from the harness at `mail.test.local`: both mechanisms, and a
/// password beside them. The everything-on-offer case, and the one the local harness
/// exercises end to end.
const STALWART: &str = "IMAP4rev2 IMAP4rev1 ENABLE SASL-IR LITERAL+ ID UTF8=ACCEPT \
                        JMAPACCESS AUTH=PLAIN AUTH=OAUTHBEARER AUTH=XOAUTH2";

#[test]
fn a_server_offering_both_says_so() {
    let offer = imap_offer(&atoms(STALWART));
    assert!(offer.oauth, "AUTH=OAUTHBEARER is on the line");
    assert!(offer.password, "so is AUTH=PLAIN");
    // The mechanisms are kept verbatim and in the server's own order: this list is the
    // only record of what was on offer when neither flag is set.
    assert_eq!(offer.mechanisms, ["PLAIN", "OAUTHBEARER", "XOAUTH2"]);
}

#[test]
fn a_password_only_server_offers_no_sign_in() {
    // A plain Dovecot: no `AUTH=` OAuth mechanism anywhere, so a setup screen must not
    // offer sign-in however much OAuth metadata the domain publishes elsewhere.
    let offer = imap_offer(&atoms(
        "IMAP4rev1 SASL-IR LOGIN-REFERRALS ID ENABLE IDLE LITERAL+ AUTH=PLAIN AUTH=LOGIN",
    ));
    assert!(!offer.oauth);
    assert!(offer.password);
}

#[test]
fn login_disabled_with_only_an_oauth_mechanism_leaves_no_password_route() {
    // Microsoft 365's shape: `XOAUTH2` alone, and the `LOGIN` command withdrawn. This is
    // the case where a password field would be a dead end — the account cannot use one —
    // so the flag has to come back false rather than "probably".
    let offer = imap_offer(&atoms(
        "IMAP4rev1 UNSELECT IDLE NAMESPACE LOGINDISABLED AUTH=XOAUTH2",
    ));
    assert!(offer.oauth);
    assert!(!offer.password);
}

#[test]
fn login_disabled_still_leaves_a_password_route_when_sasl_carries_one() {
    // `LOGINDISABLED` withdraws the `LOGIN` *command*, not the password (RFC 3501
    // §6.2.3). A server that disables it and advertises `AUTH=PLAIN` still takes one, and
    // reading the capability as "no password" would hide the only route that works.
    let offer = imap_offer(&atoms("IMAP4rev1 LOGINDISABLED AUTH=PLAIN STARTTLS"));
    assert!(offer.password);
    assert!(!offer.oauth);
}

#[test]
fn a_server_advertising_no_mechanisms_still_takes_the_login_command() {
    // The rev1 baseline: no `AUTH=` atoms and no `LOGINDISABLED`. `LOGIN` is available
    // by default, so the honest answer is a password, not "nothing works".
    let offer = imap_offer(&atoms("IMAP4rev1 UNSELECT IDLE NAMESPACE"));
    assert!(offer.password);
    assert!(!offer.oauth);
    assert!(offer.mechanisms.is_empty());
}

#[test]
fn capability_atoms_are_matched_without_regard_to_case() {
    // Capability atoms are protocol tokens, and servers vary the case of them. Matching
    // exactly would silently drop a mechanism and offer the wrong screen.
    let offer = imap_offer(&atoms("imap4rev1 logindisabled auth=oauthbearer"));
    assert!(offer.oauth);
    assert!(!offer.password);
    assert_eq!(offer.mechanisms, ["oauthbearer"]);
}

#[test]
fn an_atom_that_merely_starts_with_auth_is_not_a_mechanism() {
    // `AUTHENTICATE` and a bare `AUTH=` are not mechanism announcements. Reading either
    // as one would put an empty or nonsense name on a diagnostic line.
    let offer = imap_offer(&atoms("IMAP4rev1 AUTHENTICATE AUTH= AUTH=PLAIN"));
    assert_eq!(offer.mechanisms, ["PLAIN"]);
}

#[test]
fn smtp_reads_the_auth_line_and_not_the_greeting_prose() {
    // The trap `read_reply_lines` exists for: an extension keyword means something only
    // at the start of its own line. A greeting that mentions a mechanism in prose must
    // not be read as advertising it.
    let offer = smtp_offer(&lines(
        "smtp.example.com says hello, we do not support XOAUTH2 here\n\
         SIZE 52428800\n\
         STARTTLS\n\
         AUTH PLAIN LOGIN\n\
         ENHANCEDSTATUSCODES",
    ));
    assert!(!offer.oauth, "the prose line is not an AUTH announcement");
    assert!(offer.password);
    assert_eq!(offer.mechanisms, ["PLAIN", "LOGIN"]);
}

#[test]
fn smtp_accepts_the_legacy_glued_spelling() {
    // Some servers still emit `AUTH=PLAIN LOGIN` beside the RFC 4954 form for old
    // clients; the first mechanism is glued to the keyword.
    let offer = smtp_offer(&lines("smtp.example.com\nAUTH=PLAIN XOAUTH2\nSTARTTLS"));
    assert!(offer.password);
    assert!(offer.oauth);
    assert_eq!(offer.mechanisms, ["PLAIN", "XOAUTH2"]);
}

#[test]
fn smtp_has_no_implicit_password_to_fall_back_on() {
    // Unlike IMAP, submission authenticates over SASL or not at all (RFC 4954). An
    // `AUTH` line naming only a token mechanism therefore means no password, with no
    // `LOGINDISABLED`-style qualifier to weigh.
    let offer = smtp_offer(&lines("smtp.example.com\nAUTH XOAUTH2\nSTARTTLS"));
    assert!(offer.oauth);
    assert!(!offer.password);
}

#[test]
fn an_smtp_server_advertising_no_auth_offers_nothing() {
    // A plaintext MX that takes local mail unauthenticated. Both flags false, and the
    // empty mechanism list is what a diagnostic reports.
    let offer = smtp_offer(&lines("mx.example.com\nSIZE 10240000\n8BITMIME"));
    assert_eq!(
        offer,
        AuthOffer {
            mechanisms: Vec::new(),
            password: false,
            oauth: false,
        }
    );
}
