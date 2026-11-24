//! Tests for reading the schemes a `WWW-Authenticate` value offers.

use super::challenge_schemes;

#[test]
fn a_bare_scheme_is_parsed() {
    assert_eq!(challenge_schemes("Basic"), ["Basic"]);
}

#[test]
fn the_scheme_is_taken_and_its_params_ignored() {
    assert_eq!(challenge_schemes(r#"Basic realm="jmap""#), ["Basic"]);
}

#[test]
fn fastmails_real_challenge_parses_as_bearer() {
    // Captured verbatim from api.fastmail.com's 401 on /jmap/session.
    let header = r#"Bearer resource_metadata="https://api.fastmail.com/.well-known/oauth-protected-resource/jmap/session""#;
    assert_eq!(challenge_schemes(header), ["Bearer"]);
}

#[test]
fn trailing_auth_params_are_not_read_as_schemes() {
    // The comma before `charset` separates params of the *same* challenge, not a new
    // one — the giveaway is the `=`, which a scheme token never contains.
    assert_eq!(
        challenge_schemes(r#"Basic realm="jmap", charset="UTF-8""#),
        ["Basic"]
    );
}

#[test]
fn multiple_challenges_are_all_parsed() {
    assert_eq!(
        challenge_schemes(r#"Basic realm="jmap", Bearer"#),
        ["Basic", "Bearer"]
    );
}

#[test]
fn a_comma_inside_a_quoted_param_does_not_split_the_challenge() {
    // Naive comma-splitting would invent a second challenge here.
    assert_eq!(challenge_schemes(r#"Basic realm="a,b""#), ["Basic"]);
}

#[test]
fn an_escaped_quote_inside_a_param_does_not_end_the_quoted_string() {
    assert_eq!(challenge_schemes(r#"Basic realm="a\",b""#), ["Basic"]);
}

#[test]
fn a_token68_credential_is_not_read_as_a_scheme() {
    // `Negotiate`'s token68 may carry `=` padding; only the leading word is the scheme.
    assert_eq!(
        challenge_schemes("Negotiate YIIFxQYGKwYB, Basic"),
        ["Negotiate", "Basic"]
    );
}

#[test]
fn an_empty_header_offers_nothing() {
    assert_eq!(challenge_schemes(""), [] as [&str; 0]);
}

#[test]
fn the_scheme_keeps_the_servers_spelling() {
    // Schemes are case-insensitive; the caller compares, and an error can name what was sent.
    assert_eq!(
        challenge_schemes("DIGEST realm=\"x\", basic"),
        ["DIGEST", "basic"]
    );
}
