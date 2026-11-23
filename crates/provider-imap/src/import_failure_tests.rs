//! Offline tests for how an import answers a failure: a refusal ends only its message, a lost
//! connection ends the batch, and whether a message may be stored turns on whether its command
//! was whole when the connection went.

use super::*;

#[tokio::test]
async fn a_message_the_server_finds_too_big_is_refused_alone_and_the_batch_goes_on() {
    // A server may refuse the literal before it is sent, answering the tagged `NO` where the
    // continuation would be (RFC 7889 `TOOBIG`).
    let (provider, recorded) = provider(&[
        "a3 NO [TOOBIG] Message too large\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [message(b"big", &["INBOX"]), message(b"ok", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    let refusal = answers[0].as_ref().unwrap_err();
    assert!(refusal.is_item_unavailable(), "{refusal}");
    assert!(!refusal.requires_confirmation());
    assert!(answers[1].is_ok());
    assert!(
        !written(&recorded).contains("big"),
        "the refused literal never went out"
    );
}

#[tokio::test]
async fn a_connection_lost_once_the_message_went_out_may_have_stored_it_and_ends_the_batch() {
    let (provider, _) = provider(&["+ go\r\n"]).await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(answers.len(), 2);
    assert!(answers[0].as_ref().unwrap_err().requires_confirmation());
    let unsent = answers[1].as_ref().unwrap_err();
    assert!(!unsent.requires_confirmation(), "{unsent}");
    assert_eq!(class(&answers[1]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_connection_lost_before_the_continuation_stored_nothing() {
    let (provider, _) = provider(&[]).await;
    let batch = [message(b"one", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    let failure = answers[0].as_ref().unwrap_err();
    assert!(!failure.requires_confirmation(), "{failure}");
    assert_eq!(class(&answers[0]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_refusal_after_the_literal_stored_nothing_and_the_batch_goes_on() {
    let (provider, _) = provider(&[
        "+ go\r\n",
        "a3 NO [OVERQUOTA] Quota exceeded\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    let refusal = answers[0].as_ref().unwrap_err();
    assert!(!refusal.requires_confirmation(), "{refusal}");
    assert!(
        !refusal.is_item_unavailable(),
        "a full account is not about this message"
    );
    assert!(answers[1].is_ok());
}

#[tokio::test]
async fn a_bad_in_place_of_the_continuation_refuses_that_message_alone() {
    let (provider, _) = provider(&[
        "a3 BAD Invalid arguments\r\n",
        "+ go\r\n",
        "a4 OK APPEND done\r\n",
    ])
    .await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert!(answers[1].is_ok());
}

#[tokio::test]
async fn an_unexpected_line_in_place_of_the_continuation_ends_the_batch_with_nothing_stored() {
    let (provider, _) = provider(&["hello there\r\n"]).await;
    let batch = [message(b"one", &["INBOX"]), message(b"two", &["INBOX"])];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert_eq!(class(&answers[1]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_connection_lost_while_opening_a_folder_ends_the_batch_with_nothing_stored() {
    let (provider, _) = provider(&[]).await;
    let batch = [
        with_keywords(message(b"one", &["Archive"]), &["$label1"]),
        message(b"two", &["INBOX"]),
    ];

    let answers = provider.import_messages(&account(), &batch).await;

    assert!(!answers[0].as_ref().unwrap_err().requires_confirmation());
    assert_eq!(class(&answers[1]), FailureClass::Retryable);
}

#[tokio::test]
async fn a_batch_that_cannot_get_a_connection_is_answered_whole_by_that_failure() {
    // The first batch loses the only connection this test account has, on its last message,
    // and it cannot dial another; the next batch is answered message by message all the same.
    // Had the dead connection been parked, the next batch would have written to it.
    let (provider, recorded) = provider(&[]).await;
    let first = provider
        .import_messages(&account(), &[message(b"one", &["INBOX"])])
        .await;
    assert!(first[0].is_err());

    let answers = provider
        .import_messages(
            &account(),
            &[message(b"two", &["INBOX"]), message(b"three", &["INBOX"])],
        )
        .await;

    assert_eq!(answers.len(), 2);
    assert!(answers.iter().all(Result::is_err));
    assert_eq!(
        written(&recorded).matches("APPEND").count(),
        1,
        "the failed connection was discarded, not reused: {}",
        written(&recorded)
    );
}
