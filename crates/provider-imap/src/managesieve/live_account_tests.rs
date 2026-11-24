//! Gated live: an IMAP account's ManageSieve service at connect, and beside a reply a JMAP server
//! runs from its own script, on Stalwart (as the scratch account `carol@`). Each test takes the
//! module's lock and starts from an account with no script; skipped, saying so, where an address or
//! the harness gate is unset.

use std::sync::{Arc, Mutex};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    rules::{Action, MailRule, MailboxPath, RuleMailbox, RulesStored},
    vacation::{AutoReply, VacationResponse},
};
use engine_provider::{ConnectService, ConnectStep, Credential, VacationPrecondition};
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
async fn an_endpoint_that_does_not_answer_costs_the_probe_s_deadline_and_hides_the_rules() {
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
    held.abort();
    // The probe's own deadline, and not the minute its reply bound would allow.
    assert!(
        took >= PROBE_DEADLINE && took < std::time::Duration::from_secs(20),
        "{took:?}"
    );
    assert_eq!(
        *steps.lock().unwrap(),
        [(ConnectService::ManageSieve, FailureClass::Retryable)]
    );
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    assert!(engine_provider::Provider::mail_rules(&provider).is_none());
    assert!(engine_provider::Provider::vacation(&provider).is_none());
    let id = AccountId::try_from("live-managesieve").unwrap();
    let folders = engine_provider::Provider::sync_mailboxes(&provider, &id, None).await;
    assert!(folders.is_ok(), "the mail works: {folders:?}");
}

#[tokio::test]
async fn after_invalidate_a_service_that_failed_for_good_is_probed_again() {
    let (Some(imap), Some(sieve)) = (
        address("stalwart", "STALWART_IMAP_ADDR"),
        address("stalwart", "STALWART_MANAGESIEVE_ADDR"),
    ) else {
        return;
    };
    // At first the endpoint greets without `STARTTLS` (a failure that does not pass, so not
    // probed again by itself); after that it relays to the harness's service.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move {
        let mut first = true;
        while let Ok((mut inbound, _)) = listener.accept().await {
            if std::mem::take(&mut first) {
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
    assert!(engine_provider::Provider::mail_rules(&provider).is_none());
    assert_eq!(
        *steps.lock().unwrap(),
        [(ConnectService::ManageSieve, FailureClass::Permanent)]
    );
    // A new network: the next dial probes again.
    account.invalidate();
    let id = AccountId::try_from("live-managesieve").unwrap();
    let folders = engine_provider::Provider::sync_mailboxes(&provider, &id, None).await;
    relay.abort();
    assert!(folders.is_ok(), "{folders:?}");
    assert!(
        engine_provider::Provider::mail_rules(&provider).is_some(),
        "offered once a probe succeeded"
    );
    assert_eq!(
        steps.lock().unwrap().len(),
        1,
        "one failure, then an answer"
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
    let listed = jmap(
        &harness,
        "urn:ietf:params:jmap:sieve",
        &format!(
            "[\"SieveScript/get\",{{\"accountId\":\"{account}\",\"ids\":null,\
             \"properties\":[\"name\",\"isActive\",\"blobId\"]}},\"0\"]"
        ),
    );
    let scripts: Vec<(String, bool, String)> = listed["list"]
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
        .collect();
    let jmap_choice = engine_sieve::dialect::scripts::rules_script(
        &scripts
            .iter()
            .map(|(name, active, _)| engine_sieve::dialect::scripts::Listed {
                name,
                active: *active,
                server_reply: Some(name == "vacation"),
            })
            .collect::<Vec<_>>(),
    );
    let engine_sieve::dialect::scripts::Choice::Script(at) = jmap_choice else {
        panic!("JMAP chooses no script: {scripts:?}");
    };
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
