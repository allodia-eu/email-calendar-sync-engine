//! The out-of-office reply as a rule of the script (`engine_sieve::dialect::reply`), against the
//! bytes Roundcube's managesieve plugin wrote for its vacation engine's rules
//! (`fixtures/roundcube/reply_*.sieve`, `README.md` there).

use engine_core::{
    rules::{MailRule, RuleName},
    time::UtcDateTime,
    vacation::{AutoReply, ReplyAudience, VacationResponse, VacationWindow},
};
use engine_sieve::dialect::{
    DialectErrorKind, read_rules,
    reply::{ReplyRefusal, ScriptReply, read_reply, write_reply, write_reply_over},
    write_rules,
};

const WINDOW: &str = include_str!("fixtures/roundcube/reply_window.sieve");
const START: &str = include_str!("fixtures/roundcube/reply_start.sieve");
const ALWAYS: &str = include_str!("fixtures/roundcube/reply_always.sieve");
const DISABLED: &str = include_str!("fixtures/roundcube/reply_disabled.sieve");
const DATE_ZONE: &str = include_str!("fixtures/roundcube/reply_date_zone.sieve");
const ZONE: &str = include_str!("fixtures/roundcube/reply_zone.sieve");
const ARGUMENTS: &str = include_str!("fixtures/roundcube/reply_arguments.sieve");
const POSTED: &str = include_str!("fixtures/roundcube/reply_vacation_post.sieve");
const REDIRECT: &str = include_str!("fixtures/roundcube/reply_redirect.sieve");
const NO_ZONE: &str = include_str!("fixtures/roundcube/vacation_rule.sieve");
const ONE_RULE: &str = include_str!("fixtures/roundcube/one_rule.sieve");

fn at(text: &str) -> UtcDateTime {
    text.parse().expect("a UTC instant")
}

fn read(script: &str) -> VacationResponse {
    match read_reply(script) {
        ScriptReply::Read { reply, .. } => *reply,
        other => panic!("expected a reply read, got {other:?}"),
    }
}

