//! The session over scripted servers: what each command sends, and what its answer becomes. The
//! answers are the harness servers' own bytes (`tests/fixtures/managesieve/`).

use super::*;
use crate::{
    managesieve::response::{Quota, ResponseCode},
    mock::{MockStream, Recorded, written},
};

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("../../tests/fixtures/managesieve/", $name, ".txt")).as_slice()
    };
}

/// A session over `answers`, greeted as Stalwart greets once TLS is up.
async fn greeted(answers: &[&[u8]]) -> (Session<MockStream>, Recorded) {
    let mut script = fixture!("stalwart_greeting_tls").to_vec();
    for answer in answers {
        script.extend_from_slice(answer);
    }
    let (stream, recorded) = MockStream::new(script);
    (Session::open(stream).await.unwrap(), recorded)
}

fn name(name: &str) -> ScriptName {
    ScriptName::new(name).unwrap()
}

#[tokio::test]
async fn the_script_list_and_a_script_read_as_the_server_sent_them() {
    for (listing, script) in [
        (
            fixture!("stalwart_listscripts"),
            fixture!("stalwart_getscript"),
        ),
        (
            fixture!("dovecot_listscripts"),
            fixture!("dovecot_getscript"),
        ),
    ] {
        let (mut session, recorded) =
            greeted(&[listing, script, fixture!("stalwart_logout")]).await;
        assert_eq!(
            session.list_scripts().await.unwrap(),
            [ListedScript {
                name: name("zfixture"),
                active: true
            }]
        );
        let text = session.get_script(&name("zfixture")).await.unwrap();
        assert!(
            text.ends_with("\tfileinto \"Archive\";\r\n}\r\n"),
            "{text:?}"
        );
        session.logout().await;
        assert_eq!(
            written(&recorded),
            "LISTSCRIPTS\r\nGETSCRIPT \"zfixture\"\r\nLOGOUT\r\n"
        );
    }
}

#[tokio::test]
async fn a_script_goes_as_a_literal_and_a_name_as_a_quoted_string() {
    let ok: &[u8] = b"OK\r\n";
    let (mut session, recorded) = greeted(&[ok, ok, ok, ok]).await;
    let text = "require [\"fileinto\"];\r\nkeep;\r\n";
    session.put_script(&name("rules"), text).await.unwrap();
    session.check_script(text).await.unwrap();
    session.set_active(Some(&name("rules"))).await.unwrap();
    session.set_active(None).await.unwrap();
    assert_eq!(
        written(&recorded),
        "PUTSCRIPT \"rules\" {30+}\r\nrequire [\"fileinto\"];\r\nkeep;\r\n\r\n\
         CHECKSCRIPT {30+}\r\nrequire [\"fileinto\"];\r\nkeep;\r\n\r\n\
         SETACTIVE \"rules\"\r\nSETACTIVE \"\"\r\n"
    );
    // A name a quoted string cannot hold as it is goes as a literal too.
    let (mut session, recorded) = greeted(&[ok]).await;
    session.delete_script(&name("say \"hi\"")).await.unwrap();
    assert_eq!(written(&recorded), "DELETESCRIPT {8+}\r\nsay \"hi\"\r\n");
}

#[tokio::test]
async fn a_refusal_names_its_command_and_code() {
    let (mut session, _) = greeted(&[
        fixture!("stalwart_getscript_nonexistent"),
        fixture!("dovecot_deletescript_active"),
        fixture!("dovecot_havespace_large"),
        fixture!("dovecot_checkscript_unknown_extension"),
    ])
    .await;
    let refused = session.get_script(&name("znothere")).await.unwrap_err();
    assert!(matches!(
        refused,
        SieveError::Refused {
            command: "GETSCRIPT",
            code: Some(ResponseCode::Nonexistent)
        }
    ));
    let refused = session.delete_script(&name("zfixture")).await.unwrap_err();
    assert_eq!(refused.code(), Some(ResponseCode::Active));
    let refused = session
        .put_script(&name("big"), "keep;\r\n")
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some(ResponseCode::Quota(Quota::MaxSize)));
    // Dovecot's refusal quotes the script's lines; none of it reaches the error.
    let refused = session
        .check_script("require [\"imapsieve\"];\r\n")
        .await
        .unwrap_err();
    let shown = format!("{refused} {refused:?}");
    assert!(
        !shown.contains("imapsieve") && !shown.contains("tmp"),
        "{shown}"
    );
}

