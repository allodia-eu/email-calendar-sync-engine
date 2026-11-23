//! Gated live integration: the account's mail rules (`MailRules` for `JmapProvider`) against the
//! Stalwart harness, which proves what the offline suite cannot:
//!
//! - the server takes the dialect the engine writes, and a rule written reads back typed;
//! - the write is guarded both ways: built on a stale read it is refused (`stateMismatch`), on the
//!   current one it is accepted;
//! - `SieveScript/set` is atomic, so validate-before-set buys nothing: an invalid create that asks
//!   to be activated stores and activates nothing and leaves the state where it was, and an invalid
//!   update of the running script leaves its bytes and its active flag as they were; through the
//!   adapter, a rule the server refuses is `item_unavailable`, and nothing changed;
//! - the vocabulary is the server's: every extension the writer uses is accepted, and an extension
//!   the server does not list is refused.
//!
//! Runs as the scratch account `carol@` (`sieve_common` says why that is safe beside
//! `live_vacation`), and restores Carol whether it passes or not. About thirty requests.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 cargo test -p provider-jmap --test live_rules -- --nocapture
//! ```

#[path = "sieve_common/mod.rs"]
mod sieve_common;

use std::panic::{AssertUnwindSafe, resume_unwind};

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole},
    rules::{
        Action, Condition, HeaderField, Kept, MailRule, MailboxPath, Match, RuleBody, RuleFeature,
        RuleMailbox, RuleName, RulesActivation, RulesStored, SizeBound, TextOp, TextTest,
    },
};
use engine_provider::{MailRules, WriteGuard};
use futures_util::FutureExt;
use provider_jmap::JmapClient;
use serde_json::json;
use sieve_common::{
    account, client, provider, reply_object, restore, rules, scripts, sieve_set, text, validate,
};

fn subject(op: TextOp, value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(op, vec![value.to_owned()]).unwrap(),
    }
}

/// Rules that between them need every extension the writer uses.
fn every_extension(marker: &str) -> Vec<MailRule> {
    vec![
        MailRule::typed(
            Match::all(vec![subject(TextOp::Contains, marker)]),
            vec![Action::AddKeywords(
                [Keyword::new("$rulesprobe").unwrap()].into(),
            )],
        )
        .named(RuleName::new("Probe").unwrap())
        .stopping(),
        MailRule::typed(
            Match::any(vec![
                subject(TextOp::Regex, "^\\[never\\]"),
                Condition::Body(TextTest::new(TextOp::Contains, vec!["never".to_owned()]).unwrap()),
            ])
            .except(vec![Condition::HasAttachment]),
            // Marks first, as the writer writes them (the model's order is not semantic).
            vec![
                Action::MarkImportance(Importance::High),
                Action::FileInto {
                    mailbox: RuleMailbox::Role(MailboxRole::Trash),
                    copy: true,
                },
            ],
        )
        .named(RuleName::new("Everything else").unwrap()),
        MailRule::typed(
            Match::all(vec![Condition::Size(SizeBound::Over(1 << 30))]),
            vec![Action::FileInto {
                mailbox: RuleMailbox::Id(MailboxId::try_from("never-such-a-box").unwrap()),
                copy: false,
            }],
        )
        .disabled(),
        MailRule::typed(
            Match::all(vec![subject(TextOp::Is, "never")]),
            vec![Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("INBOX").unwrap()),
                copy: false,
            }],
        ),
    ]
}

fn unplaced(rules: &[MailRule]) -> Vec<MailRule> {
    rules
        .iter()
        .cloned()
        .map(|mut rule| {
            rule.origin = None;
            rule
        })
        .collect()
}

