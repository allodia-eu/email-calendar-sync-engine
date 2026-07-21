//! Unit tests for `WWW-Authenticate` parsing and scheme negotiation.

use super::*;

fn basic() -> Credentials {
    Credentials::basic("alice@example.com", "s3cret")
}

fn bearer() -> Credentials {
    Credentials::bearer("tok")
}

// ---- challenge parsing -------------------------------------------------------------

#[test]
fn a_bare_scheme_is_parsed() {
    assert_eq!(challenge_schemes("Basic"), ["basic"]);
}

#[test]
fn the_scheme_is_taken_and_its_params_ignored() {
    assert_eq!(challenge_schemes(r#"Basic realm="jmap""#), ["basic"]);
}

#[test]
fn fastmails_real_challenge_parses_as_bearer() {
    // Captured verbatim from api.fastmail.com's 401 on /jmap/session.
    let header = r#"Bearer resource_metadata="https://api.fastmail.com/.well-known/oauth-protected-resource/jmap/session""#;
    assert_eq!(challenge_schemes(header), ["bearer"]);
}

#[test]
fn trailing_auth_params_are_not_read_as_schemes() {
    // The comma before `charset` separates params of the *same* challenge, not a new
    // one — the giveaway is the `=`, which a scheme token never contains.
    assert_eq!(
        challenge_schemes(r#"Basic realm="jmap", charset="UTF-8""#),
        ["basic"]
    );
}

#[test]
fn multiple_challenges_are_all_parsed() {
    assert_eq!(
        challenge_schemes(r#"Basic realm="jmap", Bearer"#),
        ["basic", "bearer"]
    );
}

#[test]
fn a_comma_inside_a_quoted_param_does_not_split_the_challenge() {
    // Naive comma-splitting would invent a second challenge here.
    assert_eq!(challenge_schemes(r#"Basic realm="a,b""#), ["basic"]);
}

#[test]
fn an_escaped_quote_inside_a_param_does_not_end_the_quoted_string() {
    assert_eq!(challenge_schemes(r#"Basic realm="a\",b""#), ["basic"]);
}

#[test]
fn a_token68_credential_is_not_read_as_a_scheme() {
    // `Negotiate`'s token68 may carry `=` padding; only the leading word is the scheme.
    assert_eq!(
        challenge_schemes("Negotiate YIIFxQYGKwYB, Basic"),
        ["negotiate", "basic"]
    );
}

#[test]
fn an_empty_header_offers_nothing() {
    assert!(challenge_schemes("").is_empty());
}

// ---- negotiation policy ------------------------------------------------------------

#[test]
fn a_bearer_only_challenge_switches_a_basic_credential_to_bearer() {
    // The bug this module exists for: we sent Basic, the server takes only Bearer.
    assert_eq!(
        negotiate(AuthScheme::Basic, [r#"Bearer realm="jmap""#], &basic()),
        Some(AuthScheme::Bearer)
    );
}

#[test]
fn a_challenge_offering_the_scheme_we_used_is_left_alone() {
    // Stalwart's shape. The credential is simply wrong — retrying under the same scheme
    // would be a pointless second round trip, and switching would hide the real cause.
    assert_eq!(
        negotiate(AuthScheme::Basic, [r#"Basic realm="jmap""#], &basic()),
        None
    );
}

#[test]
fn no_challenge_at_all_leaves_the_401_standing() {
    assert_eq!(negotiate(AuthScheme::Basic, [], &basic()), None);
    assert_eq!(negotiate(AuthScheme::Basic, [""], &basic()), None);
}

#[test]
fn a_basic_only_challenge_cannot_rescue_a_bearer_only_credential() {
    // A bare token has no username to build a Basic header from, so the 401 stands.
    assert_eq!(
        negotiate(AuthScheme::Bearer, [r#"Basic realm="jmap""#], &bearer()),
        None
    );
}

#[test]
fn a_basic_only_challenge_switches_a_basic_credential_back_to_basic() {
    // The mirror case: latched onto Bearer, then a server that wants Basic.
    assert_eq!(
        negotiate(AuthScheme::Bearer, [r#"Basic realm="jmap""#], &basic()),
        Some(AuthScheme::Basic)
    );
}

#[test]
fn bearer_wins_when_both_are_offered_and_neither_was_used() {
    // RFC 8620 §8.2 marks Basic NOT RECOMMENDED, so it is the fallback, not the pick.
    assert_eq!(
        negotiate(AuthScheme::Bearer, ["Basic, Bearer"], &basic()),
        None,
        "bearer was already used and is offered — nothing to switch"
    );
    assert_eq!(
        negotiate(AuthScheme::Basic, ["Digest, Bearer, Basic"], &basic()),
        None,
        "basic was used and is offered — nothing to switch"
    );
}

#[test]
fn an_unsupported_scheme_only_challenge_leaves_the_401_standing() {
    // We cannot present Digest; reporting the server's 401 honestly beats guessing.
    assert_eq!(
        negotiate(AuthScheme::Basic, [r#"Digest realm="jmap""#], &basic()),
        None
    );
}

#[test]
fn the_scheme_token_match_is_case_insensitive() {
    // RFC 9110 §11.6.1: the scheme token is case-insensitive.
    assert_eq!(
        negotiate(AuthScheme::Basic, ["BEARER"], &basic()),
        Some(AuthScheme::Bearer)
    );
}

#[test]
fn challenges_split_across_several_headers_are_all_considered() {
    // A server may send one `WWW-Authenticate` header per challenge rather than one
    // comma-joined value; both spellings are equivalent.
    assert_eq!(
        negotiate(AuthScheme::Basic, ["Digest", "Bearer"], &basic()),
        Some(AuthScheme::Bearer)
    );
}

// ---- credential capabilities -------------------------------------------------------

#[test]
fn a_basic_credential_can_present_either_scheme_and_a_bearer_one_cannot() {
    assert!(basic().can_present(AuthScheme::Basic));
    assert!(basic().can_present(AuthScheme::Bearer));
    assert!(bearer().can_present(AuthScheme::Bearer));
    assert!(!bearer().can_present(AuthScheme::Basic));
}

#[test]
fn the_bearer_secret_is_the_password_of_a_basic_credential() {
    // Re-framing must send the same secret, not the `username:password` pair.
    assert_eq!(basic().bearer_secret(), "s3cret");
    assert_eq!(bearer().bearer_secret(), "tok");
}

#[test]
fn the_preferred_scheme_follows_the_credential_shape() {
    assert_eq!(basic().preferred_scheme(), AuthScheme::Basic);
    assert_eq!(bearer().preferred_scheme(), AuthScheme::Bearer);
}

#[test]
fn a_latched_scheme_is_readable_and_replaceable() {
    let cell = NegotiatedScheme::new(AuthScheme::Basic);
    assert_eq!(cell.get(), AuthScheme::Basic);
    cell.set(AuthScheme::Bearer);
    assert_eq!(cell.get(), AuthScheme::Bearer);
}