#[tokio::test]
async fn a_bye_ends_the_session_as_a_retryable_failure() {
    let (mut session, _) = greeted(&[b"BYE (TRYLATER) \"shutting down\"\r\n"]).await;
    let err = session.list_scripts().await.unwrap_err();
    assert!(
        matches!(err, SieveError::Imap(ImapError::Bye(_))),
        "{err:?}"
    );
    assert!(!err.to_string().contains("shutting"));
}

#[tokio::test]
async fn an_answer_of_the_wrong_shape_is_refused() {
    for (answer, what) in [
        (
            b"\"a\" \"b\"\r\nOK\r\n".as_slice(),
            "a listing with two names",
        ),
        (b"\"a\tb\"\r\nOK\r\n", "a name with a control character"),
    ] {
        let (mut session, _) = greeted(&[answer]).await;
        let err = session.list_scripts().await.unwrap_err();
        assert!(
            matches!(err, SieveError::Imap(ImapError::Protocol(_))),
            "{what}"
        );
    }
    for answer in [
        b"OK\r\n".as_slice(),
        b"\"a\"\r\n\"b\"\r\nOK\r\n",
        b"ATOM\r\nOK\r\n",
    ] {
        let (mut session, _) = greeted(&[answer]).await;
        let err = session.get_script(&name("a")).await.unwrap_err();
        assert!(matches!(err, SieveError::Imap(ImapError::Protocol(_))));
    }
}

#[tokio::test]
async fn starttls_is_asked_only_where_offered_and_nothing_after_it_is_carried_over() {
    // Over TLS the server offers no STARTTLS: asking would be a downgrade's tell.
    let (mut session, recorded) = greeted(&[]).await;
    let err = session.start_tls().await.unwrap_err();
    assert!(matches!(err, SieveError::Imap(ImapError::Protocol(_))));
    assert_eq!(written(&recorded), "", "nothing is sent");

    let mut plain = fixture!("stalwart_greeting").to_vec();
    plain.extend_from_slice(fixture!("stalwart_starttls"));
    let (stream, recorded) = MockStream::new(plain.clone());
    let mut session = Session::open(stream).await.unwrap();
    session.start_tls().await.unwrap();
    assert_eq!(written(&recorded), "STARTTLS\r\n");
    session.into_inner_stream().unwrap();

    // Bytes the server sent past its answer are plaintext an attacker could have injected.
    plain.extend_from_slice(b"\"SASL\" \"PLAIN\"\r\nOK\r\n");
    let (stream, _) = MockStream::new(plain);
    let mut session = Session::open(stream).await.unwrap();
    session.start_tls().await.unwrap();
    assert!(session.into_inner_stream().is_err());
}

#[tokio::test]
async fn a_greeting_that_refuses_or_is_not_managesieve_fails_the_open() {
    let (stream, _) = MockStream::new(b"BYE \"too many\"\r\n".to_vec());
    let err = Session::open(stream).await.unwrap_err();
    assert!(matches!(err, SieveError::Imap(ImapError::Bye(_))));
    // An IMAP greeting, say, at a misconfigured address.
    let (stream, _) = MockStream::new(b"* OK IMAP4rev1 ready\r\n".to_vec());
    let err = Session::open(stream).await.unwrap_err();
    assert!(matches!(err, SieveError::Imap(_)), "{err:?}");
}

#[tokio::test]
async fn capabilities_asked_again_are_kept() {
    let (mut session, recorded) = greeted(&[fixture!("dovecot_capability_authenticated")]).await;
    assert_eq!(session.capabilities().max_redirects, Some(1));
    let refreshed = session.refresh_capabilities().await.unwrap();
    assert_eq!(refreshed.max_redirects, Some(4));
    assert_eq!(session.capabilities().max_redirects, Some(4));
    assert_eq!(written(&recorded), "CAPABILITY\r\n");
    assert!(format!("{session:?}").contains("Session"));
}
