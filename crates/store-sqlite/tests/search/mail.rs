//! End-to-end mail search: address/membership/label/keyword structured filters,
//! date ranges, free-text ranking, account scoping, `OR`-within-a-field, and
//! query-injection safety (SQL and FTS metacharacters stay data, not operators).

use engine_core::mail::{Keyword, SystemKeyword};

use super::*;

#[tokio::test]
async fn from_filter_returns_only_matching_messages() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![
            message("m1", "Lunch", "alice@example.com", "inbox"),
            message("m2", "Dinner", "bob@example.com", "inbox"),
        ],
    )
    .await;

    let results = store
        .search_mail(
            std::slice::from_ref(&scope),
            &parse_mail("from:Alice@Example.com"),
            10,
        )
        .await
        .unwrap();
    // Address matching is case-insensitive (normalized both sides) and excludes m2.
    assert_eq!(results.keys().len(), 1);
    assert_eq!(results.keys()[0].as_str(), "m1");
    assert!(results.coverage.is_complete());
}

#[tokio::test]
async fn prefix_term_matches_partial_word_in_subject() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![
            message("hit", "Allodia weekly", "a@example.com", "inbox"),
            message("miss", "unrelated digest", "a@example.com", "inbox"),
        ],
    )
    .await;

    // Search-as-you-type: typing "allo" prefix-matches the subject token
    // "Allodia"; the unrelated message is excluded.
    let results = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("allo"), 10)
        .await
        .unwrap();
    assert_eq!(results.keys().len(), 1);
    assert_eq!(results.keys()[0].as_str(), "hit");
}

#[tokio::test]
async fn address_is_searchable_as_free_text_and_by_prefix() {
    let store = store();
    let scope = mail_scope();
    // A metadata-tier message: the subject never mentions "allodia", and there is
    // no body preview — the only "allodia" is in the sender address, which the
    // projection folds into the FTS body.
    ingest_mail(
        &store,
        &scope,
        vec![
            message("addr", "Weekly update", "info@allodia.eu", "inbox"),
            message("other", "Weekly update", "bob@example.com", "inbox"),
        ],
    )
    .await;

    // The full token "allodia" (from the address) matches via the FTS body...
    let full = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("allodia"), 10)
        .await
        .unwrap();
    assert_eq!(full.keys().len(), 1);
    assert_eq!(full.keys()[0].as_str(), "addr");

    // ...and so does the prefix "allo".
    let prefix = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("allo"), 10)
        .await
        .unwrap();
    assert_eq!(prefix.keys().len(), 1);
    assert_eq!(prefix.keys()[0].as_str(), "addr");
}

#[tokio::test]
async fn free_text_ranks_by_term_frequency() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![
            message("once", "invoice", "a@example.com", "inbox"),
            message(
                "thrice",
                "invoice invoice invoice",
                "a@example.com",
                "inbox",
            ),
            message("none", "unrelated", "a@example.com", "inbox"),
        ],
    )
    .await;

    let results = store
        .search_mail(&[scope], &parse_mail("invoice"), 10)
        .await
        .unwrap();
    let keys: Vec<&str> = results.hits.iter().map(|h| h.key.as_str()).collect();
    // Both invoice messages match; the unrelated one does not.
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"once") && keys.contains(&"thrice"));
    assert!(!keys.contains(&"none"));
    // At equal document length, bm25 ranks the higher term-frequency doc first;
    // RRF scores are positive.
    assert_eq!(keys[0], "thrice");
    assert!(results.hits[0].score > 0.0);
}

#[tokio::test]
async fn subject_scope_excludes_body_only_matches() {
    let store = store();
    let scope = mail_scope();
    let mut body_only = message("body", "weekly digest", "a@example.com", "inbox");
    body_only.preview = Some("the quarterly report is ready".to_owned());
    let subject_hit = message("subj", "quarterly numbers", "a@example.com", "inbox");
    ingest_mail(&store, &scope, vec![body_only, subject_hit]).await;

    // Unscoped free text matches the body...
    let free = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("quarterly"), 10)
        .await
        .unwrap();
    assert_eq!(free.keys().len(), 2);
    // ...but `subject:` only matches the subject column.
    let scoped = store
        .search_mail(&[scope], &parse_mail("subject:quarterly"), 10)
        .await
        .unwrap();
    assert_eq!(scoped.keys().len(), 1);
    assert_eq!(scoped.keys()[0].as_str(), "subj");
}

#[tokio::test]
async fn structured_filters_combine_with_and_semantics() {
    let store = store();
    let scope = mail_scope();
    let mut with_attachment = message("a1", "report", "alice@example.com", "inbox");
    with_attachment.has_attachment = true;
    let no_attachment = message("a2", "report", "alice@example.com", "inbox");
    let other_box = {
        let mut m = message("a3", "report", "alice@example.com", "archive");
        m.has_attachment = true;
        m
    };
    ingest_mail(
        &store,
        &scope,
        vec![with_attachment, no_attachment, other_box],
    )
    .await;

    // from AND mailbox AND has_attachment must all hold → only a1.
    let results = store
        .search_mail(
            &[scope],
            &parse_mail("from:alice@example.com mailbox:inbox has_attachment:true"),
            10,
        )
        .await
        .unwrap();
    assert_eq!(results.keys().len(), 1);
    assert_eq!(results.keys()[0].as_str(), "a1");
}

