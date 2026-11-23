//! The out-of-office reply as a rule of the script (`engine_sieve::dialect::reply`), against the
//! bytes Roundcube's managesieve plugin wrote for its vacation engine's rules
//! (`fixtures/roundcube/reply_*.sieve`, `README.md` there).

use engine_core::{
    rules::{RuleBody, RuleOrigin},
    time::UtcDateTime,
    vacation::{AutoReply, ReplyAudience, VacationResponse, VacationWindow},
};
use engine_sieve::dialect::{
    DialectErrorKind, read_rules,
    reply::{ScriptReply, read_reply, write_reply},
};

const WINDOW: &str = include_str!("fixtures/roundcube/reply_window.sieve");
const START: &str = include_str!("fixtures/roundcube/reply_start.sieve");
const ALWAYS: &str = include_str!("fixtures/roundcube/reply_always.sieve");
const DISABLED: &str = include_str!("fixtures/roundcube/reply_disabled.sieve");
const DATE_ZONE: &str = include_str!("fixtures/roundcube/reply_date_zone.sieve");
const REDIRECT: &str = include_str!("fixtures/roundcube/reply_redirect.sieve");
const NO_ZONE: &str = include_str!("fixtures/roundcube/vacation_rule.sieve");
const ONE_RULE: &str = include_str!("fixtures/roundcube/one_rule.sieve");

fn at(text: &str) -> UtcDateTime {
    text.parse().expect("a UTC instant")
}

fn read(script: &str) -> (VacationResponse, usize) {
    match read_reply(script) {
        ScriptReply::Read { reply, segment } => (*reply, segment),
        other => panic!("expected a reply read, got {other:?}"),
    }
}

fn reply(text: &str) -> VacationResponse {
    VacationResponse::default()
        .with_reply(AutoReply::default().with_text(text))
        .turned_on()
}

#[test]
fn the_plugin_s_reply_rules_read_as_the_reply_they_say() {
    let (window, segment) = read(WINDOW);
    assert_eq!(segment, 1, "the rule after the require");
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

    let (start, _) = read(START);
    assert_eq!(start.window.start, Some(at("2030-01-02T09:00:00Z")));
    assert_eq!(start.window.end, None);
    assert_eq!(start.reply.subject, None);
    assert_eq!(start.reply.text.as_deref(), Some("Away from the second."));

    let (always, _) = read(ALWAYS);
    assert!(always.enabled && always.window.is_open());
    assert_eq!(always.reply.subject.as_deref(), Some("Away"));

    let (disabled, _) = read(DISABLED);
    assert!(!disabled.enabled, "`if false # …` is a reply switched off");
    assert_eq!(disabled.window.end, Some(at("2030-01-09T17:30:01Z")));

    // A date in a zone: from midnight starting the first day to midnight ending the last, there.
    let (dated, _) = read(DATE_ZONE);
    assert_eq!(
        dated.window,
        VacationWindow::default()
            .with_start(at("2030-01-01T23:00:00Z"))
            .with_end(at("2030-01-09T23:00:00Z"))
    );
}

#[test]
fn a_reply_rule_holding_more_than_the_model_says_is_unread_not_narrowed() {
    // Another action: the reply also forwards. A date in the server's own zone: when it runs is
    // the server's to say. Neither may read as a plain reply.
    assert_eq!(read_reply(REDIRECT), ScriptReply::Unreadable { segment: 1 });
    assert_eq!(read_reply(NO_ZONE), ScriptReply::Unreadable { segment: 1 });
    for (what, script) in [
        (
            "another test",
            "if allof (true, header :contains \"from\" \"boss\")\r\n{\r\n\tvacation \"x\";\r\n}\r\n",
        ),
        (
            "a MIME body",
            "if true\r\n{\r\n\tvacation :mime \"Content-Type: text/plain\r\n\r\nx\";\r\n}\r\n",
        ),
        (
            "a value in another zone than the date's",
            "if currentdate :zone \"+0100\" :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\"\r\n{\r\n\tvacation \"x\";\r\n}\r\n",
        ),
        (
            "two starts",
            "if allof (currentdate :zone \"+0000\" :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\", currentdate :zone \"+0000\" :value \"gt\" \"iso8601\" \"2030-01-03T09:00:00Z\")\r\n{\r\n\tvacation \"x\";\r\n}\r\n",
        ),
        (
            "a stop after it",
            "if true\r\n{\r\n\tvacation \"x\";\r\n\tstop;\r\n}\r\n",
        ),
        ("a bare vacation", "vacation \"x\";\r\n"),
    ] {
        assert_eq!(
            read_reply(script),
            ScriptReply::Unreadable { segment: 0 },
            "{what}"
        );
    }
    assert_eq!(read_reply(ONE_RULE), ScriptReply::Absent);
    assert_eq!(read_reply(""), ScriptReply::Absent);
}

