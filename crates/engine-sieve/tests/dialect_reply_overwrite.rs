//! The reply and the rules written over one script: where the reply rule goes among rules a host
//! inserted, deleted or moved; what an overwrite of an unread reply keeps and discards; and the
//! second `vacation` that would make a reply an error.

use engine_core::{
    rules::{MailRule, RuleName},
    vacation::{AutoReply, VacationResponse},
};
use engine_sieve::{
    SegmentKind,
    dialect::{
        DialectError, DialectErrorKind, read_rules,
        reply::{
            ReplyRefusal, ScriptReply, UnreadReply, includes, read_reply, unnamed_vacation,
            write_reply, write_reply_over,
        },
        write_rules,
    },
    segment,
};

const REDIRECT: &str = include_str!("fixtures/roundcube/reply_redirect.sieve");
const NO_ZONE: &str = include_str!("fixtures/roundcube/vacation_rule.sieve");
const RECEIVED: &str = include_str!("fixtures/roundcube/reply_received.sieve");

const REPLY: &str = "# rule:[Out-of-Office]\r\nif true\r\n{\r\n\tvacation \"Away.\";\r\n}\r\n";

fn rule(name: &str, action: &str) -> String {
    format!(
        "# rule:[{name}]\r\nif header :contains \"subject\" \"{name}\"\r\n{{\r\n\t{action};\r\n}}\r\n"
    )
}

fn script(parts: &[&str]) -> String {
    format!("require [\"fileinto\",\"vacation\"];\r\n{}", parts.concat())
}

/// The names of the script's rules and its reply, in order.
fn order(script: &str) -> Vec<String> {
    segment(script)
        .unwrap()
        .iter()
        .filter(|segment| segment.kind() == SegmentKind::Command)
        .map(|segment| {
            let leading = segment.leading();
            let start = leading.rfind("# rule:[").expect("named") + 8;
            leading[start..leading[start..].find(']').unwrap() + start].to_owned()
        })
        .collect()
}

fn named(rules: &[MailRule], name: &str) -> MailRule {
    rules
        .iter()
        .find(|rule| rule.name.as_ref().map(RuleName::as_str) == Some(name))
        .cloned()
        .unwrap()
}

fn new_rule(name: &str) -> MailRule {
    let rules = read_rules(&script(&[&rule(name, "fileinto \"New\"")]));
    let mut rule = rules[0].clone();
    rule.origin = None;
    rule
}

#[test]
fn the_reply_keeps_its_place_among_rules_inserted_deleted_and_moved() {
    // [Spam, Reply]: a rule inserted ahead of Spam must not end up between the reply and Spam,
    // where the reply would answer the mail Spam discards.
    let base = script(&[&rule("Spam", "discard;\r\n\tstop"), REPLY]);
    let spam = named(&read_rules(&base), "Spam");
    let written = write_rules(&base, &[new_rule("New"), spam]).unwrap();
    assert_eq!(order(&written), ["New", "Spam", "Out-of-Office"]);
    // A rule that ran after the reply moved to the top: the reply stays after Spam.
    let base = script(&[
        &rule("Spam", "discard;\r\n\tstop"),
        REPLY,
        &rule("B", "keep"),
    ]);
    let rules = read_rules(&base);
    let written = write_rules(&base, &[named(&rules, "B"), named(&rules, "Spam")]).unwrap();
    assert_eq!(order(&written), ["B", "Spam", "Out-of-Office"]);

    let base = script(&[&rule("A", "keep"), REPLY, &rule("B", "keep")]);
    let rules = read_rules(&base);
    let (a, b) = (named(&rules, "A"), named(&rules, "B"));
    let cases: [(Vec<MailRule>, [&str; 3]); 2] = [
        (
            vec![a.clone(), new_rule("C"), b.clone()],
            ["A", "Out-of-Office", "C"],
        ),
        (
            vec![b.clone(), a.clone(), new_rule("C")],
            ["B", "A", "Out-of-Office"],
        ),
    ];
    for (rules, expected) in cases {
        let written = write_rules(&base, &rules).unwrap();
        assert_eq!(&order(&written)[..3], expected);
    }
    // Deleted on either side, or everything new: it stays where it was.
    assert_eq!(
        order(&write_rules(&base, &[b]).unwrap()),
        ["Out-of-Office", "B"]
    );
    assert_eq!(
        order(&write_rules(&base, &[a]).unwrap()),
        ["A", "Out-of-Office"]
    );
    assert_eq!(
        order(&write_rules(&base, &[new_rule("X")]).unwrap()),
        ["X", "Out-of-Office"]
    );
}

