//! Gated live integration: importing a mail-rule set and an out-of-office reply
//! (`RulesImporter` for `JmapProvider`) into the Stalwart harness, beside rules the account
//! already has. It proves what the offline suite cannot:
//!
//! - the server answers `Mailbox/get` for exactly the folder ids the rules name, with `notFound`
//!   for one it does not have, which the import loses as `MissingMailbox`;
//! - the imported rules, built here in the neutral model as a Graph source reads them (a redirect,
//!   and one with an Exchange-only exception), are appended after the account's own, whose bytes
//!   the server then holds unchanged, and read back typed, the redirect disabled;
//! - the preflight list (`held_forwarding`) names exactly the rule the import stored disabled;
//! - a reply imports where the account holds none, an identical one again writes nothing, and a
//!   different one is refused.
//!
//! Runs as the scratch account `carol@` (`sieve_common` says why that is safe beside the other
//! Sieve suites), and restores Carol whether it passes or not. About thirty requests.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 cargo test -p provider-jmap --all-features \
//!   --test live_rules_import -- --nocapture
//! ```

#![cfg(feature = "import")]

#[path = "sieve_common/mod.rs"]
mod sieve_common;

use std::panic::{AssertUnwindSafe, resume_unwind};

use engine_core::{
    ids::MailboxId,
    mail::Importance,
    rules::{
        Action, AddressField, AddressPart, Condition, FolderMap, ForwardingRules, HeaderField,
        Kept, LossReason, MailRule, Match, NativeCondition, NativeKind, RuleAddress, RuleBody,
        RuleMailbox, RuleName, RulePart, RulesActivation, RulesStored, TextOp, TextTest,
        held_forwarding,
    },
    time::UtcDateTime,
    vacation::{AutoReply, VacationResponse, VacationWindow},
};
use engine_provider::{MailRules, Provider, Vacation};
use futures_util::FutureExt;
use provider_jmap::{JmapClient, JmapProvider};
use serde_json::json;
use sieve_common::{
    account, client, create_folder, provider, remove_folder, reply_object, restore, rules, scripts,
    text,
};

fn contains(value: &str) -> TextTest {
    TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap()
}

fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: contains(value),
    }
}

fn into(id: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(id).unwrap()),
        copy: false,
    }
}

