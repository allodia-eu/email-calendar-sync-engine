//! The out-of-office reply over a scripted ManageSieve server.

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    time::UtcDateTime,
    vacation::{AutoReply, VacationWindow},
};

use super::*;
use crate::managesieve::test_support::{
    STALWART_REPLY, capability, listing, script, scripted, sent,
};

const LOGOUT: &[u8] = b"OK\r\n";
const EXTENSIONS: &str = "fileinto vacation date relational";
const REPLY: &str = "require [\"vacation\"];\r\n# rule:[Out-of-Office]\r\nif true\r\n{\r\n\
                     \tvacation :subject \"Away\" \"Away for now.\";\r\n}\r\n";
const RULE: &str = "# rule:[Fixture]\r\nif header :contains \"subject\" \"x\"\r\n{\r\n\
                    \tfileinto \"Archive\";\r\n}\r\n";

fn account() -> AccountId {
    AccountId::try_from("sieve").unwrap()
}

fn away() -> VacationResponse {
    VacationResponse::default()
        .with_reply(AutoReply::default().with_text("Back soon."))
        .turned_on()
}

async fn read_reply_of(answers: &[&[u8]]) -> ProviderResult<VacationResponse> {
    let (sieve, _) = scripted(answers);
    sieve.read_vacation(&account()).await
}

#[tokio::test]
async fn a_reply_is_on_where_its_rule_is_on_and_its_script_runs() {
    let on = read_reply_of(&[&listing(&[("rules", true)]), &script(REPLY), LOGOUT])
        .await
        .unwrap();
    assert!(on.enabled && on.version.is_some());
    assert_eq!(on.reply.subject.as_deref(), Some("Away"));
    let stored = read_reply_of(&[&listing(&[("rules", false)]), &script(REPLY), LOGOUT])
        .await
        .unwrap();
    assert!(!stored.enabled, "a script that does not run sends no reply");
    let none = read_reply_of(&[&listing(&[]), LOGOUT]).await.unwrap();
    assert!(!none.enabled && none.version.is_some());

    let unread = format!(
        "{}\tredirect \"x@example.com\";\r\n}}\r\n",
        &REPLY[..REPLY.len() - 3]
    );
    let err = read_reply_of(&[&listing(&[("rules", true)]), &script(&unread), LOGOUT])
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
}

#[tokio::test]
async fn the_vocabulary_has_a_window_only_where_the_server_lists_its_extensions() {
    let (sieve, _) = scripted(&[&capability(EXTENSIONS), LOGOUT]);
    let with = Vacation::vocabulary(&sieve, &account()).await.unwrap();
    assert_eq!(with.window(), WindowSupport::EitherEnd);
    assert!(with.has_subject());
    let (sieve, _) = scripted(&[&capability("vacation"), LOGOUT]);
    let without = Vacation::vocabulary(&sieve, &account()).await.unwrap();
    assert_eq!(without.window(), WindowSupport::None);
    assert_eq!(sieve.vacation_write_guard(), WriteGuard::Absent);
}

#[tokio::test]
async fn a_first_reply_creates_the_script_and_switches_it_on() {
    let (sieve, sessions) = scripted(&[
        &capability(EXTENSIONS),
        &listing(&[]),
        b"OK\r\n",
        b"OK\r\n",
        &listing(&[("rules", true)]),
        &script(REPLY),
        LOGOUT,
    ]);
    let receipt = sieve
        .set_vacation(&account(), VacationPrecondition::Unconditional, &away())
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert!(receipt.version.is_some());
    let sent = sent(&sessions);
    assert!(sent.contains("PUTSCRIPT \"rules\" {"), "{sent}");
    assert!(sent.contains("\tvacation \"Back soon.\";\r\n"), "{sent}");
    assert!(sent.contains("SETACTIVE \"rules\""), "{sent}");
}

#[tokio::test]
async fn turning_the_reply_on_never_switches_on_rules_that_are_off() {
    let stored = format!("{REPLY}{RULE}").replace("[\"vacation\"]", "[\"fileinto\",\"vacation\"]");
    let (sieve, sessions) = scripted(&[
        &capability(EXTENSIONS),
        &listing(&[("rules", false)]),
        &script(&stored),
        LOGOUT,
    ]);
    let err = sieve
        .set_vacation(&account(), VacationPrecondition::Unconditional, &away())
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(!sent(&sessions).contains("PUTSCRIPT"));

    // A stored script holding no enabled rule is switched on with the reply.
    let (sieve, sessions) = scripted(&[
        &capability(EXTENSIONS),
        &listing(&[("rules", false)]),
        &script(REPLY),
        b"OK\r\n",
        b"OK\r\n",
        &listing(&[("rules", true)]),
        &script(REPLY),
        LOGOUT,
    ]);
    let _ = sieve
        .set_vacation(&account(), VacationPrecondition::Unconditional, &away())
        .await
        .unwrap();
    assert!(sent(&sessions).contains("SETACTIVE \"rules\""));
}

