//! Gated live integration: the user's rules and the out-of-office reply together on Stalwart,
//! where the reply is a script of its own and only one script runs (RFC 9661 §2.1, §4). It
//! proves, by delivering mail, what the design rests on (`filters-and-out-of-office.md`,
//! "Settled by live tests"):
//!
//! - the server keeps the reply in a script named `vacation`, the name the adapter includes;
//! - rules written while the reply runs keep it running: the rules script runs, includes the
//!   reply's first, and a message a rule stops at or discards is still answered;
//! - the reply set while the rules run is written into the reply's script and switched on by the
//!   include, and reads back on though the server's own flag says off; switched off, the include
//!   goes, and no reply is sent;
//! - a mark before a `fileinto` reaches the filed copy, as the writer orders them, and a mark after
//!   it is recorded as the server applies it.
//!
//! Runs as the scratch account `carol@` (`sieve_common` says why that is safe beside
//! `live_vacation`), with `bob@` sending, and restores both. About sixty requests and five
//! deliveries; a run waits up to a minute for mail.

#[path = "sieve_common/mod.rs"]
mod sieve_common;

use std::{
    panic::{AssertUnwindSafe, resume_unwind},
    time::Duration,
};

use engine_core::{
    mail::{Keyword, SystemKeyword},
    rules::{
        Action, Condition, HeaderField, MailRule, MailboxPath, Match, RuleBody, RuleMailbox,
        RuleName, TextOp, TextTest,
    },
    vacation::{AutoReply, RulesEffect, VacationResponse},
};
use engine_provider::{Provider, Vacation, VacationPrecondition::IfUnchanged};
use futures_util::FutureExt;
use provider_jmap::{JmapClient, JmapProvider};
use sieve_common::{
    account, client, create_folder, messages, provider, remove_folder, remove_messages,
    reply_object, restore, rules, scripts, send, text,
};
use stalwart_harness::ScratchAccount;

const WAIT: Duration = Duration::from_secs(20);

fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap(),
    }
}

fn away(subject: &str) -> VacationResponse {
    // A reply is sent once per sender for each distinct reply (RFC 5230 §4.2, the `:handle` a
    // server derives from it), so each step's reply differs in its words too.
    VacationResponse::default().turned_on().with_reply(
        AutoReply::default()
            .with_subject(subject)
            .with_text(format!("Away ({subject}).")),
    )
}

struct Run<'a> {
    carol: &'a JmapProvider,
    carol_raw: &'a JmapClient,
    bob: &'a JmapProvider,
    bob_raw: &'a JmapClient,
    bob_who: &'a ScratchAccount,
    carol_who: &'a ScratchAccount,
    marker: String,
    folder: String,
}

impl Run<'_> {
    fn vacation(&self) -> &dyn Vacation {
        self.carol.vacation().expect("Carol has a reply")
    }

    async fn set_reply(&self, reply: &VacationResponse) {
        let read = self.vacation().read_vacation(&account()).await.unwrap();
        let receipt = self
            .vacation()
            .set_vacation(&account(), IfUnchanged(&read), reply)
            .await
            .expect("the reply is written");
        assert_eq!(
            receipt.rules,
            RulesEffect::Untouched,
            "the rules keep running"
        );
    }

    async fn deliver(&self, subject: &str) {
        send(self.bob, self.bob_who, self.carol_who, subject).await;
    }

    async fn replied(&self, reply_subject: &str, wait: Duration) -> bool {
        !messages(self.bob_raw, reply_subject, wait).await.is_empty()
    }
}

#[tokio::test]
async fn live_rules_and_the_reply_run_together() {
    let Some(harness) = sieve_common::harness("live_rules_reply") else {
        return;
    };
    let [bob_who, carol_who] = harness.scratch.clone();
    let carol = provider(&harness, &carol_who).await;
    let carol_raw = client(&harness, &carol_who).await;
    let bob = provider(&harness, &bob_who).await;
    let bob_raw = client(&harness, &bob_who).await;
    let (prior, _) = scripts(&carol_raw).await;
    let marker = sieve_common::marker("reply");
    let folder = format!("{marker}-folder");
    let folder_id = create_folder(&carol_raw, &folder).await;
    let run = Run {
        carol: &carol,
        carol_raw: &carol_raw,
        bob: &bob,
        bob_raw: &bob_raw,
        bob_who: &bob_who,
        carol_who: &carol_who,
        marker: marker.clone(),
        folder,
    };
    let checked = AssertUnwindSafe(exercise(&run, &prior))
        .catch_unwind()
        .await;
    let restored = restore(&carol_raw, &prior).await;
    remove_folder(&carol_raw, &folder_id).await;
    remove_messages(&carol_raw, &marker).await;
    remove_messages(&bob_raw, &marker).await;
    if let Err(failed) = checked {
        if let Err(problem) = restored {
            eprintln!("and restoring Carol failed too: {problem}");
        }
        resume_unwind(failed);
    }
    restored.expect("Carol is left as she was found");
}

