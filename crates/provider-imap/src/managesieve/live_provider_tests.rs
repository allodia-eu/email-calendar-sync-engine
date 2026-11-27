//! Gated live: the rules and the out-of-office reply over ManageSieve, end to end through
//! [`ImapAccount::connect`] and the provider's `mail_rules()` and `vacation()`, on both harness
//! servers (Stalwart as the scratch account `carol@`, Dovecot as `alice@`).
//!
//! Every test takes the module's lock (`ONE_AT_A_TIME`), because they share each server's account,
//! and starts and ends with the account holding no script at all, whatever an earlier run left.
//! Skips per server, saying so, where an address variable is unset, and on Stalwart where the
//! harness gate (`STALWART_HTTP_ADDR`) is unset too.

use std::{
    fmt::Write as _,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::Importance,
    rules::{
        Action, Condition, HeaderField, Kept, MailRule, MailboxPath, Match, RuleMailbox, RuleName,
        RuleSet, RulesStored, TextOp, TextTest,
    },
    time::UtcDateTime,
    vacation::{AutoReply, VacationResponse, VacationWindow},
};
use engine_provider::{ConnectStep, Credential, VacationPrecondition};
use futures_util::FutureExt as _;

use super::{
    dial,
    live_tests::{ONE_AT_A_TIME, address, clean, config},
    name::ScriptName,
    response::ResponseCode,
};
use crate::{ImapAccount, ImapConfig};

/// A harness server: label, IMAP and ManageSieve address variables, account, password.
pub(super) const SERVERS: [(&str, &str, &str, &str, &str); 2] = [
    (
        "stalwart",
        "STALWART_IMAP_ADDR",
        "STALWART_MANAGESIEVE_ADDR",
        "carol@test.local",
        "harness-carol-pw",
    ),
    (
        "dovecot",
        "DOVECOT_REV1_IMAP_ADDR",
        "DOVECOT_MANAGESIEVE_ADDR",
        "alice@test.local",
        "dovecot-alice-pw",
    ),
];

pub(super) fn connector() -> tokio_rustls::TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

pub(super) fn subject_rule(marker: &str, actions: Vec<Action>) -> MailRule {
    let test = TextTest::new(TextOp::Contains, vec![marker.to_owned()]).unwrap();
    MailRule::typed(
        Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test,
        }]),
        actions,
    )
    .named(RuleName::new(format!("Live {marker}")).unwrap())
}

pub(super) fn unplaced(set: &RuleSet) -> Vec<MailRule> {
    set.rules
        .iter()
        .cloned()
        .map(|mut rule| {
            rule.origin = None;
            rule
        })
        .collect()
}

#[tokio::test]
async fn rules_and_the_reply_round_trip_guarded_on_both_servers() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (label, imap_var, sieve_var, user, password) in SERVERS {
        let (Some(imap), Some(sieve)) = (address(label, imap_var), address(label, sieve_var))
        else {
            continue;
        };
        // From nothing, and back to nothing however the run ends.
        let admin_config = config(&sieve, user, password);
        let endpoint = admin_config.managesieve.clone().unwrap();
        let mut admin = dial::connect(&admin_config, &endpoint, &connector())
            .await
            .unwrap();
        clean(&mut admin).await;
        let outcome = AssertUnwindSafe(Box::pin(run(label, &imap, &sieve, user, password)))
            .catch_unwind()
            .await;
        clean(&mut admin).await;
        admin.logout().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}

