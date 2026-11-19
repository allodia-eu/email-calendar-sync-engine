use engine_core::mail::Importance;

use super::{header_field, header_fields, importance};

#[test]
fn every_occurrence_is_read_in_order_and_unfolded() {
    let section = "Importance: low\r\nX-Priority:\r\n 1\r\nImportance:\r\n\thigh\r\n\r\n";
    assert_eq!(header_fields(section, "importance"), ["low", "high"]);
    assert_eq!(header_fields(section, "X-Priority"), ["1"]);
    assert_eq!(header_field(section, "Importance").as_deref(), Some("low"));
    assert_eq!(header_fields(section, "References"), Vec::<String>::new());
}

#[test]
fn a_field_name_is_matched_whole() {
    let section = "X-Importance: high\r\nImportance-Note: low\r\n\r\n";
    assert_eq!(header_fields(section, "Importance"), Vec::<String>::new());
}

#[test]
fn the_last_occurrence_of_a_header_states_the_importance() {
    let section = "Importance: low\r\nImportance: high\r\n\r\n";
    assert_eq!(importance(section), Importance::High);
}

#[test]
fn every_header_the_rule_reads_is_consulted() {
    assert_eq!(
        importance("X-MSMail-Priority: Low\r\nX-Priority: 1\r\n\r\n"),
        Importance::Low
    );
    assert_eq!(
        importance("X-Priority: 1 (Highest)\r\n\r\n"),
        Importance::High
    );
    assert_eq!(
        importance("Content-Type: text/plain\r\n\r\n"),
        Importance::Normal
    );
}