/// Four rules as a Graph source reads them, filing into the source's folders: `AAMk-finance`, which
/// the host maps to the target's, and `AAMkunmapped`, which it could not map.
fn graph_rules(marker: &str) -> Vec<MailRule> {
    let from_board = Condition::Address {
        field: AddressField::From,
        part: AddressPart::All,
        test: contains("board@example.com"),
    };
    let automatic_reply = NativeCondition::new(NativeKind::Graph, r#"{"isAutomaticReply":true}"#);
    vec![
        MailRule::typed(
            Match::all(vec![subject(&format!("{marker}-invoice"))]),
            vec![
                into("AAMk-finance"),
                Action::MarkImportance(Importance::High),
            ],
        )
        .named(RuleName::new("Invoices").unwrap())
        .stopping(),
        MailRule::typed(
            Match::all(vec![from_board]).except(vec![Condition::Native(automatic_reply)]),
            vec![into("AAMk-finance")],
        )
        .named(RuleName::new("Board").unwrap()),
        MailRule::typed(
            Match::all(vec![subject(&format!("{marker}-forward"))]),
            vec![Action::Redirect {
                to: vec![RuleAddress::new("away@example.com").unwrap()],
                copy: false,
            }],
        )
        .named(RuleName::new("Forward").unwrap()),
        MailRule::typed(
            Match::all(vec![subject(&format!("{marker}-lost"))]),
            vec![into("AAMkunmapped")],
        )
        .named(RuleName::new("Lost folder").unwrap()),
        // Graph's `forwardTo`: written as a redirect that keeps a copy, and read back as one.
        MailRule::typed(
            Match::all(vec![subject(&format!("{marker}-forwardto"))]),
            vec![Action::Forward {
                to: vec![RuleAddress::new("away@example.com").unwrap()],
            }],
        )
        .named(RuleName::new("Forward to").unwrap()),
    ]
}

#[tokio::test]
async fn live_rules_and_a_reply_are_imported_beside_the_accounts_own() {
    let Some(harness) = sieve_common::harness("live_rules_import") else {
        return;
    };
    let carol = provider(&harness, &harness.scratch[1]).await;
    let raw = client(&harness, &harness.scratch[1]).await;
    let (prior, _) = scripts(&raw).await;
    let prior_reply = reply_object(&raw).await;
    let marker = sieve_common::marker("import");
    let folder = create_folder(&raw, &marker).await;
    let checked = AssertUnwindSafe(exercise(&carol, &raw, &prior, &folder, &marker))
        .catch_unwind()
        .await;
    remove_folder(&raw, &folder).await;
    let restored = restore(&raw, &prior, &prior_reply).await;
    if let Err(failed) = checked {
        if let Err(problem) = restored {
            eprintln!("and restoring Carol failed too: {problem}");
        }
        resume_unwind(failed);
    }
    restored.expect("Carol is left as she was found");
}

async fn exercise(
    carol: &JmapProvider,
    raw: &JmapClient,
    prior: &[sieve_common::Listed],
    folder: &str,
    marker: &str,
) {
    assert!(
        prior.is_empty(),
        "the suite starts only where Carol has no scripts, so it changes none of hers: {prior:?}"
    );
    let own = import_rules(carol, raw, folder, marker).await;
    import_reply(carol, &own).await;
}

/// The rules: the account's own first, then the import after them.
async fn import_rules(
    carol: &JmapProvider,
    raw: &JmapClient,
    folder: &str,
    marker: &str,
) -> MailRule {
    let rules: &dyn MailRules = rules(carol);
    // The account's own rule, written as a user's client would.
    let own = MailRule::typed(
        Match::all(vec![subject(&format!("{marker}-own"))]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(engine_core::mail::MailboxRole::Inbox),
            copy: false,
        }],
    )
    .named(RuleName::new("Own").unwrap());
    let empty = rules.read_rules(&account()).await.unwrap();
    let _ = rules
        .replace_rules(&account(), &empty, std::slice::from_ref(&own))
        .await
        .expect("the account's own rule");
    let before = rules.read_rules(&account()).await.unwrap();
    let own_text = script_text(raw).await;
    let own_segment = &own_text[own_text.find("# rule:[Own]").unwrap()..];

    let wanted = graph_rules(marker);
    let folders = FolderMap::new().with(
        MailboxId::try_from("AAMk-finance").unwrap(),
        MailboxId::try_from(folder).unwrap(),
    );
    let vocabulary = rules.rule_vocabulary(&account()).await.unwrap();
    let preflight = held_forwarding(&wanted, &vocabulary.with_folders(folders.clone()));
    let importer = carol
        .rules_importer()
        .expect("an account with rules imports");
    let receipt = importer
        .import_rules(&account(), &wanted, ForwardingRules::Disabled, &folders)
        .await
        .expect("imported");
    let kept: Vec<Kept> = receipt
        .outcomes
        .iter()
        .map(|outcome| outcome.kept)
        .collect();
    assert_eq!(
        kept,
        [
            Kept::Whole,
            Kept::NotKept,
            Kept::Disabled,
            Kept::NotKept,
            Kept::Disabled
        ],
        "{:?}",
        receipt.outcomes
    );
    let losses = |at: usize| -> Vec<(RulePart, LossReason)> {
        receipt.outcomes[at]
            .losses
            .iter()
            .map(|loss| (loss.part, loss.reason))
            .collect()
    };
    assert_eq!(
        losses(0),
        [(RulePart::Action(1), LossReason::Approximated)],
        "an importance mark is an `addheader` here"
    );
    assert_eq!(
        losses(1),
        [(RulePart::Exception(0), LossReason::NotExpressible)],
        "a lost exception drops the rule"
    );
    assert_eq!(
        losses(3),
        [(RulePart::Action(0), LossReason::MissingMailbox)]
    );
    assert_eq!(
        preflight,
        [receipt.outcomes[2].clone(), receipt.outcomes[4].clone()],
        "the preflight list"
    );
    assert_eq!(receipt.stored, RulesStored::InPlace);
    assert_eq!(receipt.activation, RulesActivation::Running);

    // The server holds the account's own rule byte for byte, the imported ones after it.
    let after_text = script_text(raw).await;
    eprintln!("after the import:\n{after_text}");
    let at = after_text
        .find(own_segment)
        .expect("the account's own rule, byte for byte");
    assert!(after_text[at + own_segment.len()..].starts_with("# rule:[Invoices]"));
    let read = rules.read_rules(&account()).await.unwrap();
    assert_eq!(read.version, receipt.version);
    let unplaced = |rule: &MailRule| {
        let mut rule = rule.clone();
        rule.origin = None;
        rule
    };
    assert_eq!(
        unplaced(&read.rules[0]),
        unplaced(&before.rules[0]),
        "the account's own rule"
    );
    let names: Vec<&str> = read
        .rules
        .iter()
        .map(|rule| rule.name.as_ref().map_or("", RuleName::as_str))
        .collect();
    assert_eq!(names, ["Own", "Invoices", "Forward", "Forward to"]);
    let forward = &read.rules[2];
    assert!(!forward.enabled, "the redirect arrives disabled");
    assert!(
        matches!(&forward.body, RuleBody::Typed(typed)
            if matches!(typed.actions.as_slice(), [Action::Redirect { .. }])),
        "read back typed: {forward:?}"
    );
    let RuleBody::Typed(invoices) = &read.rules[1].body else {
        panic!("read back typed");
    };
    // Marks first, as the writer writes them.
    assert_eq!(
        invoices.actions,
        [Action::MarkImportance(Importance::High), into(folder)]
    );
    assert!(invoices.stop);
    // Run again: everything it wrote is already held, and nothing is written.
    let again = importer
        .import_rules(&account(), &wanted, ForwardingRules::Disabled, &folders)
        .await
        .expect("imported again");
    // The forward among them, though it reads back as a redirect keeping a copy.
    assert_eq!(again.already_held, [0, 2, 4]);
    assert_eq!(again.stored, RulesStored::Unwritten);
    assert_eq!(script_text(raw).await, after_text, "nothing written");
    own
}

