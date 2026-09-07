//! Offline tests for the IMAP `AUTHENTICATE` SASL exchange, over a scripted mock
//! stream.
//!
//! The mock serves canned bytes regardless of what it is sent, so what these pin is
//! the **shape the client emits** — the mechanism it picks from the advertised set,
//! whether it uses an initial response, and the acknowledgement it sends when a server
//! answers with an error challenge. Whether a real server accepts those bytes is the
//! gated live test's job (`tests/live_imap_oauth.rs`), because a mock cannot fail on a
//! wrong request.

use crate::{
    error::ImapError,
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] Stalwart ready\r\n";

/// RFC 7628 §4.3's error challenge (`{"status":"invalid_token",…}`), verbatim.
const CHALLENGE: &str =
    "+ eyJzdGF0dXMiOiJpbnZhbGlkX3Rva2VuIiwic2NvcGUiOiJleGFtcGxlX3Njb3BlIn0=\r\n";

/// Drives a token authentication over `server_script`, returning the outcome and the
/// bytes the client wrote.
async fn authenticate(server_script: Vec<u8>) -> (Result<(), ImapError>, String) {
    let (stream, recorded) = MockStream::new(server_script);
    let mut conn = Connection::open(stream).await.expect("greeting");
    let outcome = conn
        .authenticate_oauth2(
            "alice@example.com",
            "ya29.token",
            "imap.example.com",
            Some(993),
        )
        .await;
    (outcome, written(&recorded))
}

#[tokio::test]
async fn a_sasl_ir_server_gets_the_credential_inline_on_the_command() {
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=OAUTHBEARER\r\n",
        "a1 OK CAPABILITY done\r\n",
        "a2 OK alice@example.com authenticated\r\n",
    ]))
    .await;
    outcome.expect("authenticated");

    // One line, mechanism and credential together (RFC 4959) — no second round trip.
    let expected = crate::sasl::Mechanism::OAuthBearer
        .initial_response(
            "alice@example.com",
            "ya29.token",
            "imap.example.com",
            Some(993),
        )
        .expect("clean credential");
    assert!(
        sent.contains(&format!("a2 AUTHENTICATE OAUTHBEARER {expected}\r\n")),
        "{sent}"
    );
    // The password path is never taken: no credential is ever sent as a `LOGIN`.
    assert!(!sent.contains("LOGIN"), "{sent}");
}

#[tokio::test]
async fn a_server_without_sasl_ir_is_prompted_for_the_credential() {
    // No `SASL-IR` capability: an initial response would be a syntax error, so the
    // client names the mechanism, waits for the bare `+`, and sends the blob alone.
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 AUTH=XOAUTH2\r\na1 OK CAPABILITY done\r\n",
        "+\r\n",
        "a2 OK authenticated\r\n",
    ]))
    .await;
    outcome.expect("authenticated");

    assert!(sent.contains("a2 AUTHENTICATE XOAUTH2\r\n"), "{sent}");
    let expected = crate::sasl::Mechanism::XOAuth2
        .initial_response(
            "alice@example.com",
            "ya29.token",
            "imap.example.com",
            Some(993),
        )
        .expect("clean credential");
    assert!(sent.contains(&format!("\r\n{expected}\r\n")), "{sent}");
}

#[tokio::test]
async fn the_standard_mechanism_wins_when_a_server_offers_both() {
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=XOAUTH2 AUTH=OAUTHBEARER\r\n",
        "a1 OK CAPABILITY done\r\n",
        "a2 OK authenticated\r\n",
    ]))
    .await;
    outcome.expect("authenticated");
    assert!(sent.contains("a2 AUTHENTICATE OAUTHBEARER "), "{sent}");
}

#[tokio::test]
async fn a_rejected_token_is_acknowledged_so_the_server_can_report_it() {
    // The trap this covers: a server describes the rejection in a challenge and then
    // waits. A client that does not acknowledge never reads the tagged `NO` — a stale
    // token becomes a hang instead of an authentication error.
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
        CHALLENGE,
        "a2 NO SASL authentication failed\r\n",
    ]))
    .await;

    let err = outcome.expect_err("a rejected token must fail");
    assert!(matches!(err, ImapError::Auth(_)), "{err:?}");
    // The reason the server gave travels with the error — the only place it says
    // whether the token was expired, wrongly scoped, or for another account.
    let detail = err.to_string();
    assert!(detail.contains("invalid_token"), "{detail}");
    assert!(detail.contains("example_scope"), "{detail}");

    // RFC 7628 §3.2.3's acknowledgement: a single %x01, base64 `AQ==`.
    assert!(sent.ends_with("AQ==\r\n"), "{sent}");
}

