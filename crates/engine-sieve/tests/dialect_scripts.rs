//! Which script holds the rules, and the include of a server's reply script, as every adapter
//! decides them (`engine_sieve::dialect::{scripts, include}`).

use engine_sieve::dialect::{
    include::{compose, include_command, included_scripts, includes_script, without_include},
    read_rules,
    scripts::{Choice, Listed, choose_rules_script, plainly_server_reply, rules_script},
};

fn listed(scripts: &[(&'static str, bool, bool)]) -> Vec<Listed<'static>> {
    scripts
        .iter()
        .map(|&(name, active, server_reply)| Listed {
            name,
            active,
            server_reply: Some(server_reply),
        })
        .collect()
}

#[test]
fn a_script_s_kind_is_asked_only_where_the_order_needs_it() {
    let unknown = |scripts: &[(&'static str, bool)]| -> Vec<Listed<'static>> {
        scripts
            .iter()
            .map(|&(name, active)| Listed {
                name,
                active,
                server_reply: None,
            })
            .collect()
    };
    // The active script is the user's: nothing else is asked.
    let mut asked = Vec::new();
    let choice = choose_rules_script(&unknown(&[("a", false), ("b", true), ("c", false)]), |at| {
        asked.push(at);
        Ok::<_, ()>(false)
    });
    assert_eq!((choice, asked), (Ok(Choice::Script(1)), vec![1]));
    // The active script is the server's reply: `rules` is asked next.
    let mut asked = Vec::new();
    let choice = choose_rules_script(
        &unknown(&[("vacation", true), ("rules", false), ("x", false)]),
        |at| {
            asked.push(at);
            Ok::<_, ()>(at == 0)
        },
    );
    assert_eq!((choice, asked), (Ok(Choice::Script(1)), vec![0, 1]));
    // Two scripts never read as one, though neither was read.
    let choice = choose_rules_script(&unknown(&[("a", false), ("b", false)]), |_| {
        Ok::<_, ()>(false)
    });
    assert_eq!(choice, Ok(Choice::Create));
    // The only one left is read before it is chosen, and an error ends the choice.
    let choice = choose_rules_script(&unknown(&[("only", false)]), |_| Err::<bool, _>("gone"));
    assert_eq!(choice, Err("gone"));
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

/// Stalwart's own reply script, as its JMAP `VacationResponse/set` wrote it and ManageSieve's
/// `GETSCRIPT` answered on the harness (2026-11-24): with one bound, and with two nested.
const STALWART_ONE: &str = "require [\"vacation\", \"relational\", \"date\"];\r\n\r\nif currentdate \
    :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\" {\r\nvacation :mime \"Content-Type: text/plain; \
    charset=\\\"utf-8\\\"\r\nContent-Transfer-Encoding: 7bit\r\n\r\nAway.\";\r\n}\r\n";
const STALWART_TWO: &str = "require [\"vacation\", \"relational\", \"date\"];\r\n\r\nif currentdate \
    :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\" {\r\nif currentdate :value \"le\" \"iso8601\" \
    \"2030-01-09T17:30:00Z\" {\r\nvacation :mime :subject \"Away\" \"x\";\r\n}\r\n}\r\n";

#[test]
fn only_a_script_plainly_a_server_s_reply_is_taken_for_one() {
    assert!(plainly_server_reply(STALWART_ONE));
    assert!(plainly_server_reply(STALWART_TWO));
    // A reply with filters beside it, as SOGo writes its script: someone's rules, not a server's.
    let sogo = "require [\"fileinto\",\"vacation\"];\r\nif header :contains \"subject\" \"x\" {\r\n\
                fileinto \"X\";\r\n}\r\nvacation :days 7 :addresses [\"a@example.com\"] \"Away.\";\r\n";
    assert!(!plainly_server_reply(sogo));
    // A reply rule in Roundcube's dialect is named: the user's.
    let named = "# rule:[Out-of-Office]\r\nif true\r\n{\r\n\tvacation \"x\";\r\n}\r\n";
    assert!(!plainly_server_reply(named));
    // A test on anything but the date answers some senders: not a server's reply to everyone.
    assert!(!plainly_server_reply(
        "if header :contains \"from\" \"b\" { vacation \"x\"; }\r\n"
    ));
    assert!(!plainly_server_reply(
        "if true { vacation \"x\"; vacation \"y\"; }\r\n"
    ));
    assert!(!plainly_server_reply(
        "if \"unterminated { vacation \"x\"; }"
    ));
    assert!(!plainly_server_reply(""));
}