fn unread(script: &str) -> engine_sieve::dialect::reply::UnreadReply {
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

fn refusal(result: Result<String, engine_sieve::dialect::DialectError>) -> ReplyRefusal {
    match result.expect_err("refused").kind() {
        DialectErrorKind::Reply(reason) => reason,
        other => panic!("expected a reply refusal, got {other:?}"),
    }
}

#[test]
fn the_plugin_s_reply_rules_read_as_the_reply_they_say() {
    let window = read(WINDOW);
    assert_eq!(read_reply(WINDOW).segment(), Some(1), "after the require");
    assert!(window.enabled);
    // `le` holds through the whole second it names: the reply stops at the next one.
    assert_eq!(
        window.window,
        VacationWindow::default()
            .with_start(at("2030-01-02T09:00:00Z"))
            .with_end(at("2030-01-09T17:30:01Z"))
    );
    assert_eq!(window.reply.subject.as_deref(), Some("Away"));
    assert_eq!(
        window.reply.text.as_deref(),
        Some("I am away.\nBack on the ninth.")
    );
    assert_eq!(window.reply.html, None);
    assert_eq!(window.audience, ReplyAudience::Everyone);
    assert!(window.raw.is_some(), "the rule's text is kept");

    let start = read(START);
    assert_eq!(start.window.start, Some(at("2030-01-02T09:00:00Z")));
    assert_eq!(start.window.end, None);
    assert_eq!(start.reply.subject, None);

    let always = read(ALWAYS);
    assert!(always.enabled && always.window.is_open());

    assert!(
        !read(DISABLED).enabled,
        "`if false # …` is a reply switched off"
    );

    // A date in a zone: from midnight starting the first day to midnight ending the last, there.
    assert_eq!(
        read(DATE_ZONE).window,
        VacationWindow::default()
            .with_start(at("2030-01-01T23:00:00Z"))
            .with_end(at("2030-01-09T23:00:00Z"))
    );
    // A user in Amsterdam, as the plugin's settings page saved it on the Dovecot harness, and as
    // its own expressions build it: the same instants as the UTC fixture.
    for zoned in [ZONE, POSTED] {
        assert_eq!(read(zoned).window, window.window);
    }
    assert_eq!(read(POSTED).reply.text, window.reply.text);
    assert_eq!(read(ARGUMENTS).reply.text.as_deref(), Some("Away."));
}

#[test]
fn a_reply_rule_holding_more_than_the_model_says_is_unread_not_narrowed() {
    // The plugin's own "then also" action: the reply also forwards.
    assert_eq!(unread(REDIRECT).segment(), 1);
    // A date in the server's own zone: when it runs is the server's to say.
    assert_eq!(unread(NO_ZONE).segment(), 1);
    let vacation = |test: &str, action: &str| format!("if {test}\r\n{{\r\n\t{action};\r\n}}\r\n");
    for (what, script) in [
        ("a MIME body", vacation("true", "vacation :mime \"x\"")),
        (
            "a value in another zone than the date's",
            vacation(
                "currentdate :zone \"+0100\" :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\"",
                "vacation \"x\"",
            ),
        ),
        (
            "a zero offset spelled +00:00",
            vacation(
                "currentdate :zone \"+0000\" :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00+00:00\"",
                "vacation \"x\"",
            ),
        ),
        (
            "two starts",
            vacation(
                "allof (currentdate :zone \"+0000\" :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\", \
                 currentdate :zone \"+0000\" :value \"gt\" \"iso8601\" \"2030-01-03T09:00:00Z\")",
                "vacation \"x\"",
            ),
        ),
        (
            "a stop after it",
            vacation("true", "vacation \"x\";\r\n\tstop"),
        ),
        // Found by the fuzz target: no string the writer writes holds a NUL.
        ("a NUL in the text", vacation("true", "vacation \"a\0b\"")),
        (
            "the plugin's received fallback",
            vacation("header :regex \"received\" \"2030\"", "vacation \"x\""),
        ),
        (
            "an else after it",
            "if true\r\n{\r\n\tvacation \"x\";\r\n}\r\nelse\r\n{\r\n\tkeep;\r\n}\r\n".to_owned(),
        ),
    ] {
        assert_eq!(unread(&script).segment(), 0, "{what}");
    }
}

#[test]
fn a_value_the_script_s_variables_would_expand_is_not_read_as_written() {
    // `read_rules` reads this rule as carried whole and `Expanded`; the reply reader agrees.
    let script = "require [\"variables\",\"vacation\"];\r\nset \"n\" \"Bob\";\r\n\
                  if true\r\n{\r\n\tvacation \"Hi ${n}\";\r\n}\r\n";
    assert_eq!(unread(script).segment(), 2);
    // The same text where nothing expands it is literal.
    let literal = "require [\"vacation\"];\r\nif true\r\n{\r\n\tvacation \"Hi ${n}\";\r\n}\r\n";
    assert_eq!(read(literal).reply.text.as_deref(), Some("Hi ${n}"));
}

#[test]
fn debug_shows_no_word_of_a_reply_read_or_unread() {
    let shown = format!("{:?} {:?}", read_reply(WINDOW), read_reply(REDIRECT));
    for word in ["Away", "away", "deputy", "ninth"] {
        assert!(!shown.contains(word), "{word} in {shown}");
    }
}

#[test]
fn a_rule_that_answers_some_senders_is_the_user_s_rule_not_the_reply() {
    // Roundcube would take it as the reply with a narrower audience; the engine reads no audience
    // but everyone, so it stays an ordinary rule, and a new reply goes before it.
    let boss = "require [\"vacation\"];\r\n# rule:[Boss]\r\n\
                if header :contains \"from\" \"boss\"\r\n{\r\n\tvacation \"In a meeting.\";\r\n}\r\n";
    assert_eq!(read_reply(boss), ScriptReply::Absent);
    assert_eq!(read_rules(boss).len(), 1);
    let written = write_reply(boss, &reply("Away.")).unwrap();
    assert!(
        written.ends_with(&boss[boss.find("# rule:[Boss]").unwrap()..]),
        "{written}"
    );
    assert_eq!(read(&written).reply.text.as_deref(), Some("Away."));
}

#[test]
fn the_reply_is_no_rule_of_the_user_s_and_survives_a_write_of_the_rules_byte_for_byte() {
    let rules = read_rules(WINDOW);
    assert_eq!(rules.len(), 1, "the Invoices rule alone");
    assert_eq!(
        rules[0].name.as_ref().map(RuleName::as_str),
        Some("Invoices")
    );

    // Rules and the reply in one script: read both, change the rules, and the reply is as it was.
    let reply_text = &WINDOW
        [WINDOW.find("# rule:[Out-of-Office]").unwrap()..WINDOW.find("# rule:[Invoices]").unwrap()];
    let mut changed: Vec<MailRule> = rules;
    changed[0] = changed[0].clone().disabled();
    let written = write_rules(WINDOW, &changed).unwrap();
    assert!(written.contains(reply_text), "{written}");
    assert!(written.contains("if false # header"), "{written}");
    assert_eq!(read_reply(&written), read_reply(WINDOW));

    // And the other way round: write the reply, and the rules are as they were.
    let rewritten = write_reply(&written, &reply("Back soon.")).unwrap();
    let unplaced = |script: &str| -> Vec<MailRule> {
        read_rules(script)
            .into_iter()
            .map(|mut rule| {
                rule.origin = None;
                rule
            })
            .collect()
    };
    assert_eq!(unplaced(&rewritten), unplaced(&written));
    assert!(rewritten.ends_with(&written[written.find("# rule:[Invoices]").unwrap()..]));
    // A host that hides the reply and writes no rules at all keeps it.
    let emptied = write_rules(WINDOW, &[]).unwrap();
    assert_eq!(read_reply(&emptied), read_reply(WINDOW));
}

#[test]
fn a_new_reply_is_written_as_the_plugin_writes_one() {
    let start = reply("Away from the second.")
        .with_window(VacationWindow::default().with_start(at("2030-01-02T09:00:00Z")));
    assert_eq!(write_reply("", &start).unwrap(), START);

    let always = VacationResponse::default()
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("Away for now."),
        )
        .turned_on();
    assert_eq!(write_reply("", &always).unwrap(), ALWAYS);

    let window = VacationWindow::default()
        .with_start(at("2030-01-02T09:00:00Z"))
        .with_end(at("2030-01-09T17:30:01Z"));
    let disabled = VacationResponse::default()
        .with_window(window)
        .with_reply(AutoReply::default().with_subject("Away").with_text("Away."))
        .turned_off();
    assert_eq!(write_reply("", &disabled).unwrap(), DISABLED);
}