#[tokio::test]
async fn xoauth2_acknowledges_a_challenge_with_an_empty_line() {
    // Google's mechanism differs from RFC 7628 here, and sending the wrong one leaves
    // the exchange unfinished on servers that are strict about it.
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=XOAUTH2\r\na1 OK CAPABILITY done\r\n",
        CHALLENGE,
        "a2 NO SASL authentication failed\r\n",
    ]))
    .await;
    assert!(matches!(outcome, Err(ImapError::Auth(_))));
    assert!(sent.ends_with("\r\n\r\n"), "{sent:?}");
}

#[tokio::test]
async fn a_server_offering_no_oauth_mechanism_says_what_it_does_offer() {
    // The common misconfiguration — an OAuth credential against a password-only
    // account — and the error is only useful if it names the alternative.
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=LOGIN\r\na1 OK CAPABILITY done\r\n",
    ]))
    .await;

    let err = outcome.expect_err("no mechanism to present");
    assert!(matches!(err, ImapError::Auth(_)), "{err:?}");
    let detail = err.to_string();
    assert!(
        detail.contains("PLAIN") && detail.contains("LOGIN"),
        "{detail}"
    );
    // Nothing was attempted: no token reaches a server that cannot take one.
    assert!(!sent.contains("AUTHENTICATE"), "{sent}");
    assert!(
        !sent.contains("ya29.token"),
        "token must not be sent: {sent}"
    );
}

#[tokio::test]
async fn a_refused_mechanism_is_a_protocol_failure_not_an_auth_one() {
    // A `BAD` means the command was refused, which no fresh token fixes — so it must
    // not be reported as "re-authenticate".
    let (outcome, _sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
        "a2 BAD Unsupported authentication mechanism\r\n",
    ]))
    .await;
    assert!(matches!(outcome, Err(ImapError::Bad(_))), "{outcome:?}");
}

#[tokio::test]
async fn untagged_chatter_during_the_exchange_is_skipped() {
    // RFC 9051 §7 lets a server interleave untagged responses at any point, including
    // between the command and its completion.
    let (outcome, _sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
        "* OK still here\r\n",
        "a2 OK authenticated\r\n",
    ]))
    .await;
    outcome.expect("authenticated despite the chatter");
}

#[tokio::test]
async fn a_server_advertising_no_mechanisms_at_all_still_explains_itself() {
    // The `AUTH=`-less capability list — the message has nothing to name, and must say
    // so rather than trail off after "it offers:".
    let (outcome, _sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 IDLE\r\na1 OK CAPABILITY done\r\n",
    ]))
    .await;
    let err = outcome.expect_err("nothing to present");
    assert!(err.to_string().contains("none"), "{err}");
}

#[tokio::test]
async fn a_mechanism_refused_at_the_prompt_is_reported_rather_than_waited_on() {
    // No SASL-IR, so the client expects a `+`; this server answers the command itself
    // instead. Reading that as a continuation would send the credential into a command
    // that had already failed.
    let (outcome, sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 AUTH=XOAUTH2\r\na1 OK CAPABILITY done\r\n",
        "a2 NO [AUTHENTICATIONFAILED] mechanism not available for this account\r\n",
    ]))
    .await;
    let err = outcome.expect_err("a refusal must fail");
    assert!(matches!(err, ImapError::Auth(_)), "{err:?}");
    // The command was named, but no credential followed the refusal.
    assert!(sent.contains("a2 AUTHENTICATE XOAUTH2\r\n"), "{sent}");
    assert!(!sent.contains("ya29.token"), "{sent}");
}

#[tokio::test]
async fn an_unknown_completion_word_is_a_protocol_error_not_a_success() {
    // Anything that is not OK/NO/BAD is a server this client does not understand —
    // which must never be read as "authenticated".
    let (outcome, _sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
        "a2 MAYBE who knows\r\n",
    ]))
    .await;
    assert!(
        matches!(outcome, Err(ImapError::Protocol(_))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_line_belonging_to_no_other_command_is_refused_rather_than_guessed_at() {
    // A tagged completion for a tag this exchange never issued means the connection has
    // desynced. Treating it as this command's answer would authenticate on someone
    // else's `OK`.
    let (outcome, _sent) = authenticate(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
        "a9 OK a completion for another command\r\n",
    ]))
    .await;
    assert!(
        matches!(outcome, Err(ImapError::Protocol(_))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn an_endless_stream_of_challenges_is_abandoned_rather_than_looped_on() {
    let mut parts = vec![
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
    ];
    // More challenges than the cap allows, and never a completion.
    parts.extend(std::iter::repeat_n(CHALLENGE, 32));
    let (outcome, _sent) = authenticate(script(&parts)).await;
    assert!(
        matches!(outcome, Err(ImapError::Protocol(_))),
        "{outcome:?}"
    );
}
