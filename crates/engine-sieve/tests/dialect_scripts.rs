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
/// `GETSCRIPT` answered on the harness, in every window shape (`fixtures/stalwart/README.md`).
const STALWART: [&str; 4] = [
    include_str!("fixtures/stalwart/reply_none.sieve"),
    include_str!("fixtures/stalwart/reply_start.sieve"),
    include_str!("fixtures/stalwart/reply_end.sieve"),
    include_str!("fixtures/stalwart/reply_both.sieve"),
];
const REQUIRE: &str = "require [\"vacation\", \"relational\", \"date\"];\r\n";

#[test]
fn only_a_script_in_the_shape_stalwart_writes_is_taken_for_a_server_s_reply() {
    for script in STALWART {
        assert!(plainly_server_reply(script), "{script}");
    }
    // A user's own reply-only script: theirs, so writes into it are refused, not routed around.
    assert!(!plainly_server_reply(
        "require \"vacation\";\r\nvacation :days 7 \"Away\";\r\n"
    ));
    // Stalwart's requires and window, but no `:mime`: not what it writes.
    assert!(!plainly_server_reply(&format!(
        "{REQUIRE}if currentdate :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\" {{\r\n\
         vacation :subject \"Away\" \"x\";\r\n}}\r\n"
    )));
    // Requires other than exactly Stalwart's three.
    assert!(!plainly_server_reply(
        "require [\"vacation\"];\r\nvacation :mime \"x\";\r\n"
    ));
    assert!(!plainly_server_reply(
        "require [\"vacation\", \"relational\", \"date\", \"fileinto\"];\r\nvacation :mime \"x\";\r\n"
    ));
    // The end outside the start, a test on something else, a bound on another clock, an `else`.
    for window in [
        "if currentdate :value \"le\" \"iso8601\" \"b\" {\r\nif currentdate :value \"ge\" \"iso8601\" \
         \"a\" {\r\nvacation :mime \"x\";\r\n}\r\n}\r\n",
        "if header :contains \"from\" \"b\" {\r\nvacation :mime \"x\";\r\n}\r\n",
        "if currentdate :value \"ge\" \"date\" \"a\" {\r\nvacation :mime \"x\";\r\n}\r\n",
        "if currentdate :value \"eq\" \"iso8601\" \"a\" {\r\nvacation :mime \"x\";\r\n}\r\n",
        "if currentdate :value \"ge\" \"iso8601\" \"a\" {\r\nvacation :mime \"x\";\r\n} else {\r\nkeep;\r\n}\r\n",
        "vacation :mime \"x\";\r\nkeep;\r\n",
        "if currentdate :value \"ge\" \"iso8601\" \"a\" {\r\nvacation :mime \"x\";\r\nkeep;\r\n}\r\n",
        "vacation :mime \"x\" ;\r\n}\r\n",
        "vacation :mime \"x\"\r\n",
        "if currentdate :value \"ge\" \"iso8601\" \"a\" {\r\nkeep;\r\n}\r\n",
    ] {
        assert!(
            !plainly_server_reply(&format!("{REQUIRE}{window}")),
            "{window}"
        );
    }
    // A reply with filters beside it, as SOGo writes its script: someone's rules, not a server's.
    let sogo = "require [\"fileinto\",\"vacation\"];\r\nif header :contains \"subject\" \"x\" {\r\n\
                fileinto \"X\";\r\n}\r\nvacation :days 7 :addresses [\"a@example.com\"] \"Away.\";\r\n";
    assert!(!plainly_server_reply(sogo));
    // Stalwart's script under a Roundcube rule name is the user's.
    assert!(!plainly_server_reply(&STALWART[0].replacen(
        "\r\n\r\nvacation",
        "\r\n\r\n# rule:[Away]\r\nvacation",
        1
    )));
    // A comment inside the command: Stalwart writes none.
    assert!(!plainly_server_reply(&STALWART[0].replacen(
        "vacation :mime",
        "vacation /* mine */ :mime",
        1
    )));
    assert!(!plainly_server_reply(
        "if \"unterminated { vacation \"x\"; }"
    ));
    assert!(!plainly_server_reply(""));
}

#[test]
fn several_scripts_left_are_each_read_so_a_server_s_reply_among_them_leaves_the_one_script() {
    // Stalwart: the user's inactive `myrules` and the server's inactive reply script. JMAP knows
    // the reply by name and takes `myrules`; reading both must take it too.
    let unknown = |name| Listed {
        name,
        active: false,
        server_reply: None,
    };
    let listed = [unknown("myrules"), unknown("vacation")];
    let mut asked = Vec::new();
    let choice = choose_rules_script(&listed, |at| {
        asked.push(at);
        Ok::<_, ()>(at == 1)
    });
    assert_eq!(choice, Ok(Choice::Script(0)));
    assert_eq!(asked, [0, 1], "each asked once");
    let known = [
        Listed {
            server_reply: Some(false),
            ..listed[0]
        },
        Listed {
            server_reply: Some(true),
            ..listed[1]
        },
    ];
    assert_eq!(rules_script(&known), Choice::Script(0), "as JMAP chooses");
    // Two scripts of the user's: which one is meant cannot be told.
    assert_eq!(
        choose_rules_script(&listed, |_| Ok::<_, ()>(false)),
        Ok(Choice::Create)
    );
}
