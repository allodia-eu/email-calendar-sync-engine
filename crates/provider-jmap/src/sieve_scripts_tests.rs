//! The scripts as listed, and the one answer to "does the reply run", over every account state
//! the design names: no scripts, the reply only, the rules only, the rules with the include, and
//! the include of a reply script someone destroyed.

use serde_json::{Value, json};

use super::*;
use crate::provider::provider_test_support::fixture;

fn script(id: &str, name: &str, active: bool) -> Script {
    Script {
        id: id.to_owned(),
        name: name.to_owned(),
        active,
        blob: format!("blob-{id}"),
    }
}

fn scripts(list: Vec<Script>) -> Scripts {
    Scripts {
        state: "s1".to_owned(),
        list,
    }
}

const WITH_INCLUDE: &str =
    "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\nkeep;\r\n";

#[test]
fn a_captured_listing_reads_as_the_scripts_it_names() {
    let doc = fixture("sieve_get_rules_and_reply_response.json");
    let listed = parse(&doc["methodResponses"][0][1]).unwrap();
    assert_eq!(listed.state, "steaq");
    assert_eq!(listed.reply().map(|s| s.active), Some(false));
    assert_eq!(
        listed.rules_running().map(|s| s.name.as_str()),
        Some("rules")
    );
    assert_eq!(listed.rules().map(|s| s.id.as_str()), Some("bp"));
    for malformed in [
        json!({ "list": [] }),
        json!({ "state": "s", "list": [{ "id": "a" }] }),
        json!({ "state": "s" }),
    ] {
        assert!(parse(&malformed).is_err(), "{malformed}");
    }
}

#[test]
fn the_reply_runs_where_its_script_is_active_or_the_running_rules_include_it() {
    // No scripts at all.
    assert!(!reply_runs(&scripts(Vec::new()), None));
    // The reply alone, active and not.
    assert!(reply_runs(
        &scripts(vec![script("v", REPLY_SCRIPT, true)]),
        None
    ));
    assert!(!reply_runs(
        &scripts(vec![script("v", REPLY_SCRIPT, false)]),
        None
    ));
    // The rules alone, running, with no reply script.
    let rules_only = scripts(vec![script("r", RULES_SCRIPT, true)]);
    assert!(!reply_runs(&rules_only, Some("keep;\r\n")));
    // The include of a reply script someone destroyed: nothing to run.
    assert!(!reply_runs(&rules_only, Some(WITH_INCLUDE)));
    // The rules running and including the reply's script.
    let both = scripts(vec![
        script("v", REPLY_SCRIPT, false),
        script("r", RULES_SCRIPT, true),
    ]);
    assert!(reply_runs(&both, Some(WITH_INCLUDE)));
    assert!(!reply_runs(&both, Some("keep;\r\n")));
    // An include in a script that does not run starts nothing.
    let idle = scripts(vec![
        script("v", REPLY_SCRIPT, false),
        script("r", RULES_SCRIPT, false),
    ]);
    assert!(!reply_runs(&idle, Some(WITH_INCLUDE)));
}

#[test]
fn the_rules_script_is_the_running_one_else_the_engine_s_or_the_only_other() {
    let running = scripts(vec![
        script("v", REPLY_SCRIPT, false),
        script("a", "mine", true),
        script("r", RULES_SCRIPT, false),
    ]);
    assert_eq!(running.rules().map(|s| s.id.as_str()), Some("a"));
    let reply_runs_alone = scripts(vec![
        script("v", REPLY_SCRIPT, true),
        script("a", "mine", false),
        script("r", RULES_SCRIPT, false),
    ]);
    assert_eq!(reply_runs_alone.rules().map(|s| s.id.as_str()), Some("r"));
    let one_other = scripts(vec![
        script("v", REPLY_SCRIPT, true),
        script("a", "mine", false),
    ]);
    assert_eq!(one_other.rules().map(|s| s.id.as_str()), Some("a"));
    let several = scripts(vec![script("a", "one", false), script("b", "two", false)]);
    assert_eq!(
        several.rules(),
        None,
        "the engine does not pick one for the user"
    );
    assert_eq!(scripts(Vec::new()).rules(), None);
}