fn unread(script: &str) -> UnreadReply {
    match read_reply(script) {
        ScriptReply::Unreadable(unread) => unread,
        other => panic!("expected an unread reply, got {other:?}"),
    }
}

fn reply(text: &str) -> VacationResponse {
    VacationResponse::default()
        .with_reply(AutoReply::default().with_text(text))
        .turned_on()
}

fn refusal(result: Result<String, DialectError>) -> ReplyRefusal {
    match result.expect_err("refused").kind() {
        DialectErrorKind::Reply(reason) => reason,
        other => panic!("expected a reply refusal, got {other:?}"),
    }
}

/// `text` overwritten with `reply("New.")`, checked to be a script the segmenter splits and whose
/// reply reads back with the same name.
fn overwritten(text: &str) -> String {
    let written = write_reply_over(text, &reply("New."), &unread(text)).unwrap();
    assert!(segment(&written).is_ok(), "{written}");
    assert!(read_reply(&written).segment().is_some(), "{written}");
    written
}

#[test]
fn an_overwrite_keeps_every_command_after_vacation_and_any_else_and_adds_no_stray_semicolon() {
    let blocks = "if true\r\n{\r\n\tvacation \"Old.\";\r\n\tif header :contains \"subject\" \"x\"\r\n\t\
                  {\r\n\t\tdiscard;\r\n\t}\r\n\tkeep;\r\n}\r\nelse\r\n{\r\n\tstop;\r\n}\r\n";
    let written = overwritten(blocks);
    assert!(
        written.contains("\tvacation \"New.\";\r\n\tif header :contains \"subject\" \"x\"\r\n\t{\r\n\t\tdiscard;\r\n\t}\r\n\tkeep;\r\n}\r\nelse\r\n{\r\n\tstop;\r\n}\r\n"),
        "{written}"
    );
    assert!(!written.contains("};"), "{written}");
    let again = unread(&written);
    assert!(again.does_more() && !again.sends_mail());
}

#[test]
fn a_brace_inside_the_test_s_parentheses_is_no_block_of_the_rule() {
    // Found by the fuzz target: the `{` inside `allof (…)` was taken for the rule's block, and the
    // overwrite left the test's `)` behind, which no Sieve parser splits.
    let torn = "require [\"date\",\"relational\",\"vacation\"];\r\nif allof (currentdate { true } \
                :zone \"+0000\" :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\")\r\n\
                {\r\n\tvacation \"Away.\";\r\n}\r\n";
    // Read from its real block: the reply, with a window the reader does not read.
    assert!(matches!(read_reply(torn), ScriptReply::Unreadable(_)));
    overwritten(torn);
}

#[test]
fn an_overwrite_keeps_vacation_s_arguments_only_where_vacation_was_read() {
    // Only the window was unread: `:days` and `:from` are kept.
    let written = overwritten(NO_ZONE);
    assert!(
        written.contains("\tvacation :days 1 :from \"carol@example.com\" \"New.\";"),
        "{written}"
    );
    // `:mime` makes `vacation` unread: none of its arguments are kept, `:days` included.
    let mime = "if true\r\n{\r\n\tvacation :days 3 :mime \"x\";\r\n}\r\n";
    let written = overwritten(mime);
    assert!(written.contains("\tvacation \"New.\";"), "{written}");
    assert!(
        !written.contains(":days") && !written.contains(":mime"),
        "{written}"
    );
    // Nothing besides `vacation` was kept, so the reply written reads back whole.
    assert!(matches!(read_reply(&written), ScriptReply::Read { .. }));
}

#[test]
fn an_unread_reply_says_whether_it_may_send_mail_beyond_its_reply() {
    let redirect = unread(REDIRECT);
    assert!(redirect.sends_mail() && redirect.does_more());
    let window_only = unread(NO_ZONE);
    assert!(!window_only.sends_mail() && !window_only.does_more());
    let second =
        "if true\r\n{\r\n\tvacation \"a\";\r\n}\r\nelse\r\n{\r\n\tvacation \"b\";\r\n}\r\n";
    assert!(
        unread(second).sends_mail(),
        "a second vacation is listed too"
    );
    // A disabled rule's comment shows no command, and is no action.
    assert!(!format!("{redirect:?}").contains("deputy"));
}

