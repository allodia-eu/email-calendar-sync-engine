//! Gated live: an IMAP account's ManageSieve service once it connects, and beside a reply a JMAP
//! server runs from its own script, on Stalwart (as the scratch account `carol@`). Each test takes
//! the module's lock and starts from an account with no script; skipped, saying so, where an
//! address or the harness gate is unset.

use std::sync::{Arc, Mutex};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    rules::{Action, MailRule, MailboxPath, RuleMailbox, RulesStored},
    vacation::{AutoReply, VacationResponse},
};
use engine_provider::{ConnectService, ConnectStep, Credential, VacationPrecondition};
use engine_sieve::dialect::scripts::{Choice, Listed, rules_script};
use tokio::io::AsyncWriteExt;

use super::{
    dial,
    live_provider_tests::{connector, subject_rule, unplaced},
    live_tests::{ONE_AT_A_TIME, address, clean},
    name::ScriptName,
    reach::PROBE_DEADLINE,
};
use crate::{ImapAccount, ImapConfig};

#[tokio::test]
async fn an_endpoint_that_does_not_answer_delays_no_connect_and_hides_the_rules_after_its_deadline()
{
    let Some(imap) = address("stalwart", "STALWART_IMAP_ADDR") else {
        return;
    };
    // A service that takes the connection and never greets.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap().to_string();
    let held = tokio::spawn(async move {
        let accepted = silent.accept().await;
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        drop(accepted);
    });
    let (config, steps) = observed(&imap, &addr);
    let started = std::time::Instant::now();
    let account = ImapAccount::connect(&config, connector())
        .await
        .expect("mail connects whatever Sieve does");
    let took = started.elapsed();
    // The probe runs in the background: the connect is the mail's alone.
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let rules = engine_provider::Provider::mail_rules(&provider).expect("not known yet: offered");
    let id = AccountId::try_from("live-managesieve").unwrap();
    let folders = engine_provider::Provider::sync_mailboxes(&provider, &id, None).await;
    assert!(folders.is_ok(), "the mail works meanwhile: {folders:?}");
    // A call waits for the probe, which runs out its deadline, and is refused as not reached yet.
    let read = rules.read_rules(&id).await;
    let took = started.elapsed();
    held.abort();
    let err = read.expect_err("not reached");
    assert_eq!(err.class(), FailureClass::Retryable, "{err:?}");
    assert!(
        took >= PROBE_DEADLINE && took < std::time::Duration::from_secs(20),
        "the probe's own deadline, and not the minute its reply bound would allow: {took:?}"
    );
    assert_eq!(
        *steps.lock().unwrap(),
        [(ConnectService::ManageSieve, FailureClass::Retryable)]
    );
    assert!(engine_provider::Provider::mail_rules(&provider).is_none());
    assert!(engine_provider::Provider::vacation(&provider).is_none());
}

#[tokio::test]
async fn after_invalidate_the_service_is_probed_again_and_offered_as_it_answers_now() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let (Some(imap), Some(sieve)) = (
        address("stalwart", "STALWART_IMAP_ADDR"),
        address("stalwart", "STALWART_MANAGESIEVE_ADDR"),
    ) else {
        return;
    };
    // While `relaying`, the endpoint relays to the harness's service; after that it greets
    // without `STARTTLS`, as no ManageSieve server does.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap();
    let relaying = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let relays = Arc::clone(&relaying);
    let relay = tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            if !relays.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = inbound
                    .write_all(b"\"IMPLEMENTATION\" \"plain\"\r\nOK\r\n")
                    .await;
                continue;
            }
            let target = sieve.clone();
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    let (config, steps) = observed(&imap, &port.to_string());
    let account = ImapAccount::connect(&config, connector()).await.unwrap();
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let id = AccountId::try_from("live-managesieve").unwrap();
    let rules = engine_provider::Provider::mail_rules(&provider).expect("offered");
    // The call waits for the probe at connect, which finds the service.
    let read_at_connect = rules.read_rules(&id).await;
    // A new network: the next dial starts a probe, and it finds no ManageSieve there now.
    relaying.store(false, std::sync::atomic::Ordering::SeqCst);
    account.invalidate();
    let folders = engine_provider::Provider::sync_mailboxes(&provider, &id, None).await;
    // Nothing waits for that probe: the rules stay offered until it lands, within its deadline.
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE + std::time::Duration::from_secs(5);
    while engine_provider::Provider::mail_rules(&provider).is_some()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let hidden = engine_provider::Provider::mail_rules(&provider).is_none();
    // A definite answer: a call through the reference taken before is refused as not offered.
    let read_after = rules.read_rules(&id).await;
    relay.abort();
    assert!(read_at_connect.is_ok(), "{read_at_connect:?}");
    assert!(folders.is_ok(), "{folders:?}");
    assert!(hidden, "hidden once a probe failed");
    assert!(
        read_after
            .as_ref()
            .is_err_and(engine_provider::ProviderError::is_item_unavailable),
        "{read_after:?}"
    );
    assert_eq!(
        *steps.lock().unwrap(),
        [(ConnectService::ManageSieve, FailureClass::Permanent)]
    );
}

