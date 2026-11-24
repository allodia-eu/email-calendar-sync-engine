//! Gated live: rules and the out-of-office reply imported over ManageSieve through the provider's
//! `rules_importer()`, on both harness servers (Stalwart as the scratch account `carol@`, Dovecot
//! as `alice@`), each with a rule of the account's own already stored.
//!
//! - The imported rules, built here in the neutral model as a Graph source reads them (a redirect,
//!   one with an Exchange-only exception, a folder named by id), are appended after the account's
//!   own rule, which the server then holds byte for byte; they read back typed, the redirect
//!   disabled and named by the preflight list. The two servers' vocabularies differ: Dovecot lists
//!   no `editheader`, so an importance mark is lost there.
//! - A reply imports where the account holds none, again for nothing, and is refused over another.
//!
//! Takes the module's lock (`ONE_AT_A_TIME`) and starts and ends with the account holding no
//! script, as the other live ManageSieve tests do.

use std::{
    panic::AssertUnwindSafe,
    time::{SystemTime, UNIX_EPOCH},
};

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::Importance,
    rules::{
        Action, AddressField, AddressPart, Condition, ForwardingRules, Kept, LossReason, MailRule,
        MailboxPath, Match, NativeCondition, NativeKind, RuleAddress, RuleBody, RuleMailbox,
        RuleName, RulePart, RulesActivation, RulesStored, TextOp, TextTest, held_forwarding,
    },
    time::UtcDateTime,
    vacation::{AutoReply, VacationResponse, VacationWindow},
};
use engine_provider::{Credential, Provider};
use futures_util::FutureExt as _;

use super::{
    dial,
    live_provider_tests::{SERVERS, connector, subject_rule},
    live_tests::{ONE_AT_A_TIME, address, clean, config},
    name::ScriptName,
};
use crate::{ImapAccount, ImapConfig};

