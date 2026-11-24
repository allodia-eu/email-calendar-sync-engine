//! Responses as the two harness servers sent them (`tests/fixtures/managesieve/`), read line by
//! line as the session reads them.

use tokio::io::{AsyncBufReadExt, BufReader};

use super::*;
use crate::transport_read::read_framed_line;

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("../../tests/fixtures/managesieve/", $name, ".txt")).as_slice()
    };
}

/// Every logical line of `bytes`, parsed.
async fn lines(bytes: &[u8]) -> Vec<Line> {
    let mut reader = BufReader::new(bytes);
    let mut out = Vec::new();
    while !reader.fill_buf().await.unwrap().is_empty() {
        let line = read_framed_line(&mut reader, None, 1 << 20).await.unwrap();
        out.push(parse_line(&line).unwrap());
    }
    out
}

/// The data lines and completion of one response.
async fn response(bytes: &[u8]) -> (Vec<Vec<Token>>, Completion) {
    let mut all = lines(bytes).await;
    let Some(Line::Completion(completion)) = all.pop() else {
        panic!("a response ends in a completion");
    };
    let data = all
        .into_iter()
        .map(|line| match line {
            Line::Data(tokens) => tokens,
            Line::Completion(_) => panic!("one completion per response"),
        })
        .collect();
    (data, completion)
}

async fn capabilities(bytes: &[u8]) -> Capabilities {
    let (data, completion) = response(bytes).await;
    assert_eq!(completion.status, Status::Ok);
    Capabilities::from_lines(&data).unwrap()
}

#[tokio::test]
async fn before_tls_both_servers_offer_starttls_and_no_mechanism() {
    let stalwart = capabilities(fixture!("stalwart_greeting")).await;
    assert!(stalwart.starttls);
    assert!(stalwart.sasl.is_empty(), "SASL \"\" before TLS");
    assert!(stalwart.lists("editheader") && stalwart.lists("VACATION"));
    assert_eq!(stalwart.max_redirects, Some(1));

    let dovecot = capabilities(fixture!("dovecot_greeting")).await;
    assert!(dovecot.starttls && dovecot.sasl.is_empty());
    assert!(dovecot.lists("vacation") && !dovecot.lists("editheader"));
    assert_eq!(
        dovecot.max_redirects, None,
        "Dovecot says only once signed in"
    );
}

#[tokio::test]
async fn over_tls_the_mechanisms_are_offered_and_starttls_is_not() {
    let stalwart = capabilities(fixture!("stalwart_greeting_tls")).await;
    assert_eq!(stalwart.sasl, ["PLAIN", "OAUTHBEARER", "XOAUTH2"]);
    assert!(!stalwart.starttls);
    let dovecot = capabilities(fixture!("dovecot_greeting_tls")).await;
    assert_eq!(dovecot.sasl, ["PLAIN"]);
    let signed_in = capabilities(fixture!("dovecot_capability_authenticated")).await;
    assert_eq!(signed_in.max_redirects, Some(4));
    assert_eq!(
        capabilities(fixture!("stalwart_capability_authenticated"))
            .await
            .max_redirects,
        Some(1)
    );
}

#[tokio::test]
async fn a_refusal_keeps_its_code_and_drops_the_server_s_words() {
    for (bytes, code) in [
        (
            fixture!("stalwart_getscript_nonexistent"),
            Some(ResponseCode::Nonexistent),
        ),
        (
            fixture!("dovecot_getscript_nonexistent"),
            Some(ResponseCode::Nonexistent),
        ),
        (
            fixture!("stalwart_deletescript_active"),
            Some(ResponseCode::Active),
        ),
        (
            fixture!("dovecot_deletescript_active"),
            Some(ResponseCode::Active),
        ),
        (
            fixture!("dovecot_havespace_large"),
            Some(ResponseCode::Quota(Quota::MaxSize)),
        ),
        // Dovecot's account of an invalid script is a literal naming its lines; Stalwart's a
        // quoted string. Neither carries a code.
        (fixture!("dovecot_checkscript_unknown_extension"), None),
        (fixture!("stalwart_checkscript_unknown_extension"), None),
        (fixture!("stalwart_authenticate_refused"), None),
        (fixture!("dovecot_authenticate_refused"), None),
    ] {
        let (data, completion) = response(bytes).await;
        assert_eq!(data, Vec::<Vec<Token>>::new());
        assert_eq!(
            completion,
            Completion {
                status: Status::No,
                code
            }
        );
    }
    for ok in [
        fixture!("stalwart_havespace_large"),
        fixture!("stalwart_checkscript_editheader"),
        fixture!("dovecot_logout"),
        fixture!("stalwart_listscripts_empty"),
    ] {
        assert_eq!(response(ok).await.1.status, Status::Ok);
    }
}

