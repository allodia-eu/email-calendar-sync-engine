use super::*;
use crate::{raw::RawVacation, time::UtcDateTime, version::VacationVersion};

fn at(text: &str) -> UtcDateTime {
    text.parse().unwrap()
}

fn away() -> AutoReply {
    AutoReply::default()
        .with_subject("Gone fishing")
        .with_text("Back on the ninth, ask the desk.")
        .with_html("<p>Back on the <b>ninth</b>.</p>")
}

/// What a JMAP account can hold: a window with either end, a subject, a text and an HTML body,
/// one reply, to everyone.
fn jmap() -> VacationVocabulary {
    VacationVocabulary::everyone()
        .with_window(WindowSupport::EitherEnd)
        .with_subject()
        .with_text_and_html()
}

/// What Graph will hold: a window only with both ends, no subject, one HTML body per message,
/// narrower audiences, and an outside reply.
fn graph() -> VacationVocabulary {
    VacationVocabulary::everyone()
        .with_window(WindowSupport::BothEnds)
        .with_audience(ReplyAudience::Organization)
        .with_audience(ReplyAudience::OrganizationAndContacts)
        .with_outside_reply()
}

#[test]
fn a_reply_s_debug_names_what_is_there_and_never_what_it_says() {
    let reply = VacationResponse::default()
        .turned_on()
        .with_reply(away())
        .with_outside_reply(AutoReply::default().with_text("Outside: back on the ninth."))
        .with_raw(RawVacation::new(r#"{"subject":"Gone fishing"}"#))
        .with_version(VacationVersion::new("sae"));

    let shown = format!("{reply:?}");
    for secret in ["fishing", "ninth", "desk", "Outside"] {
        assert!(!shown.contains(secret), "{secret} leaked: {shown}");
    }
    assert!(shown.contains("subject: Some(12 bytes)"), "{shown}");
    assert!(shown.contains("html: Some(32 bytes)"), "{shown}");
    assert!(shown.contains("sae"), "the version is not content: {shown}");
    assert!(
        format!("{:?}", AutoReply::default()).contains("subject: None"),
        "an absent part reads as absent"
    );
}

#[test]
fn a_reply_sends_only_while_on_and_inside_its_window_whose_end_is_exclusive() {
    let window = VacationWindow::default()
        .with_start(at("2030-01-02T09:00:00Z"))
        .with_end(at("2030-01-09T17:00:00Z"));
    let reply = VacationResponse::default().turned_on().with_window(window);

    assert!(!reply.replies_at(at("2030-01-02T08:59:59Z")));
    assert!(reply.replies_at(at("2030-01-02T09:00:00Z")), "on the start");
    assert!(reply.replies_at(at("2030-01-09T16:59:59Z")));
    assert!(
        !reply.replies_at(at("2030-01-09T17:00:00Z")),
        "not on the end"
    );
    assert!(
        !reply
            .clone()
            .turned_off()
            .replies_at(at("2030-01-05T12:00:00Z")),
        "a reply that is off sends nothing, window or not"
    );

    // An open end runs on, an open start has run all along.
    let from = VacationResponse::default()
        .turned_on()
        .with_window(VacationWindow::default().with_start(at("2030-01-02T09:00:00Z")));
    assert!(from.replies_at(at("2099-01-01T00:00:00Z")));
    assert!(!from.replies_at(at("2030-01-01T00:00:00Z")));
    let until = VacationResponse::default()
        .turned_on()
        .with_window(VacationWindow::default().with_end(at("2030-01-09T17:00:00Z")));
    assert!(until.replies_at(at("2000-01-01T00:00:00Z")));
    assert!(!until.replies_at(at("2030-01-10T00:00:00Z")));
    assert!(
        VacationResponse::default()
            .turned_on()
            .replies_at(at("2000-01-01T00:00:00Z"))
    );
}

#[test]
fn a_window_is_open_only_without_either_end() {
    assert!(VacationWindow::default().is_open());
    assert!(
        !VacationWindow::default()
            .with_start(at("2030-01-02T09:00:00Z"))
            .is_open()
    );
    assert!(
        !VacationWindow::default()
            .with_end(at("2030-01-02T09:00:00Z"))
            .is_open()
    );
}

#[test]
fn a_transport_loses_nothing_of_a_reply_it_can_hold() {
    let reply = VacationResponse::default()
        .turned_on()
        .with_reply(away())
        .with_window(VacationWindow::default().with_start(at("2030-01-02T09:00:00Z")));
    assert_eq!(jmap().lost_parts(&reply), []);
    assert_eq!(
        VacationVocabulary::everyone().lost_parts(&VacationResponse::default()),
        []
    );
}

#[test]
fn each_part_a_transport_cannot_hold_is_named_once_in_a_fixed_order() {
    let everything = VacationResponse::default()
        .with_reply(away())
        .with_window(VacationWindow::default().with_end(at("2030-01-09T17:00:00Z")))
        .with_audience(ReplyAudience::Contacts)
        .with_outside_reply(AutoReply::default().with_subject("Outside"));

    assert_eq!(
        VacationVocabulary::everyone().lost_parts(&everything),
        [
            VacationPart::Window,
            VacationPart::Subject,
            VacationPart::Bodies,
            VacationPart::Audience,
            VacationPart::OutsideReply
        ]
    );
    assert_eq!(
        jmap().lost_parts(&everything),
        [VacationPart::Audience, VacationPart::OutsideReply]
    );
    // A transport with every part holds it all.
    let all = jmap()
        .with_audience(ReplyAudience::Contacts)
        .with_outside_reply();
    assert_eq!(all.lost_parts(&everything), []);
}

#[test]
fn a_subject_counts_wherever_it_is_and_an_empty_reply_carries_none() {
    let outside_only = VacationResponse::default()
        .with_outside_reply(AutoReply::default().with_subject("Outside"));
    let held = VacationVocabulary::everyone().with_outside_reply();
    assert_eq!(held.lost_parts(&outside_only), [VacationPart::Subject]);

    let no_subject = VacationResponse::default().with_reply(AutoReply::default().with_text("x"));
    assert_eq!(VacationVocabulary::everyone().lost_parts(&no_subject), []);
}

#[test]
fn every_audience_is_held_only_where_listed_and_everyone_always_is() {
    let vocabulary = VacationVocabulary::everyone().with_audience(ReplyAudience::Organization);
    assert!(vocabulary.audiences().contains(&ReplyAudience::Everyone));
    for (audience, held) in [
        (ReplyAudience::Everyone, true),
        (ReplyAudience::Organization, true),
        (ReplyAudience::Contacts, false),
        (ReplyAudience::OrganizationAndContacts, false),
        (ReplyAudience::ContactsInOrganization, false),
    ] {
        let reply = VacationResponse::default().with_audience(audience);
        assert_eq!(
            vocabulary.lost_parts(&reply).is_empty(),
            held,
            "{audience:?}"
        );
    }
}

#[test]
fn a_part_reads_as_its_name() {
    let names: Vec<String> = [
        VacationPart::Window,
        VacationPart::Subject,
        VacationPart::Bodies,
        VacationPart::Audience,
        VacationPart::OutsideReply,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        names,
        [
            "window",
            "subject",
            "separate text and HTML bodies",
            "audience",
            "outside reply"
        ]
    );
}

#[test]
fn a_receipt_carries_the_new_version_and_what_happened_to_the_rules() {
    let receipt = VacationReceipt::new(Some(VacationVersion::new("sai")), RulesEffect::Deactivated);
    assert_eq!(
        receipt.version.as_ref().map(VacationVersion::as_str),
        Some("sai")
    );
    assert_eq!(receipt.rules, RulesEffect::Deactivated);
}

#[test]
fn a_reply_built_for_a_write_carries_no_version_and_no_raw_form() {
    let reply = VacationResponse::default().with_reply(away());
    assert_eq!(reply.version, None);
    assert_eq!(reply.raw, None);
    assert_eq!(reply.audience, ReplyAudience::Everyone);
    assert!(!reply.enabled);
    assert!(!reply.reply.is_empty());
    assert!(AutoReply::default().is_empty());
}

#[test]
fn a_window_that_needs_both_ends_loses_one_with_only_one_and_keeps_none() {
    let start = VacationWindow::default().with_start(at("2030-01-02T09:00:00Z"));
    let both = start.with_end(at("2030-01-09T17:00:00Z"));
    let reply = |window| VacationResponse::default().with_window(window);

    assert_eq!(graph().lost_parts(&reply(start)), [VacationPart::Window]);
    assert_eq!(graph().lost_parts(&reply(both)), []);
    assert_eq!(
        graph().lost_parts(&reply(VacationWindow::default())),
        [],
        "no window is a reply that runs while on (Graph's alwaysEnabled)"
    );
    assert_eq!(jmap().lost_parts(&reply(start)), []);
    assert_eq!(
        VacationVocabulary::everyone().lost_parts(&reply(both)),
        [VacationPart::Window]
    );
}

#[test]
fn a_transport_with_one_body_loses_a_reply_that_has_two() {
    let text = AutoReply::default().with_text("Back on the ninth.");
    let html = AutoReply::default().with_html("<p>Back on the ninth.</p>");
    let both = text.clone().with_html("<p>Back on the ninth.</p>");
    let reply = |part: AutoReply| VacationResponse::default().with_reply(part);

    assert_eq!(graph().lost_parts(&reply(text)), []);
    assert_eq!(graph().lost_parts(&reply(html)), []);
    assert_eq!(
        graph().lost_parts(&reply(both.clone())),
        [VacationPart::Bodies]
    );
    assert_eq!(
        graph().lost_parts(&VacationResponse::default().with_outside_reply(both.clone())),
        [VacationPart::Bodies],
        "the outside reply's bodies count too"
    );
    assert_eq!(jmap().lost_parts(&reply(both)), []);
}

#[test]
fn a_vocabulary_answers_what_it_was_built_with() {
    let vocabulary = jmap();
    assert_eq!(vocabulary.window(), WindowSupport::EitherEnd);
    assert!(vocabulary.has_subject());
    assert_eq!(vocabulary.bodies(), ReplyBodies::TextAndHtml);
    assert!(!vocabulary.has_outside_reply());
    assert_eq!(
        vocabulary.audiences().iter().copied().collect::<Vec<_>>(),
        [ReplyAudience::Everyone]
    );

    let least = VacationVocabulary::everyone();
    assert_eq!(least.window(), WindowSupport::None);
    assert!(!least.has_subject());
    assert_eq!(least.bodies(), ReplyBodies::One);
    assert!(graph().has_outside_reply());
}

#[test]
fn a_window_that_ends_at_or_before_its_start_runs_at_no_instant() {
    let start = VacationWindow::default().with_start(at("2030-01-09T17:00:00Z"));
    assert!(start.with_end(at("2030-01-02T09:00:00Z")).is_empty());
    assert!(start.with_end(at("2030-01-09T17:00:00Z")).is_empty());
    assert!(!start.with_end(at("2030-01-09T17:00:01Z")).is_empty());
    assert!(!start.is_empty());
    assert!(!VacationWindow::default().is_empty());
}