#[test]
fn a_reply_that_is_on_where_another_rule_runs_vacation_is_refused() {
    let boss = rule("Boss", "vacation \"In a meeting.\"");
    let base = script(&[&boss]);
    assert_eq!(
        refusal(write_reply(&base, &reply("Away."))),
        ReplyRefusal::OtherVacation
    );
    // Off, it sends nothing, and is written.
    assert!(write_reply(&base, &reply("Away.").turned_off()).is_ok());
    // A disabled rule's first branch never runs; what follows it can.
    let disabled = boss.replace("if header", "if false # header");
    assert!(write_reply(&script(&[&disabled]), &reply("Away.")).is_ok());
    let disabled_else = format!("{disabled}else\r\n{{\r\n\tvacation \"x\";\r\n}}\r\n");
    assert_eq!(
        refusal(write_reply(&script(&[&disabled_else]), &reply("Away."))),
        ReplyRefusal::OtherVacation
    );
    // Every branch of another rule counts.
    let otherwise = format!(
        "{}else\r\n{{\r\n\tvacation \"x\";\r\n}}\r\n",
        rule("Other", "keep")
    );
    assert_eq!(
        refusal(write_reply(&script(&[&otherwise]), &reply("Away."))),
        ReplyRefusal::OtherVacation
    );
    // The reply's own `else` runs only where its `vacation` does not: no second reply.
    let own_else =
        "if true\r\n{\r\n\tvacation \"a\";\r\n}\r\nelse\r\n{\r\n\tvacation \"b\";\r\n}\r\n";
    assert!(write_reply_over(own_else, &reply("New."), &unread(own_else)).is_ok());
    // A second `vacation` in its own block runs with the first.
    let twice = "if true\r\n{\r\n\tvacation \"a\";\r\n\tvacation \"b\";\r\n}\r\n";
    assert_eq!(
        refusal(write_reply_over(twice, &reply("New."), &unread(twice))),
        ReplyRefusal::OtherVacation
    );
}

#[test]
fn a_reply_that_is_on_beside_an_include_is_refused() {
    // The dialect reads no included script, which may reply too.
    let base = script(&[
        "include :personal :optional \"vacation\";\r\n",
        &rule("A", "keep"),
    ]);
    assert!(includes(&base));
    assert!(!includes(&script(&[&rule("A", "keep")])));
    assert!(
        !includes("# include \"x\";\r\nkeep;\r\n"),
        "a comment includes nothing"
    );
    assert_eq!(
        refusal(write_reply(&base, &reply("Away."))),
        ReplyRefusal::Include
    );
    assert!(write_reply(&base, &reply("Away.").turned_off()).is_ok());
}

#[test]
fn the_plugin_s_received_header_window_is_the_reply_unread() {
    // Without `date`, the plugin's window is `anyof` of `header :regex "received"` tests: the
    // reply, whose window the engine cannot read as instants.
    let found = unread(RECEIVED);
    assert_eq!(found.segment(), 1);
    assert!(!found.does_more());
    assert!(read_rules(RECEIVED).is_empty(), "not one of the rules");
    // Exactly the plugin's tests: another header test beside them makes it the user's rule.
    let mixed = RECEIVED.replace(
        "header :regex \"received\" \"([ 0]1|[ 0]2) Feb 2030\"",
        "header :contains \"from\" \"boss\"",
    );
    assert_ne!(mixed, RECEIVED);
    assert_eq!(read_reply(&mixed), ScriptReply::Absent);
}

#[test]
fn a_vacation_the_dialect_did_not_name_is_told_apart_from_a_named_rule_s() {
    // A server's own reply script (Stalwart's, as its JMAP reply writes it) names no rule.
    let server = "require [\"vacation\", \"relational\", \"date\"];\r\n\r\nif currentdate \
                  :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\" {\r\nvacation :mime \"x\";\r\n}\r\n";
    assert!(unnamed_vacation(server));
    // A Roundcube-named rule of the user's, and Roundcube's own reply, are named.
    assert!(!unnamed_vacation(&script(&[&rule(
        "Boss",
        "vacation \"In a meeting.\""
    )])));
    assert!(!unnamed_vacation(&script(&[REPLY, &rule("A", "keep")])));
    assert!(!unnamed_vacation(&script(&[&rule("A", "keep")])));
    // An unnamed reply-shaped rule counts too: no Roundcube wrote it.
    assert!(unnamed_vacation(
        "if true\r\n{\r\n\tvacation \"x\";\r\n}\r\n"
    ));
}