#[tokio::test]
async fn date_range_filters_on_received_date() {
    let store = store();
    let scope = mail_scope();
    let mut jan = message("jan", "x", "a@example.com", "inbox");
    jan.received_at = Some("2026-01-15T12:00:00Z".parse().unwrap());
    let mut mar = message("mar", "x", "a@example.com", "inbox");
    mar.received_at = Some("2026-03-15T12:00:00Z".parse().unwrap());
    ingest_mail(&store, &scope, vec![jan, mar]).await;

    let results = store
        .search_mail(
            &[scope],
            &parse_mail("after:2026-02-01 before:2026-04-01"),
            10,
        )
        .await
        .unwrap();
    assert_eq!(results.keys().len(), 1);
    assert_eq!(results.keys()[0].as_str(), "mar");
}

#[tokio::test]
async fn search_is_account_scoped() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![message("mine", "secret", "a@example.com", "inbox")],
    )
    .await;

    // A different account's scope sees nothing — scope keys embed the account.
    let other = SyncScope::JmapType {
        account: engine_core::ids::AccountId::try_from("acct-2").unwrap(),
        data_type: JmapDataType::Email,
    };
    let results = store
        .search_mail(&[other], &parse_mail("secret"), 10)
        .await
        .unwrap();
    assert!(results.hits.is_empty());

    // No scopes at all → empty, still complete (nothing claimed to be missing).
    let none = store
        .search_mail(&[], &parse_mail("secret"), 10)
        .await
        .unwrap();
    assert!(none.hits.is_empty());
    assert!(none.coverage.is_complete());
}

#[tokio::test]
async fn to_cc_label_and_keyword_filters() {
    let store = store();
    let scope = mail_scope();

    let mut m1 = Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::new([
            MailboxId::try_from("inbox").unwrap(),
            MailboxId::try_from("work").unwrap(),
        ])
        .unwrap(),
    );
    m1.envelope.to = vec![EmailAddress::new("bob@example.com")];
    m1.envelope.cc = vec![EmailAddress::new("carol@example.com")];
    m1.keywords.insert(Keyword::system(SystemKeyword::Flagged));
    ingest_mail(
        &store,
        &scope,
        vec![m1, message("m2", "x", "z@example.com", "inbox")],
    )
    .await;

    // `label:` queries the same membership kind as `mailbox:`; `keyword:` is
    // case-folded to the stored canonical form.
    for query in [
        "to:bob@example.com",
        "cc:carol@example.com",
        "label:work",
        "keyword:$FLAGGED",
    ] {
        let results = store
            .search_mail(std::slice::from_ref(&scope), &parse_mail(query), 10)
            .await
            .unwrap();
        assert_eq!(results.keys().len(), 1, "query {query}");
        assert_eq!(results.keys()[0].as_str(), "m1", "query {query}");
    }
}

#[tokio::test]
async fn repeated_operator_is_or_within_a_field() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![
            message("a", "x", "alice@example.com", "inbox"),
            message("b", "x", "bob@example.com", "inbox"),
            message("c", "x", "carol@example.com", "inbox"),
        ],
    )
    .await;

    // Two `from:` values become an `IN` list — alice OR bob, never carol.
    let results = store
        .search_mail(
            std::slice::from_ref(&scope),
            &parse_mail("from:alice@example.com from:bob@example.com"),
            10,
        )
        .await
        .unwrap();
    let keys: Vec<&str> = results.hits.iter().map(|h| h.key.as_str()).collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"a") && keys.contains(&"b"));
    assert!(!keys.contains(&"c"));
}

#[tokio::test]
async fn sql_metacharacters_in_filter_values_are_data_not_code() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![message("m1", "hello", "alice@example.com", "inbox")],
    )
    .await;

    // A classic injection payload as a `from:` value. It is a bound parameter, so
    // it matches no address and is never executed.
    let injection = "from:\"'; DROP TABLE mail_index; --\"";
    let attack = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail(injection), 10)
        .await
        .unwrap();
    assert!(attack.hits.is_empty());

    // The table is intact — a normal query still succeeds and returns the message.
    // (If `DROP TABLE` had executed, this query would error on the missing table.)
    let ok = store
        .search_mail(
            std::slice::from_ref(&scope),
            &parse_mail("from:alice@example.com"),
            10,
        )
        .await
        .unwrap();
    assert_eq!(ok.keys().len(), 1);
    assert_eq!(ok.keys()[0].as_str(), "m1");
}

#[tokio::test]
async fn fts_syntax_in_text_is_literal_not_operators() {
    let store = store();
    let scope = mail_scope();
    let mut body_zzz = message("zzz", "plain", "a@example.com", "inbox");
    body_zzz.preview = Some("zzz".to_owned()); // lands in the FTS `body` column
    ingest_mail(
        &store,
        &scope,
        vec![
            message("apple", "apple", "a@example.com", "inbox"),
            message("banana", "banana", "a@example.com", "inbox"),
            body_zzz,
        ],
    )
    .await;

    // `OR` is a literal term, not an FTS operator: `apple OR banana` requires all
    // three tokens, which no message has — it does not union the two matches.
    let or = store
        .search_mail(
            std::slice::from_ref(&scope),
            &parse_mail("apple OR banana"),
            10,
        )
        .await
        .unwrap();
    assert!(or.hits.is_empty());

    // A column-filter payload in free text stays a literal phrase: `body:zzz` is
    // the phrase "body zzz" (quoted), which the body-only "zzz" message lacks — it
    // does not become an FTS column filter that would match it.
    let column = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("body:zzz"), 10)
        .await
        .unwrap();
    assert!(column.hits.is_empty());

    // The legitimate scoped operator still works.
    let legit = store
        .search_mail(
            std::slice::from_ref(&scope),
            &parse_mail("subject:apple"),
            10,
        )
        .await
        .unwrap();
    assert_eq!(legit.keys().len(), 1);
    assert_eq!(legit.keys()[0].as_str(), "apple");
}
