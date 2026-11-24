//! Unit tests for `WWW-Authenticate` parsing and scheme negotiation.

use super::*;

fn basic() -> Credential {
    Credential::password("alice@example.com", "s3cret")
}

fn bearer() -> Credential {
    Credential::bearer("tok")
}

fn acting_as() -> Credential {
    Credential::acting_as("admin@example.com", "s3cret", "alice@example.com")
}

// ---- negotiation policy ------------------------------------------------------------

#[test]
fn a_bearer_only_challenge_switches_a_basic_credential_to_bearer() {
    // The bug this module exists for: we sent Basic, the server takes only Bearer.
    assert_eq!(
        read_challenge(AuthScheme::Basic, [r#"Bearer realm="jmap""#], &basic()),
        Challenge::Reframe(AuthScheme::Bearer)
    );
}

#[test]
fn a_challenge_offering_the_scheme_we_used_refuses_the_credential() {
    // Stalwart's shape. The credential is simply wrong — retrying under the same scheme
    // would be a pointless second round trip, and switching would hide the real cause. Only
    // a replacement can answer it.
    assert_eq!(
        read_challenge(AuthScheme::Basic, [r#"Basic realm="jmap""#], &basic()),
        Challenge::Refused
    );
}

#[test]
fn no_challenge_at_all_refuses_the_credential() {
    // Nothing to re-frame to, and the server judged the credential all the same (YAHOO-1).
    assert_eq!(
        read_challenge(AuthScheme::Basic, [], &basic()),
        Challenge::Refused
    );
    assert_eq!(
        read_challenge(AuthScheme::Basic, [""], &basic()),
        Challenge::Refused
    );
}

#[test]
fn a_basic_only_challenge_cannot_rescue_a_bearer_only_credential() {
    // A bare token has no username to build a Basic header from, so the 401 stands.
    assert_eq!(
        read_challenge(AuthScheme::Bearer, [r#"Basic realm="jmap""#], &bearer()),
        Challenge::Unusable
    );
}

#[test]
fn a_basic_only_challenge_switches_a_basic_credential_back_to_basic() {
    // The mirror case: latched onto Bearer, then a server that wants Basic.
    assert_eq!(
        read_challenge(AuthScheme::Bearer, [r#"Basic realm="jmap""#], &basic()),
        Challenge::Reframe(AuthScheme::Basic)
    );
}

#[test]
fn bearer_wins_when_both_are_offered_and_neither_was_used() {
    // RFC 8620 §8.2 marks Basic NOT RECOMMENDED, so it is the fallback, not the pick.
    assert_eq!(
        read_challenge(AuthScheme::Bearer, ["Basic, Bearer"], &basic()),
        Challenge::Refused,
        "bearer was already used and is offered — nothing to switch"
    );
    assert_eq!(
        read_challenge(AuthScheme::Basic, ["Digest, Bearer, Basic"], &basic()),
        Challenge::Refused,
        "basic was used and is offered — nothing to switch"
    );
}

#[test]
fn an_unsupported_scheme_only_challenge_leaves_the_401_standing() {
    // We cannot present Digest; reporting the server's 401 honestly beats guessing.
    assert_eq!(
        read_challenge(AuthScheme::Basic, [r#"Digest realm="jmap""#], &basic()),
        Challenge::Unusable
    );
}

#[test]
fn the_scheme_token_match_is_case_insensitive() {
    // RFC 9110 §11.6.1: the scheme token is case-insensitive.
    assert_eq!(
        read_challenge(AuthScheme::Basic, ["BEARER"], &basic()),
        Challenge::Reframe(AuthScheme::Bearer)
    );
}

#[test]
fn challenges_split_across_several_headers_are_all_considered() {
    // A server may send one `WWW-Authenticate` header per challenge rather than one
    // comma-joined value; both spellings are equivalent.
    assert_eq!(
        read_challenge(AuthScheme::Basic, ["Digest", "Bearer"], &basic()),
        Challenge::Reframe(AuthScheme::Bearer)
    );
}

// ---- credential capabilities -------------------------------------------------------

#[test]
fn a_password_can_be_framed_either_way_and_a_token_only_as_bearer() {
    assert!(can_present(&basic(), AuthScheme::Basic));
    assert!(can_present(&basic(), AuthScheme::Bearer));
    assert!(can_present(&bearer(), AuthScheme::Bearer));
    assert!(!can_present(&bearer(), AuthScheme::Basic));
}

#[test]
fn an_administrator_acting_as_another_account_cannot_be_presented_over_jmap() {
    // HTTP has no authorization identity: framing the administrator's password alone would
    // open the administrator's own account.
    assert!(!can_present(&acting_as(), AuthScheme::Basic));
    assert!(!can_present(&acting_as(), AuthScheme::Bearer));
    assert_eq!(natural_scheme(&acting_as()), None);
    assert_eq!(NegotiatedScheme::default().scheme_for(&acting_as()), None);
    assert_eq!(
        read_challenge(AuthScheme::Basic, ["Bearer"], &acting_as()),
        Challenge::Unusable
    );
}

#[test]
fn the_natural_scheme_follows_the_credential_shape() {
    assert_eq!(natural_scheme(&basic()), Some(AuthScheme::Basic));
    assert_eq!(natural_scheme(&bearer()), Some(AuthScheme::Bearer));
}

#[test]
fn a_latched_scheme_applies_to_every_credential_that_can_be_framed_under_it() {
    let cell = NegotiatedScheme::default();
    assert_eq!(cell.latched(), None);
    assert_eq!(cell.scheme_for(&basic()), Some(AuthScheme::Basic));
    cell.set(AuthScheme::Bearer);
    assert_eq!(cell.latched(), Some(AuthScheme::Bearer));
    assert_eq!(cell.scheme_for(&basic()), Some(AuthScheme::Bearer));
    cell.set(AuthScheme::Basic);
    assert_eq!(
        cell.scheme_for(&bearer()),
        Some(AuthScheme::Bearer),
        "a token stays a bearer token whatever the server said to a password"
    );
}