#[test]
fn every_readable_reply_writes_back_as_it_was_and_a_zoned_one_as_the_same_instants() {
    for (name, script) in [
        ("window", WINDOW),
        ("start", START),
        ("always", ALWAYS),
        ("disabled", DISABLED),
        // In the plugin's argument order, and with `vacation-seconds` alone required, as it does.
        ("arguments", ARGUMENTS),
    ] {
        assert_eq!(
            write_reply(script, &read(script)).unwrap(),
            script,
            "{name}"
        );
    }
    for zoned in [ZONE, POSTED, DATE_ZONE] {
        let written = write_reply(zoned, &read(zoned)).unwrap();
        assert_eq!(read(&written).window, read(zoned).window);
    }
}

#[test]
fn a_rewritten_reply_keeps_its_place_name_and_unmodelled_arguments_and_touches_no_rule() {
    let mut changed = read(WINDOW);
    changed.reply = AutoReply::default().with_text("Back soon.");
    changed.window = VacationWindow::default();
    let written = write_reply(WINDOW, &changed).unwrap();
    let invoices = &WINDOW[WINDOW.find("# rule:[Invoices]").unwrap()..];
    assert!(written.ends_with(invoices), "the other rule, byte for byte");
    assert!(
        written.starts_with("require [\"date\",\"fileinto\",\"relational\",\"vacation\"];\r\n"),
        "a require is only ever added to"
    );
    assert!(
        written.contains(
            "# rule:[Out-of-Office]\r\nif true\r\n{\r\n\tvacation :days 7 \"Back soon.\";\r\n}\r\n"
        ),
        "{written}"
    );
    // `:seconds` kept needs `vacation-seconds`, over a script that did not require it.
    let seconds = "if true\r\n{\r\n\tvacation :seconds 60 \"x\";\r\n}\r\n";
    let written = write_reply(seconds, &reply("y")).unwrap();
    assert!(
        written.starts_with("require [\"vacation-seconds\"];\r\n"),
        "{written}"
    );
}