/// The text of the one rules script.
async fn script_text(raw: &JmapClient) -> String {
    let (listed, _) = scripts(raw).await;
    let script = listed
        .iter()
        .find(|(_, name, ..)| name == "rules")
        .expect("the rules script");
    text(raw, &script.3).await
}

/// The reply: imported where none is held, again for nothing, and never over another.
async fn import_reply(carol: &JmapProvider, own: &MailRule) {
    let importer = carol.rules_importer().unwrap();
    let start = UtcDateTime::parse_rfc3339("2030-01-02T09:00:00Z").unwrap();
    let away = VacationResponse::default()
        .turned_on()
        .with_window(VacationWindow::default().with_start(start))
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("I am away.\nBack on Monday."),
        );
    let receipt = importer
        .import_vacation(&account(), &away)
        .await
        .expect("a reply where none is held");
    let reply: &dyn Vacation = carol.vacation().unwrap();
    let read = reply.read_vacation(&account()).await.unwrap();
    assert!(read.enabled);
    assert_eq!(
        (read.window, read.reply.clone()),
        (away.window, away.reply.clone())
    );
    assert_eq!(read.version, receipt.version);

    let again = importer.import_vacation(&account(), &away).await.unwrap();
    assert_eq!(
        again.version, read.version,
        "the same reply again writes nothing"
    );

    let other = away.with_reply(AutoReply::default().with_text("Someone else's."));
    let err = importer
        .import_vacation(&account(), &other)
        .await
        .expect_err("a reply of the account's own is never replaced");
    assert!(err.is_item_unavailable(), "{err:?}");
    assert_eq!(
        reply.read_vacation(&account()).await.unwrap().version,
        read.version,
        "nothing written"
    );
    // The rules are where they were.
    let rules = rules(carol).read_rules(&account()).await.unwrap();
    let mut first = rules.rules[0].clone();
    first.origin = None;
    assert_eq!(&first, own);
}