#[tokio::test]
async fn a_script_list_and_a_script_read_as_strings() {
    for bytes in [
        fixture!("stalwart_listscripts"),
        fixture!("dovecot_listscripts"),
    ] {
        let (data, _) = response(bytes).await;
        assert_eq!(
            data,
            [vec![
                Token::Str("zfixture".into()),
                Token::Atom("ACTIVE".into())
            ]]
        );
    }
    for bytes in [
        fixture!("stalwart_getscript"),
        fixture!("dovecot_getscript"),
    ] {
        let (data, _) = response(bytes).await;
        let [line] = data.as_slice() else {
            panic!("one line")
        };
        let [Token::Str(script)] = line.as_slice() else {
            panic!("one string")
        };
        assert!(script.starts_with("require [\"fileinto\"];\r\n# rule:[Fixture]\r\n"));
        assert_eq!(script.len(), 111);
    }
}

#[test]
fn the_response_codes_read_as_named() {
    for (atom, code) in [
        ("AUTH-TOO-WEAK", ResponseCode::AuthTooWeak),
        ("encrypt-needed", ResponseCode::EncryptNeeded),
        ("QUOTA", ResponseCode::Quota(Quota::Unspecified)),
        ("QUOTA/MAXSCRIPTS", ResponseCode::Quota(Quota::MaxScripts)),
        ("TRANSITION-NEEDED", ResponseCode::TransitionNeeded),
        ("TRYLATER", ResponseCode::TryLater),
        ("ALREADYEXISTS", ResponseCode::AlreadyExists),
        ("WARNINGS", ResponseCode::Warnings),
        ("REFERRAL", ResponseCode::Other),
    ] {
        let line = format!("NO ({atom} \"x\") \"why\"\r\n");
        assert_eq!(
            parse_line(line.as_bytes()).unwrap(),
            Line::Completion(Completion {
                status: Status::No,
                code: Some(code)
            }),
            "{atom}"
        );
        if code != ResponseCode::Other {
            assert!(code.to_string().eq_ignore_ascii_case(atom));
        }
    }
    assert_eq!(
        parse_line(b"BYE \"going\"\r\n").unwrap(),
        Line::Completion(Completion {
            status: Status::Bye,
            code: None
        })
    );
}

#[test]
fn a_line_that_is_not_managesieve_is_refused_without_quoting_it() {
    for bad in [
        b"\"unterminated\r\n".as_slice(),
        b"{5}\r\nab\r\n",
        b"{x}\r\nab\r\n",
        b"{2}ab\r\n",
        b"\"no line end\"",
        b"\"\xff\xfe\"\r\n",
        b"\"escape at the end\\",
        b"NO (\"code\") \"x\"\r\n",
    ] {
        let err = parse_line(bad).unwrap_err();
        assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
    }
    // The prose of a completion may be any bytes: it is never read.
    assert_eq!(
        parse_line(b"NO \"\xff\xfe\"\r\n").unwrap(),
        Line::Completion(Completion {
            status: Status::No,
            code: None
        })
    );
    // An escaped quote and backslash, and a non-synchronizing literal, read as their values.
    assert_eq!(
        parse_line(b"\"a\\\"b\\\\c\" {2+}\r\nxy\r\n").unwrap(),
        Line::Data(vec![Token::Str("a\"b\\c".into()), Token::Str("xy".into())])
    );
}

#[test]
fn a_capability_that_is_not_a_string_or_a_count_that_is_not_a_number_is_refused() {
    assert!(Capabilities::from_lines(&[vec![Token::Atom("SIEVE".into())]]).is_err());
    assert!(
        Capabilities::from_lines(&[vec![
            Token::Str("MAXREDIRECTS".into()),
            Token::Str("many".into())
        ]])
        .is_err()
    );
    // A name this client does not use is skipped.
    let unknown = Capabilities::from_lines(&[vec![
        Token::Str("NOTIFY".into()),
        Token::Str("mailto".into()),
    ]])
    .unwrap();
    assert_eq!(unknown, Capabilities::default());
}

#[tokio::test]
async fn only_a_crlf_announces_a_literal_in_both_readers() {
    // `{3}` then a bare LF is no literal to the framed reader, and none to the tokenizer either.
    let mut reader = BufReader::new(b"{3}\nabc\r\n".as_slice());
    let line = read_framed_line(&mut reader, None, 1 << 20).await.unwrap();
    assert_eq!(line, b"{3}\n");
    assert!(parse_line(&line).is_err());
    let mut reader = BufReader::new(b"{3}\r\nabc\r\n".as_slice());
    let line = read_framed_line(&mut reader, None, 1 << 20).await.unwrap();
    assert_eq!(line, b"{3}\r\nabc\r\n");
    assert_eq!(
        parse_line(&line).unwrap(),
        Line::Data(vec![Token::Str("abc".into())])
    );
}
