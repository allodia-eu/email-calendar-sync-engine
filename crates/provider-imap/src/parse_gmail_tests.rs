use crate::parse::parse_fetch;

fn lines(strs: &[&str]) -> Vec<Vec<u8>> {
    strs.iter().map(|s| s.as_bytes().to_vec()).collect()
}

// Shapes captured from imap.gmail.com, labels renamed: a STORE's echo carries the labels and
// MODSEQ ahead of the UID, a plain FETCH the ids ahead of it.
#[test]
fn reads_gmail_ids_labels_and_modseq_in_any_order() {
    let rows = parse_fetch(&lines(&[
        r#"609 FETCH (X-GM-LABELS ("\\Important" "Label one" plain) MODSEQ (234381) UID 5002)"#,
        "855 FETCH (X-GM-THRID 1402140321003522132 X-GM-MSGID 1402140321003522133 UID 6870)",
    ]))
    .unwrap();
    assert_eq!(rows[0].uid, 5002);
    assert_eq!(rows[0].modseq, Some(234_381));
    assert_eq!(
        rows[0].gmail.labels.as_deref(),
        Some(
            &[
                "\\Important".to_owned(),
                "Label one".to_owned(),
                "plain".to_owned()
            ][..]
        )
    );
    assert_eq!(rows[0].gmail.message, None);
    assert_eq!(rows[1].gmail.message, Some(1_402_140_321_003_522_133));
    assert_eq!(rows[1].gmail.thread, Some(1_402_140_321_003_522_132));
    assert_eq!(rows[1].gmail.labels, None);
    assert_eq!(rows[1].modseq, None);
}

#[test]
fn an_empty_label_list_is_known_to_be_empty() {
    let rows = parse_fetch(&lines(&["1 FETCH (UID 7 X-GM-LABELS ())"])).unwrap();
    assert_eq!(rows[0].gmail.labels, Some(Vec::new()));
}

#[test]
fn an_unreadable_number_is_absent_rather_than_zero() {
    let rows = parse_fetch(&lines(&["1 FETCH (UID 7 X-GM-MSGID x MODSEQ (1 2))"])).unwrap();
    assert_eq!(rows[0].gmail.message, None);
    assert_eq!(rows[0].modseq, None);
}