#[tokio::test]
async fn live_an_import_into_a_script_switched_off_drops_its_idle_include_and_copies_the_rest() {
    let Some(harness) = sieve_common::harness("live_rules_import") else {
        return;
    };
    let carol = provider(&harness, &harness.scratch[1]).await;
    let raw = client(&harness, &harness.scratch[1]).await;
    let (prior, _) = scripts(&raw).await;
    let prior_reply = reply_object(&raw).await;
    let checked = AssertUnwindSafe(idle_script(&carol, &raw, &prior))
        .catch_unwind()
        .await;
    let restored = restore(&raw, &prior, &prior_reply).await;
    if let Err(failed) = checked {
        if let Err(problem) = restored {
            eprintln!("and restoring Carol failed too: {problem}");
        }
        resume_unwind(failed);
    }
    restored.expect("Carol is left as she was found");
}

/// The account holds a rules script, switched off, that includes the reply's script; the source
/// held the very same text, so the rules handed in name segments of a script with the same
/// fingerprint as the target's.
async fn idle_script(carol: &JmapProvider, raw: &JmapClient, prior: &[sieve_common::Listed]) {
    assert!(prior.is_empty(), "{prior:?}");
    let marker = sieve_common::marker("importidle");
    let rule = |name: &str, action: &str| {
        format!(
            "# rule:[{name}]\r\nif header :contains \"subject\" \"{marker}-{name}\"\r\n{{\r\n\t\
             {action};\r\n}}\r\n"
        )
    };
    let forward = rule("Forward", "redirect \"away@example.com\"");
    let own = rule("Own", "fileinto \"INBOX\"");
    let text = format!(
        "require [\"fileinto\",\"include\"];\r\ninclude :personal :optional \"vacation\";\r\n\
         {forward}{own}"
    );
    let created = sieve_common::sieve_set(
        raw,
        json!({ "create": { "r": { "name": "rules", "blobId": "#b1" } } }),
        Some(&text),
    )
    .await;
    assert!(created["created"]["r"].is_object(), "{created}");

    let source = engine_sieve::dialect::include::without_include(&text, "vacation");
    assert_eq!(source.len(), 2);
    let importer = carol.rules_importer().unwrap();
    let receipt = importer
        .import_rules(
            &account(),
            &source,
            ForwardingRules::Disabled,
            &FolderMap::new(),
        )
        .await
        .expect("imported");
    let kept: Vec<Kept> = receipt.outcomes.iter().map(|o| o.kept).collect();
    assert_eq!(kept, [Kept::Disabled, Kept::Whole]);
    assert_eq!(
        receipt.already_held,
        [1],
        "the account's own rule, as it is"
    );
    assert_eq!(receipt.activation, RulesActivation::Inactive);
    assert_eq!(receipt.stored, RulesStored::InPlace);

    let after = script_text(raw).await;
    eprintln!("after the import into a script switched off:\n{after}");
    assert!(
        !after.contains("include :personal"),
        "the idle include is left out"
    );
    let held = format!("{forward}{own}");
    let at = after
        .find(&held)
        .expect("the account's rules, byte for byte");
    assert!(
        after[at + held.len()..].starts_with("# rule:[Forward]\r\nif false # header"),
        "the forward appended, disabled: {after}"
    );
    let (listed, _) = scripts(raw).await;
    assert!(
        listed.iter().all(|(.., active, _)| !active),
        "nothing switched on"
    );

    let again = importer
        .import_rules(
            &account(),
            &source,
            ForwardingRules::Disabled,
            &FolderMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(again.already_held, [0, 1]);
    assert_eq!(again.stored, RulesStored::Unwritten);
}
