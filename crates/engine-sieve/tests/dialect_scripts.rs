//! Which script holds the rules, and the include of a server's reply script, as every adapter
//! decides them (`engine_sieve::dialect::{scripts, include}`).

use engine_sieve::dialect::{
    include::{compose, include_command, included_scripts, includes_script, without_include},
    read_rules,
    scripts::{Choice, Listed, rules_script},
};

fn listed(scripts: &[(&'static str, bool, bool)]) -> Vec<Listed<'static>> {
    scripts
        .iter()
        .map(|&(name, active, server_reply)| Listed {
            name,
            active,
            server_reply,
        })
        .collect()
}

#[test]
fn the_rules_are_in_the_running_script_else_rules_else_the_only_one_never_the_server_s_reply() {
    let choose = |scripts: &[(&'static str, bool, bool)]| rules_script(&listed(scripts));
    assert_eq!(
        choose(&[("a", false, false), ("b", true, false)]),
        Choice::Script(1)
    );
    // The server's reply running: the rules are elsewhere.
    assert_eq!(
        choose(&[("vacation", true, true), ("mine", false, false)]),
        Choice::Script(1)
    );
    assert_eq!(
        choose(&[
            ("vacation", true, true),
            ("a", false, false),
            ("rules", false, false)
        ]),
        Choice::Script(2)
    );
    // Only the server's reply: a write creates `rules`, whatever the reply script is named.
    assert_eq!(choose(&[("vacation", false, true)]), Choice::Create);
    assert_eq!(choose(&[("other", true, true)]), Choice::Create);
    assert_eq!(
        choose(&[("a", false, false), ("b", false, false)]),
        Choice::Create
    );
    assert_eq!(choose(&[]), Choice::Create);
}

const INCLUDING: &str = "require [\"fileinto\",\"include\"];\r\ninclude :personal :optional \
                         \"vacation\";\r\n# rule:[A]\r\nif header :contains \"subject\" \"a\"\r\n\
                         {\r\n\tfileinto \"A\";\r\n}\r\n";

#[test]
fn the_include_of_a_reply_script_is_no_rule_and_is_kept_first_on_a_write() {
    assert!(includes_script(INCLUDING, "vacation"));
    assert!(!includes_script(INCLUDING, "other"));
    assert_eq!(included_scripts(INCLUDING), ["vacation"]);
    let rules = without_include(INCLUDING, "vacation");
    assert_eq!(rules.len(), 1);
    assert_eq!(
        read_rules(INCLUDING).len(),
        2,
        "read_rules alone sees the include as a rule"
    );
    // Written with no rules at all, the include stays, byte for byte.
    let written = compose(INCLUDING, &[], Some("vacation"), true).unwrap();
    assert!(
        written.contains("include :personal :optional \"vacation\";\r\n"),
        "{written}"
    );
    assert_eq!(without_include(&written, "vacation").len(), 0);
    // A new include goes first; none is written where there is no reply to run.
    let new = compose("", &rules, Some("vacation"), true).unwrap();
    assert!(
        new.starts_with("require [\"fileinto\",\"include\"];\r\ninclude :personal"),
        "{new}"
    );
    let none = compose(INCLUDING, &rules, Some("vacation"), false).unwrap();
    assert!(!none.contains("include :personal"), "{none}");
    // A caller handing the include back among its rules does not switch the reply on.
    let handed = read_rules(INCLUDING);
    let off = compose(INCLUDING, &handed, Some("vacation"), false).unwrap();
    assert!(!off.contains("include :personal"), "{off}");
    // With no reply script at all, the rules are written as they are, an include among them.
    let plain = compose("", &handed, None, false).unwrap();
    assert!(plain.contains("include :personal"), "{plain}");
    assert_eq!(
        include_command("say \"hi\"").unwrap(),
        "include :personal :optional \"say \\\"hi\\\"\";\r\n"
    );
    assert!(include_command("a\0b").is_none());
}
