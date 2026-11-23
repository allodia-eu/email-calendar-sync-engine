//! The scripts as listed, and the one answer to "does the reply run", over every account state
//! the design names: no scripts, the reply only, the rules only, the rules with the include, and
//! the include of a reply script someone destroyed.

use serde_json::json;

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
fn an_include_of_the_reply_is_recognised_however_it_is_written_and_only_at_the_top() {
    for (text, includes) in [
        (WITH_INCLUDE, true),
        ("include \"vacation\";", true),
        ("INCLUDE :global \"vacation\";", true),
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