async fn run(label: &str, imap: &str, sieve: &str, user: &str, password: &str) {
    let marker = format!(
        "zlive-rules-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let steps: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&steps);
    let config = ImapConfig::new(imap, "localhost", Credential::password(user, password))
        .with_managesieve(sieve, "localhost")
        .with_connect_observer(Arc::new(move |step: &ConnectStep<'_>| {
            if let ConnectStep::Negotiated { dialect, .. } = step {
                seen.lock().unwrap().push((*dialect).to_owned());
            }
        }));
    let account = ImapAccount::connect(&config, connector()).await.unwrap();
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let rules = engine_provider::Provider::mail_rules(&provider).expect("rules offered");
    let reply = engine_provider::Provider::vacation(&provider).expect("reply offered");
    let id = AccountId::try_from("live-managesieve").unwrap();
    // The probe lands after the connect returned: a call waits for it, and the observer hears of
    // it as it lands.
    rules.rule_vocabulary(&id).await.unwrap();
    let negotiated = || {
        steps
            .lock()
            .unwrap()
            .iter()
            .any(|step| step == "ManageSieve")
    };
    for _ in 0..50 {
        if negotiated() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(negotiated(), "{label}");

    let mut admin = dial::connect(&config, config.managesieve.as_ref().unwrap(), &connector())
        .await
        .unwrap();
    let ran = admin
        .list_scripts()
        .await
        .unwrap()
        .into_iter()
        .any(|script| script.active);

    // A rule written is read back typed, and the write's version is the read's.
    let empty = rules.read_rules(&id).await.unwrap();
    let archive = RuleMailbox::Path(MailboxPath::new("Archive").unwrap());
    let filed = subject_rule(
        &marker,
        vec![Action::FileInto {
            mailbox: archive,
            copy: false,
        }],
    );
    let written = rules
        .replace_rules(&id, &empty, std::slice::from_ref(&filed))
        .await
        .unwrap();
    let created = written.stored == RulesStored::CreatedRunning;
    assert!(created || ran, "{label}: {:?}", written.stored);
    let read = rules.read_rules(&id).await.unwrap();
    assert_eq!(unplaced(&read), std::slice::from_ref(&filed), "{label}");
    assert_eq!(read.version, written.version, "{label}");

    // A stale version is refused, and the current one accepted.
    let stale = rules
        .replace_rules(&id, &empty, std::slice::from_ref(&filed))
        .await
        .unwrap_err();
    assert_eq!(stale.class(), FailureClass::Conflict, "{label}: {stale:?}");
    let _ = rules.replace_rules(&id, &read, &read.rules).await.unwrap();

    // An extension the server lacks: refused by the plan where its vocabulary leaves it out, and
    // by the server itself when the rule is written anyway, in a request only this test sends.
    let important = subject_rule(&marker, vec![Action::MarkImportance(Importance::High)]);
    let vocabulary = rules.rule_vocabulary(&id).await.unwrap();
    let plan = engine_core::rules::plan_rules(
        std::slice::from_ref(&important),
        &vocabulary,
        engine_core::rules::PlanMode::Edit,
    );
    let anyway = engine_sieve::dialect::write_rules("", std::slice::from_ref(&important)).unwrap();
    let checked = admin.check_script(&anyway).await;
    if label == "dovecot" {
        // Dovecot lists no `editheader`.
        assert_eq!(plan.outcomes[0].kept, Kept::NotKept, "{label}");
        assert_eq!(checked.unwrap_err().code(), None, "{label}");
    } else {
        // Stalwart lists it, and takes it.
        assert_ne!(plan.outcomes[0].kept, Kept::NotKept, "{label}");
        checked.unwrap();
    }

    // The reply: off at first, then on with a window and a subject, read back as written.
    let off = reply.read_vacation(&id).await.unwrap();
    assert!(!off.enabled, "{label}");
    let start = UtcDateTime::new(2030, 1, 2, 9, 0, 0).unwrap();
    let end = UtcDateTime::new(2030, 1, 9, 17, 30, 0).unwrap();
    let away = VacationResponse::default()
        .with_window(VacationWindow::default().with_start(start).with_end(end))
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("I am away.\nBack soon."),
        )
        .turned_on();
    let _ = reply
        .set_vacation(&id, VacationPrecondition::IfUnchanged(&off), &away)
        .await
        .unwrap();
    let on = reply.read_vacation(&id).await.unwrap();
    assert!(on.enabled, "{label}");
    assert_eq!(
        (on.window, on.reply.clone()),
        (away.window, away.reply.clone()),
        "{label}"
    );
    // Rules and reply are one script with one version: the reply's write moved the rules'.
    let after = rules.read_rules(&id).await.unwrap();
    assert_eq!(
        unplaced(&after),
        std::slice::from_ref(&filed),
        "{label}: the reply is not a rule"
    );
    assert_ne!(after.version, read.version, "{label}");
    let stale = reply
        .set_vacation(&id, VacationPrecondition::IfUnchanged(&off), &away)
        .await
        .unwrap_err();
    assert_eq!(stale.class(), FailureClass::Conflict, "{label}");
    // A write of the rules keeps the reply.
    let _ = rules.replace_rules(&id, &after, &[]).await.unwrap();
    assert!(reply.read_vacation(&id).await.unwrap().enabled, "{label}");

    // An overwrite of an unread reply keeps its block and its `else`, as a server takes it.
    let unread = "require [\"vacation\"];\r\nif true\r\n{\r\n\tvacation \"Old.\";\r\n\t\
                  if header :contains \"subject\" \"x\"\r\n\t{\r\n\t\tkeep;\r\n\t}\r\n}\r\n\
                  else\r\n{\r\n\tkeep;\r\n}\r\n";
    let engine_sieve::dialect::reply::ScriptReply::Unreadable(found) =
        engine_sieve::dialect::reply::read_reply(unread)
    else {
        panic!("an unread reply");
    };
    let over = engine_sieve::dialect::reply::write_reply_over(unread, &away, &found).unwrap();
    admin
        .check_script(&over)
        .await
        .unwrap_or_else(|err| panic!("{label}: {err}"));

    eprintln!("{label}: rules and reply round-tripped");
    admin.logout().await;
}

#[tokio::test]
async fn a_script_over_stalwart_s_size_limit_is_refused_by_the_store() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let Some(addr) = address("stalwart", "STALWART_MANAGESIEVE_ADDR") else {
        return;
    };
    let config = config(&addr, "carol@test.local", "harness-carol-pw");
    let mut session = dial::connect(&config, config.managesieve.as_ref().unwrap(), &connector())
        .await
        .unwrap();
    clean(&mut session).await;
    // Over the 102,400 bytes Stalwart's JMAP session states as `maxSizeScript`.
    let mut text = String::from("require [\"fileinto\"];\r\n");
    for at in 0..2500 {
        let _ = write!(
            text,
            "if header :contains \"subject\" \"zbig-{at:06}\"\r\n{{\r\n\tfileinto \"Archive\";\r\n}}\r\n"
        );
    }
    assert!(text.len() > 150_000);
    let name = ScriptName::new("zlive-size").unwrap();
    // Asked first, Stalwart says it would fit (STALWART-10); RFC 5804 §2.3 has it answer
    // `NO (QUOTA/MAXSIZE)`, as Dovecot does. The store then refuses it.
    session
        .have_space(&name, text.len())
        .await
        .expect("Stalwart answers HAVESPACE yes whatever the size");
    let stored = session.put_script(&name, &text).await;
    let listed = session.list_scripts().await.unwrap();
    if listed.iter().any(|script| script.name == name) {
        session.delete_script(&name).await.unwrap();
    }
    session.logout().await;
    let refused = stored.expect_err("Stalwart stores no script over its limit");
    assert_eq!(
        refused.code(),
        Some(ResponseCode::Quota(super::response::Quota::MaxSize))
    );
}
