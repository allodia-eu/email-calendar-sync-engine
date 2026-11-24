//! The rules over a scripted ManageSieve server: what is read, what is sent, and what each refusal
//! becomes.

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    rules::{Kept, RuleSet, RuleVocabulary, RulesActivation, RulesStored},
};
use engine_provider::{MailRules, TargetRefusal};

use super::*;
use crate::managesieve::test_support::{
    STALWART_REPLY, capability, listing, script, scripted, sent,
};

const RULE: &str = "require [\"fileinto\"];\r\n# rule:[Fixture]\r\nif header :contains \
                    \"subject\" \"zfixture\"\r\n{\r\n\tfileinto \"Archive\";\r\n}\r\n";
const LOGOUT: &[u8] = b"OK\r\n";

fn account() -> AccountId {
    AccountId::try_from("sieve").unwrap()
}

async fn read_set(answers: &[&[u8]]) -> RuleSet {
    let (sieve, _) = scripted(answers);
    sieve.read_rules(&account()).await.unwrap()
}

#[tokio::test]
async fn the_running_script_is_read_as_the_rules_with_a_version() {
    let (sieve, sessions) = scripted(&[
        &listing(&[("other", false), ("zfixture", true)]),
        &script(RULE),
        LOGOUT,
    ]);
    let set = sieve.read_rules(&account()).await.unwrap();
    assert_eq!(set.rules.len(), 1);
    assert_eq!(set.activation, RulesActivation::Running);
    assert!(set.version.is_some());
    assert_eq!(
        set.raw.as_ref().map(engine_core::raw::RawRules::as_str),
        Some(RULE)
    );
    assert_eq!(
        sent(&sessions),
        "LISTSCRIPTS\r\nGETSCRIPT \"zfixture\"\r\nLOGOUT\r\n"
    );
    assert_eq!(sieve.rules_write_guard(), WriteGuard::Absent);

    // Stored but not running: the only script, inactive.
    let set = read_set(&[&listing(&[("mine", false)]), &script(RULE), LOGOUT]).await;
    assert_eq!(set.activation, RulesActivation::Inactive);
    // None at all: no rules, running as a first write would make them.
    let set = read_set(&[&listing(&[]), LOGOUT]).await;
    assert!(set.rules.is_empty() && set.activation == RulesActivation::Running);
}

#[tokio::test]
async fn the_vocabulary_is_what_the_server_lists_once_signed_in() {
    let (sieve, sessions) = scripted(&[&capability("fileinto copy"), LOGOUT]);
    let vocabulary: RuleVocabulary = sieve.rule_vocabulary(&account()).await.unwrap();
    assert_eq!(vocabulary.max_redirects(), Some(4));
    assert_eq!(sent(&sessions), "CAPABILITY\r\nLOGOUT\r\n");
}

/// A base read from the scripted server, and the answers a write over it then gets.
async fn base(script_text: &str) -> RuleSet {
    read_set(&[
        &listing(&[("zfixture", true)]),
        &script(script_text),
        LOGOUT,
    ])
    .await
}

#[tokio::test]
async fn a_rule_changed_is_stored_in_place_over_the_script_read() {
    let base = base(RULE).await;
    let disabled = vec![base.rules[0].clone().disabled()];
    let (sieve, sessions) = scripted(&[
        &capability("fileinto"),
        &listing(&[("zfixture", true)]),
        &script(RULE),
        b"OK\r\n",
        &listing(&[("zfixture", true)]),
        &script(RULE),
        LOGOUT,
    ]);
    let receipt = sieve
        .replace_rules(&account(), &base, &disabled)
        .await
        .unwrap();
    assert_eq!(receipt.stored, RulesStored::InPlace);
    assert_eq!(receipt.outcomes[0].kept, Kept::Whole);
    assert!(receipt.version.is_some());
    let sent = sent(&sessions);
    assert!(
        sent.contains("PUTSCRIPT \"zfixture\" {") && sent.contains("if false # header"),
        "{sent}"
    );
    assert!(
        !sent.contains("SETACTIVE"),
        "the running script stays as it was"
    );
}

#[tokio::test]
async fn an_account_with_no_script_gets_rules_created_and_switched_on() {
    let empty = read_set(&[&listing(&[]), LOGOUT]).await;
    let rules = base(RULE).await.rules;
    let (sieve, sessions) = scripted(&[
        &capability("fileinto"),
        &listing(&[]),
        b"OK\r\n",
        b"OK\r\n",
        &listing(&[("rules", true)]),
        &script(RULE),
        LOGOUT,
    ]);
    let receipt = sieve
        .replace_rules(&account(), &empty, &rules)
        .await
        .unwrap();
    assert_eq!(receipt.stored, RulesStored::CreatedRunning);
    let sent = sent(&sessions);
    assert!(sent.contains("PUTSCRIPT \"rules\" {"), "{sent}");
    assert!(sent.contains("SETACTIVE \"rules\"\r\n"), "{sent}");
}

#[tokio::test]
async fn a_base_built_rather_than_read_is_refused_and_nothing_is_stored() {
    // `RuleSet::new(rules)` carries no version: written, it would delete every stored rule unseen.
    let (sieve, sessions) = scripted(&[
        &capability("fileinto"),
        &listing(&[("zfixture", true)]),
        &script(RULE),
        LOGOUT,
    ]);
    let err = sieve
        .replace_rules(&account(), &RuleSet::new(Vec::new()), &[])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(!err.is_item_unavailable());
    assert!(!sent(&sessions).contains("PUTSCRIPT"));
}

