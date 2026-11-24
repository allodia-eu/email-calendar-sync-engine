//! Rules and the reply imported over a scripted ManageSieve server: appended after the script's
//! own rules through the edit's one write path, a folder named by id lost, and the reply's guard.

use engine_core::{
    mail::{Keyword, SystemKeyword},
    rules::{
        Action, Condition, HeaderField, Kept, LossReason, MailboxPath, Match, RuleAddress,
        RuleMailbox, RuleName, RulePart, RulesActivation, RulesStored, TextOp, TextTest,
    },
    vacation::AutoReply,
};

use super::*;
use crate::managesieve::test_support::{capability, listing, script, scripted_sessions, sent};

const LOGOUT: &[u8] = b"OK\r\n";

/// The account's own script: one rule of the user's, stored but not running.
const OWN: &str = "require [\"fileinto\"];\r\n# rule:[Mine]\r\nif header :contains \"subject\" \
                   \"mine\"\r\n{\r\n\tfileinto \"Mine\";\r\n}\r\n";

fn account() -> AccountId {
    AccountId::try_from("sieve").unwrap()
}

fn named(name: &str, actions: Vec<Action>) -> MailRule {
    let test = TextTest::new(TextOp::Contains, vec![name.to_owned()]).unwrap();
    let when = Match::all(vec![Condition::Header {
        field: HeaderField::Subject,
        test,
    }]);
    MailRule::typed(when, actions).named(RuleName::new(name).unwrap())
}

#[tokio::test]
async fn rules_are_appended_after_the_scripts_own_and_a_folder_named_by_id_is_lost() {
    let extensions = "fileinto copy imap4flags mailboxid vacation";
    let (sieve, sessions) = scripted_sessions(&[
        &[&capability(extensions), LOGOUT],
        &[&listing(&[("mine", false)]), &script(OWN), LOGOUT],
        &[
            &capability(extensions),
            &listing(&[("mine", false)]),
            &script(OWN),
            b"OK\r\n",
            &listing(&[("mine", false)]),
            &script(OWN),
            LOGOUT,
        ],
    ]);
    let seen = Action::AddKeywords([Keyword::system(SystemKeyword::Seen)].into());
    let rules = vec![
        named(
            "Forward",
            vec![Action::Redirect {
                to: vec![RuleAddress::new("away@example.com").unwrap()],
                copy: false,
            }],
        ),
        named(
            "ById",
            vec![
                Action::FileInto {
                    mailbox: RuleMailbox::Id(engine_core::ids::MailboxId::try_from("Ab").unwrap()),
                    copy: false,
                },
                seen,
            ],
        ),
        named(
            "Path",
            vec![Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("Archive").unwrap()),
                copy: false,
            }],
        ),
    ];
    let receipt = sieve
        .import_rules(&account(), &rules, ForwardingRules::Disabled)
        .await
        .unwrap();
    let kept: Vec<Kept> = receipt
        .outcomes
        .iter()
        .map(|outcome| outcome.kept)
        .collect();
    assert_eq!(kept, [Kept::Disabled, Kept::Reduced, Kept::Whole]);
    let lost: Vec<(RulePart, LossReason)> = receipt.outcomes[1]
        .losses
        .iter()
        .map(|loss| (loss.part, loss.reason))
        .collect();
    assert_eq!(lost, [(RulePart::Action(0), LossReason::MissingMailbox)]);
    assert_eq!(receipt.stored, RulesStored::InPlace);
    assert_eq!(
        receipt.activation,
        RulesActivation::Inactive,
        "stored where the account's rules are, and switched off as they are"
    );

    let sent = sent(&sessions);
    let own_rules = &OWN[OWN.find("# rule").unwrap()..];
    let at = sent
        .find(own_rules)
        .expect("the account's own rule, byte for byte");
    let added = &sent[at + own_rules.len()..];
    assert!(sent.contains("PUTSCRIPT \"mine\" {"), "{sent}");
    assert!(
        added.starts_with("# rule:[Forward]\r\nif false # header"),
        "{added}"
    );
    assert!(!added.contains(":mailboxid"), "{added}");
    assert!(!sent.contains("SETACTIVE"), "never switched on: {sent}");
}

#[tokio::test]
async fn a_reply_of_the_scripts_own_is_never_replaced() {
    let held = "require [\"vacation\"];\r\n# rule:[Out-of-Office]\r\nif true\r\n{\r\n\tvacation \
                \"Gone fishing.\";\r\n}\r\n";
    let (sieve, sessions) = scripted_sessions(&[
        &[&capability("fileinto vacation date relational"), LOGOUT],
        &[&listing(&[("rules", true)]), &script(held), LOGOUT],
    ]);
    let reply = VacationResponse::default()
        .turned_on()
        .with_reply(AutoReply::default().with_text("Away."));
    let err = sieve.import_vacation(&account(), &reply).await.unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(!sent(&sessions).contains("PUTSCRIPT"), "nothing written");
}