#[test]
fn an_unread_reply_is_written_over_only_when_named_and_what_was_read_of_it_survives() {
    assert_eq!(
        refusal(write_reply(REDIRECT, &reply("Away."))),
        ReplyRefusal::UnreadReply
    );
    let written = write_reply_over(REDIRECT, &reply("Away."), &unread(REDIRECT)).unwrap();
    assert!(
        written.contains("\tvacation \"Away.\";\r\n\tredirect :copy \"deputy@example.com\";\r\n"),
        "the plugin's own action is kept: {written}"
    );
    assert_eq!(
        written.matches("vacation").count(),
        2,
        "require and one action"
    );

    // Only the window was unread: `:days` and `:from` survive, the window is the new one.
    let written = write_reply_over(NO_ZONE, &reply("Back."), &unread(NO_ZONE)).unwrap();
    assert!(
        written.contains("\tvacation :days 1 :from \"carol@example.com\" \"Back.\";"),
        "{written}"
    );
    assert_eq!(read(&written).reply.text.as_deref(), Some("Back."));

    // Found by the fuzz target: a `;` inside an open list ends no command, so nothing after the
    // `vacation` is taken for an action of its own and written back torn.
    let torn = "require [\"copy\",\"wacation\"];\r\n#fice]\r\nif true\r\n{\r\n\t\
                vacation \"e.co;m\"\rJreqwire [\"copy\",\"wacation\"\n\r;f]#ice]\r\n\
                if true\r\n{\r\n\tvacation \"e.co;m\"\rJ}\r\n}\r\n";
    let written = write_reply_over(torn, &reply("y"), &unread(torn)).unwrap();
    assert_eq!(read(&written).reply.text.as_deref(), Some("y"));

    // Naming an unread reply the script no longer holds is refused, as is naming one where the
    // script has none.
    let stale = unread(NO_ZONE);
    assert_eq!(
        refusal(write_reply_over(REDIRECT, &reply("x"), &stale)),
        ReplyRefusal::UnreadReply
    );
    assert_eq!(
        refusal(write_reply_over(ONE_RULE, &reply("x"), &stale)),
        ReplyRefusal::UnreadReply
    );
}

#[test]
fn a_new_reply_goes_first() {
    let written = write_reply(ONE_RULE, &reply("Away.")).unwrap();
    let reply_at = written.find("# rule:[Out-of-Office]").unwrap();
    assert!(reply_at < written.find("# rule:[Invoices]").unwrap());
    assert!(written.ends_with(&ONE_RULE[ONE_RULE.find("# rule:[Invoices]").unwrap()..]));
}

