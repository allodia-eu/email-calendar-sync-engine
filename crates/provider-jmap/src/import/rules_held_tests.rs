//! What the import leaves alone and knows again: an include of a reply that does not run, a rule
//! an earlier import wrote, and a rule a host built carried whole, judged by its own text.

use engine_core::rules::{
    Action, ForwardingRules, Kept, MailRule, RuleAddress, RulesActivation, RulesStored,
};
use engine_provider::import::RulesImporter;
use serde_json::json;

use super::tests::{OWN, folders, named, over};
use crate::provider::provider_test_support::{account, fixture, sieve_listing};

#[tokio::test]
async fn an_include_of_a_reply_that_does_not_run_is_dropped_from_a_script_switched_off() {
    // The rules script is stored but not active, and so is the reply's: the include runs nothing
    // now, and an included script runs even where it is not the active one, so a write leaves it
    // out, as an edit does, rather than keep a reply that is off running once the rules are on.
    let mut idle = sieve_listing();
    idle["methodResponses"][0][1]["list"][1]["isActive"] = json!(false);
    let (provider, exec) = over(
        vec![
            idle.clone(),
            idle,
            fixture("sieve_set_update_rules_response.json"),
        ],
        OWN,
    );
    let receipt = provider
        .import_rules(
            &account(),
            &[named("Added", vec![Action::Discard])],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.activation, RulesActivation::Inactive);
    let uploads = exec.uploads.lock().unwrap();
    let text = String::from_utf8_lossy(&uploads[0].2);
    assert!(!text.contains("include :personal"), "{text}");
    let own_rule = &OWN[OWN.find("# rule:[Mine]").unwrap()..];
    assert!(
        text.contains(own_rule),
        "the account's own rule, byte for byte: {text}"
    );
}

#[tokio::test]
async fn a_forward_an_earlier_import_wrote_is_known_as_held() {
    // What a first import of a Graph `forwardTo` rule left: a redirect keeping a copy, disabled.
    let held = "require [\"copy\"];\r\n# rule:[Forward]\r\nif false # header :contains \
                \"subject\" \"Forward\"\r\n{\r\n\tredirect :copy \"away@example.com\";\r\n}\r\n";
    let (provider, exec) = over(vec![sieve_listing()], held);
    let forward = named(
        "Forward",
        vec![Action::Forward {
            to: vec![RuleAddress::new("away@example.com").unwrap()],
        }],
    );
    let receipt = provider
        .import_rules(
            &account(),
            &[forward],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::Disabled);
    assert_eq!(receipt.already_held, [0]);
    assert_eq!(receipt.stored, RulesStored::Unwritten);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0, "nothing written");
}

/// Rules a host built carried whole, whose texts name a folder by id and run another script: once
/// as a host that vouches for neither, once as one that wrongly vouches that they do neither.
fn host_built() -> Vec<MailRule> {
    use engine_core::rules::{RawRule, RuleSyntax};
    let texts = [
        (
            "fileinto :mailboxid \"a\" \"INBOX\";",
            vec!["fileinto", "mailboxid"],
        ),
        ("include :personal \"other\";", vec!["include"]),
    ];
    texts
        .iter()
        .flat_map(|(text, needs)| {
            let raw = RawRule::new(RuleSyntax::Sieve, *text)
                .requiring(needs.iter().map(|need| (*need).to_owned()))
                .sending_no_mail();
            [
                raw.clone(),
                raw.naming_no_folder_by_id().including_no_script(),
            ]
        })
        .map(MailRule::opaque)
        .collect()
}

#[tokio::test]
async fn a_host_built_rule_carried_whole_is_judged_by_its_text() {
    let (provider, exec) = over(vec![sieve_listing()], OWN);
    let receipt = provider
        .import_rules(
            &account(),
            &host_built(),
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    let kept: Vec<Kept> = receipt
        .outcomes
        .iter()
        .map(|outcome| outcome.kept)
        .collect();
    assert_eq!(kept, [Kept::NotKept; 4], "{:?}", receipt.outcomes);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0, "nothing written");
}