#[tokio::test]
async fn a_guarded_write_is_refused_when_the_script_moved_or_its_reply_is_unread() {
    let read = read_reply_of(&[&listing(&[("rules", true)]), &script(REPLY), LOGOUT])
        .await
        .unwrap();
    let moved = REPLY.replace("Away for now.", "Elsewhere.");
    let (sieve, sessions) = scripted(&[
        &capability(EXTENSIONS),
        &listing(&[("rules", true)]),
        &script(&moved),
        LOGOUT,
    ]);
    let err = sieve
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read),
            &away(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(!sent(&sessions).contains("PUTSCRIPT"));

    // An unread reply rule is replaced only by an unconditional write.
    let unread = format!(
        "{}\tredirect \"x@example.com\";\r\n}}\r\n",
        &REPLY[..REPLY.len() - 3]
    );
    let built = VacationResponse::default();
    let (sieve, _) = scripted(&[
        &capability(EXTENSIONS),
        &listing(&[("rules", true)]),
        &script(&unread),
        LOGOUT,
    ]);
    let err = sieve
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&built),
            &away(),
        )
        .await
        .unwrap_err();
    // A reply built, not read, has no version to guard by: refused, never written unseen.
    assert_eq!(err.class(), FailureClass::Permanent, "{err:?}");
    let (sieve, sessions) = scripted(&[
        &capability(EXTENSIONS),
        &listing(&[("rules", true)]),
        &script(&unread),
        b"OK\r\n",
        &listing(&[("rules", true)]),
        &script(&unread),
        LOGOUT,
    ]);
    let _ = sieve
        .set_vacation(&account(), VacationPrecondition::Unconditional, &away())
        .await
        .unwrap();
    let sent = sent(&sessions);
    assert!(
        sent.contains("\tvacation \"Back soon.\";\r\n\tredirect \"x@example.com\";"),
        "the replaced rule keeps the action after `vacation`: {sent}"
    );
}

#[tokio::test]
async fn a_reply_that_may_run_from_elsewhere_is_neither_read_nor_written() {
    // A JMAP-managed account: Stalwart's reply script running, or the rules including it.
    let including = format!("include :personal :optional \"vacation\";\r\n{RULE}");
    for (name, text) in [
        ("vacation", STALWART_REPLY.to_owned()),
        ("rules", including),
    ] {
        assert!(super::reply_elsewhere(&text), "{name}");
        let err = read_reply_of(&[&listing(&[(name, true)]), &script(&text), LOGOUT])
            .await
            .unwrap_err();
        assert!(err.is_item_unavailable(), "{name}: {err:?}");
        for reply in [away(), away().turned_off()] {
            let (sieve, sessions) = scripted(&[
                &capability(EXTENSIONS),
                &listing(&[(name, true)]),
                &script(&text),
                LOGOUT,
            ]);
            let err = sieve
                .set_vacation(&account(), VacationPrecondition::Unconditional, &reply)
                .await
                .unwrap_err();
            assert!(err.is_item_unavailable(), "{name}: {err:?}");
            assert!(!sent(&sessions).contains("PUTSCRIPT"), "{name}");
        }
    }
    assert!(!super::reply_elsewhere(REPLY), "the reply rule alone");
}

#[tokio::test]
async fn a_reply_the_account_cannot_hold_is_refused_before_anything_is_stored() {
    let html = VacationResponse::default()
        .with_reply(AutoReply::default().with_html("<p>Away</p>"))
        .turned_on();
    let window = away().with_window(
        VacationWindow::default()
            .with_start("2030-01-02T09:00:00Z".parse::<UtcDateTime>().unwrap()),
    );
    for (extensions, reply) in [(EXTENSIONS, html), ("vacation", window)] {
        let (sieve, sessions) = scripted(&[&capability(extensions), &listing(&[]), LOGOUT]);
        let err = sieve
            .set_vacation(&account(), VacationPrecondition::Unconditional, &reply)
            .await
            .unwrap_err();
        assert!(err.is_item_unavailable(), "{err:?}");
        assert!(!err.to_string().contains("Away"));
        assert!(!sent(&sessions).contains("PUTSCRIPT"));
    }
}