#[tokio::test]
async fn a_base_read_before_the_script_moved_is_refused_and_nothing_is_stored() {
    let base = base(RULE).await;
    let moved = RULE.replace("Archive", "Elsewhere");
    let (sieve, sessions) = scripted(&[
        &capability("fileinto"),
        &listing(&[("zfixture", true)]),
        &script(&moved),
        LOGOUT,
    ]);
    let err = sieve
        .replace_rules(&account(), &base, &base.rules)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(!sent(&sessions).contains("PUTSCRIPT"));
}

#[tokio::test]
async fn each_refusal_of_the_store_is_classed_by_its_code() {
    let base = base(RULE).await;
    for (answer, check) in [
        (
            b"NO \"line 3: error\"\r\n".as_slice(),
            (|err: &ProviderError| err.is_item_unavailable()) as fn(&ProviderError) -> bool,
        ),
        (b"NO (QUOTA/MAXSIZE) \"too big\"\r\n", |err| {
            err.is_item_unavailable()
        }),
        (b"NO (QUOTA/MAXSCRIPTS) \"too many\"\r\n", |err| {
            err.target_refusal() == Some(TargetRefusal::StorageFull)
        }),
        (b"NO (TRYLATER) \"busy\"\r\n", |err| {
            err.class() == FailureClass::Retryable
        }),
        // The connection went away with the script sent: it may have been stored.
        (b"", |err| err.requires_confirmation()),
    ] {
        let (sieve, _) = scripted(&[
            &capability("fileinto"),
            &listing(&[("zfixture", true)]),
            &script(RULE),
            answer,
        ]);
        let err = sieve
            .replace_rules(&account(), &base, &base.rules)
            .await
            .unwrap_err();
        assert!(check(&err), "{answer:?}: {err:?}");
        assert!(!err.to_string().contains("line 3") && !err.to_string().contains("big"));
    }
}

#[tokio::test]
async fn a_script_stored_and_not_switched_on_may_need_a_second_look() {
    let empty = read_set(&[&listing(&[]), LOGOUT]).await;
    let (sieve, _) = scripted(&[
        &capability("fileinto"),
        &listing(&[]),
        b"OK\r\n",
        b"NO \"cannot\"\r\n",
    ]);
    let err = sieve
        .replace_rules(&account(), &empty, &base(RULE).await.rules)
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");
}

#[tokio::test]
async fn the_reply_s_rule_is_no_rule_and_is_kept_by_a_write_of_the_rules() {
    let reply = "# rule:[Out-of-Office]\r\nif true\r\n{\r\n\tvacation \"Away.\";\r\n}\r\n";
    let both = format!(
        "require [\"fileinto\",\"vacation\"];\r\n{reply}{}",
        &RULE[23..]
    );
    let base = base(&both).await;
    assert_eq!(base.rules.len(), 1, "the reply is not among the rules");
    let (sieve, sessions) = scripted(&[
        &capability("fileinto vacation"),
        &listing(&[("zfixture", true)]),
        &script(&both),
        b"OK\r\n",
        &listing(&[("zfixture", true)]),
        &script(&both),
        LOGOUT,
    ]);
    let _ = sieve.replace_rules(&account(), &base, &[]).await.unwrap();
    let sent = sent(&sessions);
    assert!(sent.contains(reply), "{sent}");
    assert!(
        !sent.contains("fileinto \"Archive\""),
        "the rule is gone: {sent}"
    );
}

#[tokio::test]
async fn rules_are_not_written_into_a_script_a_server_keeps_its_reply_in() {
    // Stalwart's own reply script, running: rules written there are undone by its next reply write.
    let base = read_set(&[
        &listing(&[("vacation", true)]),
        &script(STALWART_REPLY),
        LOGOUT,
    ])
    .await;
    let rules = base_rules().await;
    let (sieve, sessions) = scripted(&[
        &capability("fileinto vacation"),
        &listing(&[("vacation", true)]),
        &script(STALWART_REPLY),
        LOGOUT,
    ]);
    let err = sieve
        .replace_rules(&account(), &base, &rules)
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(!sent(&sessions).contains("PUTSCRIPT"));

    // A Roundcube-named rule of the user's that replies does not stop a write.
    let boss = format!(
        "require [\"fileinto\",\"vacation\"];\r\n# rule:[Boss]\r\nif header :contains \
         \"from\" \"boss\"\r\n{{\r\n\tvacation \"In a meeting.\";\r\n}}\r\n{}",
        &RULE[23..]
    );
    let base = read_set(&[&listing(&[("zfixture", true)]), &script(&boss), LOGOUT]).await;
    let (sieve, sessions) = scripted(&[
        &capability("fileinto vacation"),
        &listing(&[("zfixture", true)]),
        &script(&boss),
        b"OK\r\n",
        &listing(&[("zfixture", true)]),
        &script(&boss),
        LOGOUT,
    ]);
    let _ = sieve
        .replace_rules(&account(), &base, &base.rules)
        .await
        .unwrap();
    assert!(sent(&sessions).contains("PUTSCRIPT"));
}

async fn base_rules() -> Vec<MailRule> {
    base(RULE).await.rules
}