#[tokio::test]
async fn rules_and_a_reply_are_imported_beside_the_accounts_own_on_both_servers() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (label, imap_var, sieve_var, user, password) in SERVERS {
        let (Some(imap), Some(sieve)) = (address(label, imap_var), address(label, sieve_var))
        else {
            continue;
        };
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

/// Four rules as a Graph source reads them; the folder id is one the host could not map.
fn graph_rules(marker: &str) -> Vec<MailRule> {
    let from_board = Condition::Address {
        field: AddressField::From,
        part: AddressPart::All,
        test: TextTest::new(TextOp::Contains, vec!["board@example.com".to_owned()]).unwrap(),
    };
    let automatic_reply = NativeCondition::new(NativeKind::Graph, r#"{"isAutomaticReply":true}"#);
    let inbox = || Action::FileInto {
        mailbox: RuleMailbox::Path(MailboxPath::new("INBOX").unwrap()),
        copy: false,
    };
    vec![
        subject_rule(
            &format!("{marker}-invoice"),
            vec![Action::MarkImportance(Importance::High), inbox()],
        )
        .stopping(),
        MailRule::typed(
            Match::all(vec![from_board]).except(vec![Condition::Native(automatic_reply)]),
            vec![inbox()],
        )
        .named(RuleName::new("Board").unwrap()),
        subject_rule(
            &format!("{marker}-forward"),
            vec![Action::Redirect {
                to: vec![RuleAddress::new("away@example.com").unwrap()],
                copy: false,
            }],
        ),
        subject_rule(
            &format!("{marker}-lost"),
            vec![Action::FileInto {
                mailbox: RuleMailbox::Id(MailboxId::try_from("AAMkunmapped").unwrap()),
                copy: false,
            }],
        ),
    ]
}

async fn run(label: &str, imap: &str, sieve: &str, user: &str, password: &str) {
    let marker = format!(
        "zlive-import-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // The account's own rule, stored as a user's client stored it, and running.
    let own = format!(
        "require [\"fileinto\"];\r\n# rule:[Own]\r\nif header :contains \"subject\" \
         \"{marker}-own\"\r\n{{\r\n\tfileinto \"INBOX\";\r\n}}\r\n"
    );
    let rules_script = ScriptName::new("rules").unwrap();
    let admin_config = config(sieve, user, password);
    let endpoint = admin_config.managesieve.clone().unwrap();
    let mut admin = dial::connect(&admin_config, &endpoint, &connector())
        .await
        .unwrap();
    admin.put_script(&rules_script, &own).await.unwrap();
    admin.set_active(Some(&rules_script)).await.unwrap();

    let config = ImapConfig::new(imap, "localhost", Credential::password(user, password))
        .with_managesieve(sieve, "localhost");
    let account = ImapAccount::connect(&config, connector()).await.unwrap();
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let rules = provider.mail_rules().expect("rules offered");
    let importer = provider.rules_importer().expect("rules import offered");
    let id = AccountId::try_from("live-managesieve").unwrap();
    let before = rules.read_rules(&id).await.unwrap();

    let wanted = graph_rules(&marker);
    let vocabulary = rules.rule_vocabulary(&id).await.unwrap();
    let preflight = held_forwarding(&wanted, &vocabulary);
    let receipt = importer
        .import_rules(&id, &wanted, ForwardingRules::Disabled)
        .await
        .unwrap_or_else(|err| panic!("{label}: {err:?}"));
    let kept: Vec<Kept> = receipt
        .outcomes
        .iter()
        .map(|outcome| outcome.kept)
        .collect();
    let losses = |at: usize| -> Vec<(RulePart, LossReason)> {
        receipt.outcomes[at]
            .losses
            .iter()
            .map(|loss| (loss.part, loss.reason))
            .collect()
    };
    let (invoice, mark) = if label == "dovecot" {
        // No `editheader`: the mark is lost, and the rule does the rest.
        (Kept::Reduced, LossReason::NotExpressible)
    } else {
        (Kept::Whole, LossReason::Approximated)
    };
    assert_eq!(
        kept,
        [invoice, Kept::NotKept, Kept::Disabled, Kept::NotKept],
        "{label}: {:?}",
        receipt.outcomes
    );
    assert_eq!(losses(0), [(RulePart::Action(0), mark)], "{label}");
    assert_eq!(
        losses(1),
        [(RulePart::Exception(0), LossReason::NotExpressible)],
        "{label}: a lost exception drops the rule"
    );
    assert_eq!(
        losses(3),
        [(RulePart::Action(0), LossReason::MissingMailbox)],
        "{label}: no folder is known by id over ManageSieve"
    );
    assert_eq!(preflight, [receipt.outcomes[2].clone()], "{label}");
    assert_eq!(receipt.stored, RulesStored::InPlace, "{label}");
    assert_eq!(receipt.activation, RulesActivation::Running, "{label}");

    // The account's own rule byte for byte, the imported ones after it, read back typed.
    let text = admin.get_script(&rules_script).await.unwrap();
    eprintln!("{label} after the import:\n{text}");
    let own_rule = &own[own.find("# rule:[Own]").unwrap()..];
    let at = text.find(own_rule).expect("the account's own rule");
    assert!(
        text[at + own_rule.len()..].starts_with(&format!("# rule:[Live {marker}-invoice]")),
        "{label}"
    );
    let read = rules.read_rules(&id).await.unwrap();
    assert_eq!(read.version, receipt.version, "{label}");
    let unplaced = |rule: &MailRule| {
        let mut rule = rule.clone();
        rule.origin = None;
        rule
    };
    assert_eq!(
        unplaced(&read.rules[0]),
        unplaced(&before.rules[0]),
        "{label}"
    );
    assert_eq!(read.rules.len(), 3, "{label}");
    let forward = &read.rules[2];
    assert!(!forward.enabled, "{label}: the redirect arrives disabled");
    assert!(
        matches!(&forward.body, RuleBody::Typed(typed)
            if matches!(typed.actions.as_slice(), [Action::Redirect { .. }])),
        "{label}: {forward:?}"
    );

    // The reply: where none is held, again for nothing, never over another.
    let start = UtcDateTime::new(2030, 1, 2, 9, 0, 0).unwrap();
    let away = VacationResponse::default()
        .with_window(VacationWindow::default().with_start(start))
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("I am away."),
        )
        .turned_on();
    let written = importer
        .import_vacation(&id, &away)
        .await
        .unwrap_or_else(|err| panic!("{label}: {err:?}"));
    let reply = provider.vacation().expect("reply offered");
    let on = reply.read_vacation(&id).await.unwrap();
    assert!(on.enabled, "{label}");
    assert_eq!(
        (on.window, on.reply.clone()),
        (away.window, away.reply.clone()),
        "{label}"
    );
    assert_eq!(on.version, written.version, "{label}");
    let again = importer.import_vacation(&id, &away).await.unwrap();
    assert_eq!(again.version, on.version, "{label}: nothing written");
    let other = away.with_reply(AutoReply::default().with_text("Someone else's."));
    let refused = importer.import_vacation(&id, &other).await.unwrap_err();
    assert!(refused.is_item_unavailable(), "{label}: {refused:?}");
    assert_eq!(
        reply.read_vacation(&id).await.unwrap().version,
        on.version,
        "{label}: nothing written"
    );
    // The rules are where they were: the reply is no rule.
    let after = rules.read_rules(&id).await.unwrap();
    assert_eq!(
        after.rules.iter().map(unplaced).collect::<Vec<_>>(),
        read.rules.iter().map(unplaced).collect::<Vec<_>>(),
        "{label}"
    );
    eprintln!("{label}: rules and a reply imported");
    admin.logout().await;
}