async fn exercise(run: &Run<'_>, prior: &[sieve_common::Listed]) {
    assert!(
        prior.iter().all(|(_, _, active, _)| !active),
        "the suite starts with none of Carol's scripts running: {prior:?}"
    );
    let m = &run.marker;

    // The reply's script is called `vacation`, the name the adapter includes.
    run.set_reply(&away(&format!("{m}-away1"))).await;
    let (listed, _) = scripts(run.carol_raw).await;
    let active: Vec<&str> = listed
        .iter()
        .filter(|s| s.2)
        .map(|s| s.1.as_str())
        .collect();
    assert_eq!(
        active,
        ["vacation"],
        "the reply runs as a script named vacation"
    );

    // Rules written while the reply runs: the rules script runs, the reply's first in it.
    let seen = Keyword::system(SystemKeyword::Seen);
    let probe = Keyword::new("$rulesprobe").unwrap();
    let folder = MailboxPath::new(run.folder.as_str()).unwrap();
    let late = engine_sieve::dialect::carried(&format!(
        "if header :contains \"subject\" \"{m}-late\" {{ fileinto \"{}\"; addflag \"\\\\Seen\"; }}\r\n",
        run.folder
    ))
    .unwrap();
    let wanted = vec![
        MailRule::typed(
            Match::all(vec![subject(&format!("{m}-stop"))]),
            vec![Action::AddKeywords([probe.clone()].into())],
        )
        .named(RuleName::new("Stops").unwrap())
        .stopping(),
        MailRule::typed(
            Match::all(vec![subject(&format!("{m}-discard"))]),
            vec![Action::Discard],
        ),
        MailRule::typed(
            Match::all(vec![subject(&format!("{m}-file"))]),
            vec![
                Action::FileInto {
                    mailbox: RuleMailbox::Path(folder),
                    copy: false,
                },
                Action::AddKeywords([seen.clone()].into()),
            ],
        ),
        MailRule::opaque(late),
    ];
    let rules = rules(run.carol);
    let empty = rules.read_rules(&account()).await.unwrap();
    let receipt = rules
        .replace_rules(&account(), &empty, &wanted)
        .await
        .expect("rules written");
    assert!(
        receipt
            .outcomes
            .iter()
            .all(|outcome| outcome.kept == engine_core::rules::Kept::Whole),
        "{:?}",
        receipt.outcomes
    );
    let (listed, _) = scripts(run.carol_raw).await;
    let running: Vec<_> = listed.iter().filter(|s| s.2).collect();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].1, "rules", "the rules script runs");
    let script = text(run.carol_raw, &running[0].3).await;
    eprintln!("the rules script:\n{script}");
    let first = engine_sieve::segment(&script).unwrap();
    assert!(
        first[1]
            .command()
            .starts_with("include :personal :optional \"vacation\""),
        "the reply's script is included first: {script}"
    );
    let reply = run.vacation().read_vacation(&account()).await.unwrap();
    assert!(reply.enabled, "the reply reads as on");
    assert_eq!(
        reply_object(run.carol_raw).await["isEnabled"],
        false,
        "the server's flag says off"
    );
    assert_eq!(
        rules.read_rules(&account()).await.unwrap().rules.len(),
        wanted.len(),
        "the include is no rule of the user's"
    );

    // A message a rule stops at is still answered, and the rule ran.
    run.deliver(&format!("{m}-stop")).await;
    assert!(
        run.replied(&format!("{m}-away1"), WAIT).await,
        "answered, though a rule stopped"
    );
    let stopped = messages(run.carol_raw, &format!("{m}-stop"), WAIT).await;
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].1["$rulesprobe"], true, "the stopping rule ran");

    // The reply set while the rules run: its words change, it stays on, the rules keep running.
    run.set_reply(&away(&format!("{m}-away2"))).await;
    assert_eq!(
        scripts(run.carol_raw)
            .await
            .0
            .iter()
            .filter(|s| s.2)
            .count(),
        1
    );
    // A discarded message is answered too: the include runs before the rule.
    run.deliver(&format!("{m}-discard")).await;
    assert!(
        run.replied(&format!("{m}-away2"), WAIT).await,
        "answered, though a rule discards"
    );
    assert_eq!(
        messages(
            run.carol_raw,
            &format!("{m}-discard"),
            Duration::from_secs(3)
        )
        .await
        .len(),
        0
    );

    // A mark written before the fileinto reaches the filed copy. One written after it misses
    // the copy, as RFC 5232 §3 says (observed on Stalwart v1.0.2): why the writer puts marks
    // first, and reads only that order typed.
    run.deliver(&format!("{m}-file")).await;
    run.deliver(&format!("{m}-late")).await;
    let filed = messages(run.carol_raw, &format!("{m}-file"), WAIT).await;
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0].1["$seen"], true, "the filed copy is marked read");
    let late = messages(run.carol_raw, &format!("{m}-late"), WAIT).await;
    assert_eq!(late.len(), 1);
    assert_eq!(
        late[0].1.get("$seen"),
        None,
        "a mark after the fileinto misses the copy"
    );

    // Off while the rules run: the include goes, and nothing is answered.
    run.set_reply(&away(&format!("{m}-away3")).turned_off())
        .await;
    let reply = run.vacation().read_vacation(&account()).await.unwrap();
    assert!(!reply.enabled, "the reply reads as off");
    let running = scripts(run.carol_raw)
        .await
        .0
        .into_iter()
        .find(|s| s.2)
        .unwrap();
    let script = text(run.carol_raw, &running.3).await;
    // The require keeps naming `include` (a require never drops a name); the command is gone.
    assert!(
        engine_sieve::segment(&script)
            .unwrap()
            .iter()
            .all(|segment| segment.identifier() != Some("include")),
        "the include is gone: {script}"
    );
    run.deliver(&format!("{m}-quiet")).await;
    assert!(
        !messages(run.carol_raw, &format!("{m}-quiet"), WAIT)
            .await
            .is_empty(),
        "delivered"
    );
    assert!(
        !run.replied(&format!("{m}-away3"), Duration::from_secs(8))
            .await,
        "no reply once it is off"
    );
    let RuleBody::Typed(_) = &rules.read_rules(&account()).await.unwrap().rules[0].body else {
        panic!("the rules read back typed")
    };
}