#[tokio::test]
async fn live_rules_are_written_guarded_read_back_typed_and_refused_whole() {
    let Some(harness) = sieve_common::harness("live_rules") else {
        return;
    };
    let carol = provider(&harness, &harness.scratch[1]).await;
    let raw = client(&harness, &harness.scratch[1]).await;
    let (prior, _) = scripts(&raw).await;
    let prior_reply = reply_object(&raw).await;
    let checked = AssertUnwindSafe(exercise(rules(&carol), &raw, &prior))
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

async fn exercise(rules: &dyn MailRules, raw: &JmapClient, prior: &[sieve_common::Listed]) {
    assert!(
        prior.is_empty(),
        "the suite starts only where Carol has no scripts, so it changes none of hers: {prior:?}"
    );
    assert_eq!(rules.rules_write_guard(), WriteGuard::Enforced);
    let vocabulary = rules.rule_vocabulary(&account()).await.unwrap();
    assert_eq!(vocabulary.max_redirects(), Some(1), "maxNumberRedirects");
    for feature in [
        RuleFeature::FileIntoId,
        RuleFeature::FileIntoRole,
        RuleFeature::MarkImportance,
    ] {
        assert!(vocabulary.support(feature).is_some(), "{feature:?}");
    }

    // A first write creates `rules`, activates it, and every rule reads back typed as written.
    let marker = sieve_common::marker("rules");
    let empty = rules.read_rules(&account()).await.unwrap();
    assert_eq!(
        empty.rules,
        Vec::new(),
        "no rules script of the engine's yet"
    );
    let wanted = every_extension(&marker);
    let receipt = rules
        .replace_rules(&account(), &empty, &wanted)
        .await
        .expect("a write built on the version just read");
    assert!(
        receipt
            .outcomes
            .iter()
            .all(|outcome| outcome.kept == Kept::Whole),
        "{:?}",
        receipt.outcomes
    );
    assert_eq!(receipt.stored, RulesStored::CreatedRunning);
    let read = rules.read_rules(&account()).await.unwrap();
    assert_eq!(
        read.version, receipt.version,
        "the receipt's version is the rules'"
    );
    assert_eq!(read.activation, RulesActivation::Running);
    let mut expected = unplaced(&wanted);
    // `fileinto "INBOX"` names the inbox, which reads back as its role.
    expected[3] = MailRule::typed(
        Match::all(vec![subject(TextOp::Is, "never")]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Inbox),
            copy: false,
        }],
    );
    assert_eq!(unplaced(&read.rules), expected, "read back typed");
    let (listed, state) = scripts(raw).await;
    let ours: Vec<_> = listed
        .iter()
        .filter(|(_, name, ..)| name == "rules")
        .collect();
    assert_eq!(ours.len(), 1, "{listed:?}");
    assert!(ours[0].2, "the rules script runs");
    let script = text(raw, &ours[0].3).await;
    eprintln!("the engine wrote:\n{script}");

    // Stale: a write built on the empty read is refused, and lands nothing.
    let err = rules
        .replace_rules(&account(), &empty, &[])
        .await
        .expect_err("a stale version is refused");
    assert_eq!(err.class(), FailureClass::Conflict);
    assert_eq!(
        scripts(raw).await.1,
        state,
        "the refused write moved nothing"
    );

    // Current: an edit to one rule leaves the others' bytes as they were.
    let mut edited = read.rules.clone();
    let RuleBody::Typed(last) = &mut edited[3].body else {
        unreachable!()
    };
    last.when = Match::all(vec![subject(TextOp::Is, "still never")]);
    let receipt = rules
        .replace_rules(&account(), &read, &edited)
        .await
        .unwrap();
    let again = rules.read_rules(&account()).await.unwrap();
    assert_eq!(again.version, receipt.version);
    let after = text(
        raw,
        &scripts(raw)
            .await
            .0
            .iter()
            .find(|s| s.1 == "rules")
            .unwrap()
            .3,
    )
    .await;
    let (was, now) = (
        engine_sieve::segment(&script).unwrap(),
        engine_sieve::segment(&after).unwrap(),
    );
    assert_eq!(was.len(), now.len());
    for (i, (was, now)) in was.iter().zip(&now).enumerate() {
        // Segment 0 is the require; the edited rule is the fourth rule, segment 4.
        if i != 4 {
            assert_eq!(was.text(), now.text(), "segment {i} is byte-identical");
        }
    }

    // Atomic: an invalid create that asks to be activated stores and activates nothing.
    let (listed, state) = scripts(raw).await;
    let refused = sieve_set(
        raw,
        json!({ "create": { "s1": { "name": "live-rules-invalid", "blobId": "#b1" } },
                "onSuccessActivateScript": "#s1" }),
        Some("require \"regex\";\r\nif header :regex \"subject\" \"(\" { discard; }\r\n"),
    )
    .await;
    eprintln!("an invalid create: {refused}");
    assert_eq!(
        refused["notCreated"]["s1"]["type"], "invalidScript",
        "STALWART-5: not invalidSieve"
    );
    assert_eq!(
        scripts(raw).await,
        (listed.clone(), state.clone()),
        "nothing stored or switched"
    );

    // And an invalid update of the running script leaves its bytes and its flag as they were.
    let running = listed.iter().find(|s| s.2).unwrap().clone();
    let refused = sieve_set(
        raw,
        json!({ "update": { running.0.as_str(): { "blobId": "#b1" } } }),
        Some("if header :bogus \"subject\" \"x\" { discard; }\r\n"),
    )
    .await;
    assert!(
        refused["notUpdated"][running.0.as_str()].is_object(),
        "{refused}"
    );
    assert_eq!(scripts(raw).await, (listed.clone(), state.clone()));
    assert_eq!(
        text(raw, &running.3).await,
        after,
        "the script's bytes are unchanged"
    );

    // Through the adapter: a rule the server refuses is `item_unavailable`, and nothing moved.
    let current = rules.read_rules(&account()).await.unwrap();
    let mut invalid = current.rules.clone();
    invalid.push(MailRule::typed(
        Match::all(vec![subject(TextOp::Regex, "(")]),
        vec![Action::Discard],
    ));
    let err = rules
        .replace_rules(&account(), &current, &invalid)
        .await
        .expect_err("an invalid regex is refused by the server");
    assert!(err.is_item_unavailable(), "{err}");
    assert!(
        !err.to_string().contains("parenthesis"),
        "no server text: {err}"
    );
    assert_eq!(scripts(raw).await, (listed, state));

    // The vocabulary is the server's: what the writer used was accepted above, and an extension
    // the server does not list is refused. Stalwart takes an unknown name in `require` silently
    // and refuses only a known one it lacks (STALWART-8), so `imapsieve` is the probe that can
    // fail.
    let refused = validate(raw, "require \"imapsieve\";\r\nkeep;\r\n").await;
    assert_eq!(refused["error"]["type"], "invalidScript", "{refused}");
    let accepted = validate(raw, &script).await;
    assert_eq!(
        accepted["error"],
        json!(null),
        "the engine's own script validates"
    );
}