#[test]
fn a_window_is_written_to_the_whole_second_and_never_wider() {
    // A start between seconds starts at the next one. An end between seconds stops after the
    // last whole second that ends by it, and reads back no later than it was.
    let start = at("2030-01-02T09:00:00.25Z");
    let end = at("2030-01-02T10:00:00.5Z");
    let written = write_reply(
        "",
        &reply("x").with_window(VacationWindow::default().with_start(start).with_end(end)),
    )
    .unwrap();
    assert!(
        written.contains("\"ge\" \"iso8601\" \"2030-01-02T09:00:01Z\""),
        "{written}"
    );
    assert!(
        written.contains("\"le\" \"iso8601\" \"2030-01-02T09:59:59Z\""),
        "{written}"
    );
    let back = read(&written).window;
    assert!(back.start.unwrap() >= start && back.end.unwrap() <= end);

    // A window holding no whole second would never send: refused, not widened.
    for (start, end) in [
        ("2030-01-02T10:00:00.2Z", "2030-01-02T10:00:00.9Z"),
        ("2030-01-02T10:00:00Z", "2030-01-02T10:00:00Z"),
        ("2030-01-02T11:00:00Z", "2030-01-02T10:00:00Z"),
    ] {
        let window = VacationWindow::default()
            .with_start(at(start))
            .with_end(at(end));
        assert_eq!(
            refusal(write_reply("", &reply("x").with_window(window))),
            ReplyRefusal::EmptyWindow,
            "{start}..{end}"
        );
    }
}

#[test]
fn every_relation_and_a_zone_west_of_utc_read_as_instants() {
    let script = "require [\"date\",\"relational\",\"vacation\"];\r\n\
        if allof (currentdate :zone \"-0130\" :value \"gt\" \"iso8601\" \"2030-01-02T07:30:00-01:30\", \
        currentdate :value \"lt\" :zone \"-0130\" \"date\" \"2030-01-09\")\r\n{\r\n\
        \tvacation :addresses \"b@example.com\" :handle \"h\" \"x\";\r\n}\r\n";
    let window = read(script).window;
    // `gt` holds from the second after; `lt` on a date stops at midnight starting it, there.
    assert_eq!(window.start, Some(at("2030-01-02T09:00:01Z")));
    assert_eq!(window.end, Some(at("2030-01-09T01:30:00Z")));
    let written = write_reply(script, &read(script)).unwrap();
    assert!(
        written.contains("vacation :addresses [\"b@example.com\"] :handle \"h\" \"x\";"),
        "{written}"
    );

    let script = "if allof (currentdate :zone \"+0000\" :value \"gt\" \"date\" \"2030-01-02\", \
        currentdate :zone \"+0000\" :value \"lt\" \"iso8601\" \"2030-01-09T00:00:00Z\")\r\n\
        {\r\n\tvacation \"x\";\r\n}\r\n";
    let window = read(script).window;
    assert_eq!(window.start, Some(at("2030-01-03T00:00:00Z")));
    assert_eq!(window.end, Some(at("2030-01-09T00:00:00Z")));
}

#[test]
fn a_reply_the_rule_cannot_hold_is_refused_with_its_reason() {
    let html = VacationResponse::default()
        .with_reply(AutoReply::default().with_text("x").with_html("<p>x</p>"))
        .turned_on();
    assert_eq!(refusal(write_reply(ONE_RULE, &html)), ReplyRefusal::Html);
    assert_eq!(
        refusal(write_reply(
            ONE_RULE,
            &reply("x").with_audience(ReplyAudience::Contacts)
        )),
        ReplyRefusal::Audience
    );
    assert_eq!(
        refusal(write_reply(
            ONE_RULE,
            &reply("x").with_outside_reply(AutoReply::default().with_text("y"))
        )),
        ReplyRefusal::OutsideReply
    );
    assert_eq!(
        refusal(write_reply(
            ONE_RULE,
            &VacationResponse::default().turned_on()
        )),
        ReplyRefusal::NoText
    );
    assert_eq!(
        refusal(write_reply(ONE_RULE, &reply("a\rb"))),
        ReplyRefusal::Value
    );
    // A value a script requiring `variables` would expand.
    let variables = "require [\"variables\"];\r\nset \"a\" \"b\";\r\n";
    assert!(matches!(
        write_reply(variables, &reply("costs ${a}"))
            .unwrap_err()
            .kind(),
        DialectErrorKind::Variable { .. }
    ));
}
