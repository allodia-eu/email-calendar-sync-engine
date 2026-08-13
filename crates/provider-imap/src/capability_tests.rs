//! The rules that decide what one session may ask a server for: advertised vs enabled,
//! and what IMAP4rev2 supplies without either.

use super::*;

fn caps(list: &[&str]) -> Negotiated {
    Negotiated::from_capabilities(&list.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>())
}

#[test]
fn an_advertised_extension_is_available_without_enabling_it() {
    let session = caps(&["IMAP4rev1", "IDLE", "LIST-STATUS"]);
    assert!(session.has(Extension::Idle));
    assert!(session.has(Extension::ListStatus));
    assert!(!session.has(Extension::SpecialUse));
}

#[test]
fn capability_atoms_match_case_insensitively() {
    // RFC 9051 §6.1.1: capability names are case insensitive.
    assert!(caps(&["idle"]).has(Extension::Idle));
    assert!(
        caps(&["Imap4Rev2"])
            .enable_arguments()
            .contains(&"IMAP4rev2")
    );
}

#[test]
fn advertising_rev2_is_not_speaking_it() {
    // A dual-revision server behaves as rev1 until the client enables rev2, so the
    // capability alone must grant nothing — including the UTF-8 mailbox names that would
    // otherwise leave modified UTF-7 undecoded on the wire.
    let session = caps(&["IMAP4rev2", "IMAP4rev1"]);
    assert!(!session.rev2());
    assert!(!session.has(Extension::Idle));
    assert!(session.names_are_modified_utf7());
}

#[test]
fn enabling_rev2_supplies_every_extension_it_folded_in() {
    let mut session = caps(&["IMAP4rev2", "IMAP4rev1"]);
    session.confirm_enabled(&["IMAP4rev2".to_owned()]);

    assert!(session.rev2());
    // RFC 9051 Appendix E item 2: folded in, so available with no atom of their own.
    assert!(session.has(Extension::Idle));
    assert!(session.has(Extension::ListStatus));
    assert!(session.has(Extension::SpecialUse));
    // …and names stop being modified UTF-7 (§5.1, Appendix E item 16).
    assert!(!session.names_are_modified_utf7());
}

#[test]
fn qresync_is_not_folded_into_rev2_and_still_needs_its_own_enable() {
    // rev2 took only QRESYNC's CLOSED response code (Appendix E item 9); the extension
    // itself remains RFC 7162's, with its own capability and its own ENABLE.
    let mut session = caps(&["IMAP4rev2", "QRESYNC"]);
    session.confirm_enabled(&["IMAP4rev2".to_owned()]);
    assert!(session.rev2());
    assert!(
        !session.has(Extension::Qresync),
        "rev2 does not grant QRESYNC"
    );

    session.confirm_enabled(&["QRESYNC".to_owned()]);
    assert!(session.has(Extension::Qresync));
}

#[test]
fn an_extension_that_needs_enabling_is_not_had_by_advertising_it() {
    // The trap RFC 5161 exists to prevent: the server offers QRESYNC and sends nothing
    // different until the client says so.
    let session = caps(&["QRESYNC"]);
    assert!(!session.has(Extension::Qresync));
    assert_eq!(session.enable_arguments(), ["QRESYNC"]);
}

#[test]
fn a_bare_enabled_response_enables_nothing() {
    let mut session = caps(&["IMAP4rev2", "QRESYNC"]);
    session.confirm_enabled(&[]);
    assert!(!session.rev2());
    assert!(!session.has(Extension::Qresync));
}

#[test]
fn one_enable_carries_the_dialect_and_every_extension_that_needs_one() {
    let session = caps(&["IMAP4rev2", "IMAP4rev1", "QRESYNC", "IDLE", "LIST-STATUS"]);
    // The dialect leads; IDLE and LIST-STATUS need no announcement, so they are absent.
    assert_eq!(session.enable_arguments(), ["IMAP4rev2", "QRESYNC"]);

    // Nothing to enable at all is an empty list, and the caller issues no command.
    assert!(caps(&["IMAP4rev1", "IDLE"]).enable_arguments().is_empty());
}

#[test]
fn special_use_is_asked_for_wherever_it_is_advertised_whatever_the_dialect() {
    // rev1 + the extension: the attributes come only when a return option asks (RFC 5258 §3).
    let rev1 = caps(&["IMAP4rev1", "SPECIAL-USE"]);
    assert!(rev1.must_request_special_use());

    // rev1 without it: nothing to ask with, and nothing that would answer.
    assert!(!caps(&["IMAP4rev1"]).must_request_special_use());

    // rev2 **still asks**, because the dialect is not what decides this. rev2 folds the
    // attributes into base `LIST` data (RFC 9051 §7.3.1) and defines no return option — but
    // a rev2 server that also advertises RFC 6154 keeps RFC 6154's rule, and Dovecot's rev2
    // does exactly that: it strips every role from an extended `LIST` that did not ask.
    // Reading "folded in" as "will arrive unbidden" costs every folder its role there.
    let mut rev2 = caps(&["IMAP4rev2", "SPECIAL-USE"]);
    rev2.confirm_enabled(&["IMAP4rev2".to_owned()]);
    assert!(rev2.must_request_special_use());

    // rev2 that never advertised RFC 6154: `has` is true (rev2 folded the attributes in),
    // yet the return option must **not** be sent — a client may not send an option the
    // server has not advertised, and the server must answer `BAD` (RFC 9051 §6.3.9). So the
    // gate is what was advertised, never what the session may use.
    let mut folded_only = caps(&["IMAP4rev2"]);
    folded_only.confirm_enabled(&["IMAP4rev2".to_owned()]);
    assert!(folded_only.has(Extension::SpecialUse));
    assert!(!folded_only.must_request_special_use());
}
