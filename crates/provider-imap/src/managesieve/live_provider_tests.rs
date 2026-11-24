//! Gated live: the rules and the out-of-office reply over ManageSieve, end to end through
//! [`ImapAccount::connect`] and the provider's `mail_rules()` and `vacation()`, on both harness
//! servers (Stalwart as the scratch account `carol@`, Dovecot as `alice@`).
//!
//! One test function per server, run one after the other, because both write the account's one
//! rules script. The account must start with no script named `rules`; the run deletes the one it
//! creates and switches back on whatever ran before. Skips per server when an address variable is
//! unset.

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
use engine_provider::{ConnectService, ConnectStep, Credential, VacationPrecondition};
use futures_util::FutureExt as _;

use super::{
    dial,
    live_tests::{ONE_AT_A_TIME, config},
    name::ScriptName,
    response::ResponseCode,
};
use crate::{ImapAccount, ImapConfig};

/// A harness server: label, IMAP and ManageSieve address variables, account, password.
const SERVERS: [(&str, &str, &str, &str, &str); 2] = [
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

fn connector() -> tokio_rustls::TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

fn subject_rule(marker: &str, actions: Vec<Action>) -> MailRule {
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

fn unplaced(set: &RuleSet) -> Vec<MailRule> {
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
        let (Ok(imap), Ok(sieve)) = (std::env::var(imap_var), std::env::var(sieve_var)) else {
            eprintln!("skipping ManageSieve rules on {label}: {imap_var} or {sieve_var} unset");
            continue;
        };
        // What ran before, to switch back on afterwards, however the run ends; and no `rules`
        // script of anyone's, which the run would edit.
        let admin_config = config(&sieve, user, password);
        let endpoint = admin_config.managesieve.clone().unwrap();
        let mut admin = dial::connect(&admin_config, &endpoint, &connector())
            .await
            .unwrap();
        let before = admin.list_scripts().await.unwrap();
        let rules = ScriptName::new("rules").unwrap();
        assert!(
            before.iter().all(|script| script.name != rules),
            "{label}: the account already has a `rules` script"
        );
        let ran = before
            .into_iter()
            .find(|script| script.active)
            .map(|script| script.name);
        let outcome = AssertUnwindSafe(run(label, &imap, &sieve, user, password))
            .catch_unwind()
            .await;
        admin.set_active(ran.as_ref()).await.unwrap();
        if admin
            .list_scripts()
            .await
            .unwrap()
            .iter()
            .any(|script| script.name == rules)
        {
            admin.delete_script(&rules).await.unwrap();
        }
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
    assert!(
        steps
            .lock()
            .unwrap()
            .iter()
            .any(|step| step == "ManageSieve"),
        "{label}"
    );
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let rules = engine_provider::Provider::mail_rules(&provider).expect("rules offered");
    let reply = engine_provider::Provider::vacation(&provider).expect("reply offered");
    let id = AccountId::try_from("live-managesieve").unwrap();

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
    let Ok(addr) = std::env::var("STALWART_MANAGESIEVE_ADDR") else {
        eprintln!("skipping the size limit: STALWART_MANAGESIEVE_ADDR unset");
        return;
    };
    let config = config(&addr, "carol@test.local", "harness-carol-pw");
    let mut session = dial::connect(&config, config.managesieve.as_ref().unwrap(), &connector())
        .await
        .unwrap();
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
    // Asked first, Stalwart says it would fit (STALWART-NEW-1); RFC 5804 §2.3 has it answer
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

#[tokio::test]
async fn an_endpoint_that_does_not_answer_leaves_the_mail_working_and_says_so() {
    let Ok(imap) = std::env::var("STALWART_IMAP_ADDR") else {
        eprintln!("skipping an unreachable ManageSieve: STALWART_IMAP_ADDR unset");
        return;
    };
    let steps: Arc<Mutex<Vec<(ConnectService, FailureClass)>>> = Arc::default();
    let seen = Arc::clone(&steps);
    let config = ImapConfig::new(
        imap.as_str(),
        "localhost",
        Credential::password("carol@test.local", "harness-carol-pw"),
    )
    .with_managesieve("127.0.0.1:1", "localhost")
    .with_connect_observer(Arc::new(move |step: &ConnectStep<'_>| {
        if let ConnectStep::Unavailable { service, class, .. } = step {
            seen.lock().unwrap().push((*service, *class));
        }
    }));
    let account = ImapAccount::connect(&config, connector())
        .await
        .expect("mail connects whatever Sieve does");
    assert_eq!(
        *steps.lock().unwrap(),
        [(ConnectService::ManageSieve, FailureClass::Retryable)]
    );
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    assert!(engine_provider::Provider::mail_rules(&provider).is_none());
    assert!(engine_provider::Provider::vacation(&provider).is_none());
    let folders = engine_provider::Provider::sync_mailboxes(
        &provider,
        &AccountId::try_from("live-managesieve").unwrap(),
        None,
    )
    .await;
    assert!(folders.is_ok(), "the mail works: {folders:?}");
}

/// One JMAP call as carol, answering its result's arguments.
fn jmap(harness: &stalwart_harness::Harness, using: &str, call: &str) -> serde_json::Value {
    let body = format!(
        "{{\"using\":[\"urn:ietf:params:jmap:core\",\"{using}\"],\"methodCalls\":[{call}]}}"
    );
    let response = harness
        .jmap_post_as(("carol@test.local", "harness-carol-pw"), body.as_bytes())
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&response.body_text()).unwrap();
    value["methodResponses"][0][1].clone()
}

#[tokio::test]
async fn a_reply_a_jmap_server_runs_from_its_own_script_is_neither_read_nor_written() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let (Some(harness), Ok(imap), Ok(sieve)) = (
        stalwart_harness::Harness::from_env(),
        std::env::var("STALWART_IMAP_ADDR"),
        std::env::var("STALWART_MANAGESIEVE_ADDR"),
    ) else {
        eprintln!("skipping a JMAP-shaped account: a Stalwart address is unset");
        return;
    };
    let config = ImapConfig::new(
        imap.as_str(),
        "localhost",
        Credential::password("carol@test.local", "harness-carol-pw"),
    )
    .with_managesieve(sieve.as_str(), "localhost");
    let mut admin = dial::connect(&config, config.managesieve.as_ref().unwrap(), &connector())
        .await
        .unwrap();
    assert!(
        admin.list_scripts().await.unwrap().is_empty(),
        "scripts already"
    );
    let session = harness
        .jmap_session_as(("carol@test.local", "harness-carol-pw"))
        .unwrap();
    let vacation_using = "urn:ietf:params:jmap:vacationresponse";
    let account = session["primaryAccounts"][vacation_using]
        .as_str()
        .unwrap()
        .to_owned();
    let set_reply = |update: &str| {
        jmap(
            &harness,
            vacation_using,
            &format!(
                "[\"VacationResponse/set\",{{\"accountId\":\"{account}\",\
                 \"update\":{{\"singleton\":{update}}}}},\"0\"]"
            ),
        )
    };
    // The reply switched on over JMAP: Stalwart writes and activates a `vacation` script of its
    // own.
    let on = set_reply(
        "{\"isEnabled\":true,\"fromDate\":\"2030-01-02T09:00:00Z\",\"textBody\":\"Away.\"}",
    );
    assert!(on["updated"].is_object(), "{on}");
    let reply = ScriptName::new("vacation").unwrap();
    let rules = ScriptName::new("rules").unwrap();
    let ours = admin.get_script(&reply).await.unwrap();
    // ManageSieve may not write that script: it is the reply's (RFC 9661 §4).
    let refused = admin.put_script(&reply, "keep;\r\n").await.unwrap_err();
    assert_eq!(refused.code(), None);

    let account_handle = ImapAccount::connect(&config, connector()).await.unwrap();
    let provider = account_handle.provider(MailboxId::try_from("INBOX").unwrap());
    let vacation = engine_provider::Provider::vacation(&provider).unwrap();
    let rules_of = engine_provider::Provider::mail_rules(&provider).unwrap();
    let id = AccountId::try_from("live-managesieve").unwrap();
    // Rules are not written into the server's reply script while it runs: its next reply write
    // would drop them.
    let base = rules_of.read_rules(&id).await.unwrap();
    let marker = subject_rule(
        "zlive-server-script",
        vec![Action::FileInto {
            mailbox: RuleMailbox::Path(MailboxPath::new("Archive").unwrap()),
            copy: false,
        }],
    );
    // Refused before anything is sent: Stalwart would refuse `PUTSCRIPT "vacation"` itself, so the
    // refusal must be the engine's, which names the reply no rule names.
    let rules_refused = rules_of
        .replace_rules(&id, &base, std::slice::from_ref(&marker))
        .await
        .is_err_and(|err| err.is_item_unavailable() && err.detail().contains("no rule names"));
    let mut outcome = Vec::new();
    // Its script running; then the shape the JMAP adapter leaves, the user's `rules` running and
    // including it.
    let including = "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\n";
    for step in 0..2 {
        if step == 1 {
            admin.put_script(&rules, including).await.unwrap();
            admin.set_active(Some(&rules)).await.unwrap();
        }
        let read = vacation.read_vacation(&id).await;
        let mut written = Vec::new();
        for new in [
            VacationResponse::default().turned_on(),
            VacationResponse::default(),
        ] {
            let new = new.with_reply(AutoReply::default().with_text("Mine."));
            written.push(
                vacation
                    .set_vacation(&id, VacationPrecondition::Unconditional, &new)
                    .await
                    .is_err_and(|err| err.is_item_unavailable()),
            );
        }
        outcome.push((read.is_err_and(|err| err.is_item_unavailable()), written));
    }
    let after_reply = admin.get_script(&reply).await.unwrap();
    let after_rules = admin.get_script(&rules).await.unwrap();

    admin.set_active(None).await.unwrap();
    admin.delete_script(&rules).await.unwrap();
    let off = set_reply("{\"isEnabled\":false}");
    assert!(off["updated"].is_object(), "{off}");
    admin.delete_script(&reply).await.unwrap();
    admin.logout().await;

    assert!(
        rules_refused,
        "rules written into the server's reply script"
    );
    assert_eq!(
        outcome,
        [(true, vec![true, true]), (true, vec![true, true])]
    );
    assert_eq!(after_reply, ours, "the server's reply script is untouched");
    assert_eq!(after_rules, including, "nothing was written beside it");
}