#[test]
fn an_include_of_the_reply_is_one_of_the_user_s_own_scripts_and_only_at_the_top() {
    for (text, includes) in [
        (WITH_INCLUDE, true),
        ("include \"vacation\";", true),
        ("INCLUDE :personal :once \"vacation\";", true),
        // A global script of that name is not the user's reply.
        ("include :global \"vacation\";", false),
        ("include :personal :global \"vacation\";", false),
        ("include \"other\";", false),
        ("if true { include \"vacation\"; }", false),
        ("include \"vacation\" \"more\";", false),
        ("keep;", false),
        ("not a script {", false),
    ] {
        assert_eq!(includes_reply(text), includes, "{text}");
    }
}

#[test]
fn the_include_is_no_rule_of_the_user_s_and_compose_puts_it_first_or_takes_it_out() {
    let rules = user_rules(WITH_INCLUDE);
    assert_eq!(rules.len(), 1, "only the keep");
    let on = compose(WITH_INCLUDE, &rules, true).unwrap();
    assert_eq!(
        on, WITH_INCLUDE,
        "an include already there is copied as it was"
    );
    let off = compose(WITH_INCLUDE, &rules, false).unwrap();
    assert_eq!(off, "require [\"include\"];\r\nkeep;\r\n");
    let added = compose("keep;\r\n", &user_rules("keep;\r\n"), true).unwrap();
    assert_eq!(
        added,
        "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\nkeep;\r\n"
    );
    assert!(compose("if true {", &[], true).is_err());
}

#[test]
fn a_whole_script_carried_as_one_rule_is_never_taken_for_the_include() {
    // A script the reader keeps whole (here: a malformed `require`) holds the include among other
    // commands; it is the user's, all of it.
    let whole = "require 1;\r\ninclude :personal \"vacation\";\r\nkeep;\r\n";
    let rules = user_rules(whole);
    assert_eq!(rules.len(), 1, "carried whole, not filtered out");
}

#[test]
fn a_refusal_names_the_rule_handed_in_not_one_shifted_by_the_include() {
    use engine_core::rules::{Condition, HeaderField, Match, TextOp, TextTest};
    let rules = vec![
        MailRule::typed(Match::default(), vec![engine_core::rules::Action::Discard]),
        MailRule::typed(
            Match::all(vec![Condition::Header {
                field: HeaderField::Subject,
                test: TextTest::new(TextOp::Is, vec!["${x}".to_owned()]).unwrap(),
            }]),
            vec![engine_core::rules::Action::Discard],
        ),
    ];
    let base = "require \"variables\";\r\n";
    let err = compose(base, &rules, true).unwrap_err();
    assert!(err.to_string().starts_with("rule 1 "), "{err}");
    let err = compose(base, &rules, false).unwrap_err();
    assert!(err.to_string().starts_with("rule 1 "), "{err}");
    assert_eq!(
        compose("if true {", &rules, false).unwrap_err().to_string(),
        "the script cannot be edited rule by rule"
    );
}

#[test]
fn the_include_is_usable_only_where_include_is_listed_and_the_states_are_one() {
    let doc = fixture("sieve_listing_with_reply_state_response.json");
    let state = doc["methodResponses"][0][1]["state"].as_str().unwrap();
    let reply_state = doc["methodResponses"][1][1]["state"].as_str();
    assert_eq!(
        Some(state),
        reply_state,
        "one counter, observed in one response"
    );
    let session = |extensions: Value| {
        let doc = json!({
            "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:sieve": {} },
            "primaryAccounts": { "urn:ietf:params:jmap:sieve": "c" },
            "accounts": { "c": { "accountCapabilities": {
                "urn:ietf:params:jmap:sieve": { "sieveExtensions": extensions }
            } } },
            "apiUrl": "https://mail.test.local/jmap/"
        });
        Session::parse(
            &doc,
            &reqwest::Url::parse("http://127.0.0.1:18080").unwrap(),
            crate::session::SessionUrlPolicy::RebaseToConnection,
            None,
        )
        .unwrap()
    };
    let with = session(json!(["include"]));
    assert!(include_usable(&with, state, reply_state));
    assert!(!include_usable(&with, state, Some("other")));
    assert!(!include_usable(&with, state, None));
    assert!(!include_usable(
        &session(json!(["fileinto"])),
        state,
        reply_state
    ));
}