#[test]
fn the_first_rule_that_replies_is_the_reply_and_read_rules_numbers_it_alike() {
    let segment = read(WINDOW).1;
    let rules = read_rules(WINDOW);
    let reply_rule = rules
        .iter()
        .find(|rule| {
            matches!(rule.origin, Some(RuleOrigin::ScriptSegment { index, .. }) if index == segment)
        })
        .expect("read_rules has the reply rule at the same segment");
    assert!(matches!(reply_rule.body, RuleBody::Opaque(_)));
    assert_eq!(read_reply(WINDOW).segment(), Some(segment));
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
fn every_readable_reply_writes_back_as_it_was() {
    for (name, script) in [
        ("window", WINDOW),
        ("start", START),
        ("always", ALWAYS),
        ("disabled", DISABLED),
    ] {
        let (read, _) = read(script);
        assert_eq!(write_reply(script, &read).unwrap(), script, "{name}");
    }
}

#[test]
fn a_rewritten_reply_keeps_its_place_name_and_unmodelled_arguments_and_touches_no_rule() {
    let (mut changed, _) = read(WINDOW);
    changed.reply = AutoReply::default().with_text("Back soon.");
    changed.window = VacationWindow::default();
    let written = write_reply(WINDOW, &changed).unwrap();
    let invoices = &WINDOW[WINDOW.find("# rule:[Invoices]").unwrap()..];
    assert!(
        written.ends_with(invoices),
        "the other rule is copied byte for byte"
    );
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
    let (back, _) = read(&written);
    assert_eq!(back.reply.text.as_deref(), Some("Back soon."));
}

#[test]
fn a_new_reply_goes_first_and_an_unread_one_is_replaced_where_it_is() {
    let written = write_reply(ONE_RULE, &reply("Away.")).unwrap();
    let reply_at = written.find("# rule:[Out-of-Office]").unwrap();
    let rule_at = written.find("# rule:[Invoices]").unwrap();
    assert!(reply_at < rule_at, "{written}");
    assert!(written.ends_with(&ONE_RULE[ONE_RULE.find("# rule:[Invoices]").unwrap()..]));

    // The unread reply is overwritten, keeping its name; the redirect it held is gone.
    let written = write_reply(REDIRECT, &reply("Away.")).unwrap();
    assert!(!written.contains("redirect"), "{written}");
    assert_eq!(read(&written).0.reply.text.as_deref(), Some("Away."));
    assert_eq!(written.matches("vacation \"Away.\";").count(), 1);
}

#[test]
fn a_window_is_written_to_the_whole_second() {
    // A start between seconds starts at the next one; an end between seconds stops after the
    // last second before it.
    let start = "2030-01-02T09:00:00.25Z".parse::<UtcDateTime>().unwrap();
    let end = "2030-01-02T10:00:00.5Z".parse::<UtcDateTime>().unwrap();
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
        written.contains("\"le\" \"iso8601\" \"2030-01-02T10:00:00Z\""),
        "{written}"
    );
}

#[test]
fn a_reply_the_rule_cannot_hold_is_refused_not_narrowed() {
    let kind = |reply: &VacationResponse| write_reply(ONE_RULE, reply).unwrap_err().kind();
    let html = VacationResponse::default()
        .with_reply(AutoReply::default().with_text("x").with_html("<p>x</p>"))
        .turned_on();
    assert_eq!(kind(&html), DialectErrorKind::Reply);
    assert_eq!(
        kind(&reply("x").with_audience(ReplyAudience::Contacts)),
        DialectErrorKind::Reply
    );
    assert_eq!(
        kind(&reply("x").with_outside_reply(AutoReply::default().with_text("y"))),
        DialectErrorKind::Reply
    );
    assert_eq!(
        kind(&VacationResponse::default().turned_on()),
        DialectErrorKind::Reply,
        "a reply with no text"
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

#[test]
fn every_relation_and_a_zone_west_of_utc_read_as_instants_and_kept_arguments_survive() {
    let script = "require [\"date\",\"relational\",\"vacation\",\"vacation-seconds\"];\r\n\
        if allof (currentdate :zone \"-0130\" :value \"gt\" \"iso8601\" \"2030-01-02T07:30:00-01:30\", \
        currentdate :value \"lt\" :zone \"-0130\" \"date\" \"2030-01-09\")\r\n{\r\n\
        \tvacation :seconds 3600 :from \"a@example.com\" :addresses [\"b@example.com\",\"c@example.com\"] \
        :handle \"h\" \"x\";\r\n}\r\n";
    let (read, _) = read(script);
    // `gt` holds from the second after; `lt` on a date stops at midnight starting it, there.
    assert_eq!(read.window.start, Some(at("2030-01-02T09:00:01Z")));
    assert_eq!(read.window.end, Some(at("2030-01-09T01:30:00Z")));
    let written = write_reply(script, &read).unwrap();
    assert!(
        written.contains(
            "vacation :seconds 3600 :from \"a@example.com\" \
             :addresses [\"b@example.com\",\"c@example.com\"] :handle \"h\" \"x\";"
        ),
        "{written}"
    );
    let (back, _) = self::read(&written);
    assert_eq!(back.window, read.window);

    // A date `gt` starts at the next midnight; an iso8601 `lt` stops at the instant itself.
    let script = "if allof (currentdate :zone \"+0000\" :value \"gt\" \"date\" \"2030-01-02\", \
        currentdate :zone \"+0000\" :value \"lt\" \"iso8601\" \"2030-01-09T00:00:00+00:00\")\r\n\
        {\r\n\tvacation \"x\";\r\n}\r\n";
    let (read, _) = self::read(script);
    assert_eq!(read.window.start, Some(at("2030-01-03T00:00:00Z")));
    assert_eq!(read.window.end, Some(at("2030-01-09T00:00:00Z")));
}