/// The `Unavailable` steps an observer saw.
type Steps = Arc<Mutex<Vec<(ConnectService, FailureClass)>>>;

/// carol's account with ManageSieve at `sieve`, and the `Unavailable` steps its observer saw.
fn observed(imap: &str, sieve: &str) -> (ImapConfig, Steps) {
    let steps: Steps = Arc::default();
    let seen = Arc::clone(&steps);
    let config = ImapConfig::new(
        imap,
        "localhost",
        Credential::password("carol@test.local", "harness-carol-pw"),
    )
    .with_managesieve(sieve, "localhost")
    .with_connect_observer(Arc::new(move |step: &ConnectStep<'_>| {
        if let ConnectStep::Unavailable { service, class, .. } = step {
            seen.lock().unwrap().push((*service, *class));
        }
    }));
    (config, steps)
}

/// carol's Sieve scripts as JMAP lists them: name, active, blob.
fn jmap_scripts(harness: &stalwart_harness::Harness, account: &str) -> Vec<(String, bool, String)> {
    let listed = jmap(
        harness,
        "urn:ietf:params:jmap:sieve",
        &format!(
            "[\"SieveScript/get\",{{\"accountId\":\"{account}\",\"ids\":null,\
             \"properties\":[\"name\",\"isActive\",\"blobId\"]}},\"0\"]"
        ),
    );
    listed["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|script| {
            (
                script["name"].as_str().unwrap().to_owned(),
                script["isActive"].as_bool().unwrap(),
                script["blobId"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The script the JMAP adapter takes for the rules': it knows the server's reply by its name.
fn jmap_chooses(scripts: &[(String, bool, String)]) -> Option<usize> {
    let listed: Vec<Listed<'_>> = scripts
        .iter()
        .map(|(name, active, _)| Listed {
            name,
            active: *active,
            server_reply: Some(name == "vacation"),
        })
        .collect();
    match rules_script(&listed) {
        Choice::Script(at) => Some(at),
        _ => None,
    }
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
async fn beside_a_jmap_server_s_reply_rules_go_in_a_script_that_includes_it() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let (Some(harness), Some(imap), Some(sieve)) = (
        stalwart_harness::Harness::from_env(),
        address("stalwart", "STALWART_IMAP_ADDR"),
        address("stalwart", "STALWART_MANAGESIEVE_ADDR"),
    ) else {
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
    clean(&mut admin).await;
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
    let replies_refused = async || {
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
        (read.is_err_and(|err| err.is_item_unavailable()), written)
    };
    // The server's reply script running: the reply is neither read nor written over ManageSieve.
    let mut outcome = vec![replies_refused().await];

    // Rules written beside it: never into the server's script, but into a new `rules` that
    // includes it first and runs, so the reply runs on, as the JMAP adapter writes them.
    let base = rules_of.read_rules(&id).await.unwrap();
    assert!(base.rules.is_empty(), "the server's reply is no rule");
    let marker = subject_rule(
        "zlive-server-script",
        vec![Action::FileInto {
            mailbox: RuleMailbox::Path(MailboxPath::new("Archive").unwrap()),
            copy: false,
        }],
    );
    let receipt = rules_of
        .replace_rules(&id, &base, std::slice::from_ref(&marker))
        .await
        .unwrap();
    // What JMAP sees: `rules` running and including the reply's script, which JMAP's choice,
    // made by the reply script's name, takes for the rules' script too.
    let scripts = jmap_scripts(&harness, &account);
    let at = jmap_chooses(&scripts).expect("JMAP chooses a script");
    let (jmap_name, jmap_active, blob) = scripts[at].clone();
    let download = session["downloadUrl"]
        .as_str()
        .unwrap()
        .replace("{accountId}", &account)
        .replace("{blobId}", &blob)
        .replace("{name}", "rules.sieve")
        .replace("{type}", "application/sieve");
    let path = download
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| format!("/{path}"))
        .unwrap();
    let jmap_text = harness
        .dav_get_as(("carol@test.local", "harness-carol-pw"), &path)
        .unwrap()
        .body_text();
    let jmap_rules: Vec<MailRule> =
        engine_sieve::dialect::include::without_include(&jmap_text, "vacation")
            .into_iter()
            .map(|mut rule| {
                rule.origin = None;
                rule
            })
            .collect();
    // The reply's script is still there, and runs through the include.
    outcome.push(replies_refused().await);
    // A write of no rules keeps the include, so the reply runs on.
    let after_write = rules_of.read_rules(&id).await.unwrap();
    let emptied = rules_of
        .replace_rules(&id, &after_write, &[])
        .await
        .unwrap();
    let left_rules = admin.get_script(&rules).await.unwrap();
    let after_reply = admin.get_script(&reply).await.unwrap();

    admin.set_active(None).await.unwrap();
    admin.delete_script(&rules).await.unwrap();
    let off = set_reply("{\"isEnabled\":false}");
    assert!(off["updated"].is_object(), "{off}");
    admin.delete_script(&reply).await.unwrap();
    admin.logout().await;

    assert_eq!(receipt.stored, RulesStored::CreatedRunning);
    assert_eq!(receipt.reply, engine_core::rules::ReplyEffect::Untouched);
    assert_eq!(
        (jmap_name.as_str(), jmap_active),
        ("rules", true),
        "{scripts:?}"
    );
    assert!(
        engine_sieve::dialect::include::includes_script(&jmap_text, "vacation"),
        "{jmap_text}"
    );
    assert_eq!(
        jmap_rules,
        [marker],
        "JMAP reads the rules ManageSieve wrote"
    );
    assert_eq!(
        unplaced(&after_write),
        [subject_rule(
            "zlive-server-script",
            vec![Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("Archive").unwrap()),
                copy: false,
            }],
        )]
    );
    assert_eq!(emptied.reply, engine_core::rules::ReplyEffect::Untouched);
    assert!(
        engine_sieve::dialect::include::includes_script(&left_rules, "vacation"),
        "the include survives a write of no rules: {left_rules}"
    );
    assert_eq!(
        outcome,
        [(true, vec![true, true]), (true, vec![true, true])]
    );
    assert_eq!(after_reply, ours, "the server's reply script is untouched");
}

#[tokio::test]
async fn beside_stalwart_s_reply_script_both_adapters_take_the_user_s_one_script() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let (Some(harness), Some(imap), Some(sieve)) = (
        stalwart_harness::Harness::from_env(),
        address("stalwart", "STALWART_IMAP_ADDR"),
        address("stalwart", "STALWART_MANAGESIEVE_ADDR"),
    ) else {
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
    clean(&mut admin).await;
    let session = harness
        .jmap_session_as(("carol@test.local", "harness-carol-pw"))
        .unwrap();
    let using = "urn:ietf:params:jmap:vacationresponse";
    let account = session["primaryAccounts"][using]
        .as_str()
        .unwrap()
        .to_owned();
    let set_reply = |update: &str| {
        jmap(
            &harness,
            using,
            &format!(
                "[\"VacationResponse/set\",{{\"accountId\":\"{account}\",\
                 \"update\":{{\"singleton\":{update}}}}},\"0\"]"
            ),
        )
    };
    // The reply switched on and off over JMAP: Stalwart keeps its script, inactive.
    assert!(set_reply("{\"isEnabled\":true,\"textBody\":\"Away.\"}")["updated"].is_object());
    assert!(set_reply("{\"isEnabled\":false}")["updated"].is_object());
    // The user's own rules, in a script of their own, inactive.
    let mine = ScriptName::new("myrules").unwrap();
    let first = subject_rule("zlive-myrules", archived());
    let text =
        engine_sieve::dialect::include::compose("", std::slice::from_ref(&first), None, false)
            .unwrap();
    admin.put_script(&mine, &text).await.unwrap();
    let listed: Vec<(String, bool)> = admin
        .list_scripts()
        .await
        .unwrap()
        .into_iter()
        .map(|script| (script.name.as_str().to_owned(), script.active))
        .collect();

    let scripts = jmap_scripts(&harness, &account);
    let jmap_name = jmap_chooses(&scripts).map(|at| scripts[at].0.clone());
    let handle = ImapAccount::connect(&config, connector()).await.unwrap();
    let provider = handle.provider(MailboxId::try_from("INBOX").unwrap());
    let rules_of = engine_provider::Provider::mail_rules(&provider).unwrap();
    let id = AccountId::try_from("live-managesieve").unwrap();
    let base = rules_of.read_rules(&id).await.unwrap();
    let second = subject_rule("zlive-myrules-second", archived());
    let written = rules_of
        .replace_rules(&id, &base, std::slice::from_ref(&second))
        .await;
    let after: Vec<String> = admin
        .list_scripts()
        .await
        .unwrap()
        .into_iter()
        .map(|script| script.name.as_str().to_owned())
        .collect();
    let stored = admin.get_script(&mine).await.unwrap();

    clean(&mut admin).await;
    admin.logout().await;

    let mut listed_sorted = listed;
    listed_sorted.sort();
    assert_eq!(
        listed_sorted,
        [
            ("myrules".to_owned(), false),
            ("vacation".to_owned(), false)
        ]
    );
    assert_eq!(jmap_name.as_deref(), Some("myrules"), "{scripts:?}");
    assert_eq!(unplaced(&base), [first], "ManageSieve reads myrules too");
    assert!(written.is_ok(), "{written:?}");
    let mut after_sorted = after;
    after_sorted.sort();
    assert_eq!(after_sorted, ["myrules", "vacation"], "no `rules` created");
    assert!(stored.contains("zlive-myrules-second"), "{stored}");
}

/// Filing into `Archive`, the action the rules here take.
fn archived() -> Vec<Action> {
    vec![Action::FileInto {
        mailbox: RuleMailbox::Path(MailboxPath::new("Archive").unwrap()),
        copy: false,
    }]
}
