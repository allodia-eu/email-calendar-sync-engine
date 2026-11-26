//! The names IMAP can and cannot carry as one level: the adapter refuses exactly those,
//! before any change is sent, and leaves the rest to the server.

use engine_core::error::FailureClass;
use engine_provider::{MailboxEdit, MailboxEditReceipt};

use super::{TREE, id, run};

#[tokio::test]
async fn an_empty_name_is_refused_before_any_change_is_sent() {
    for edit in [
        MailboxEdit::create(String::new(), None),
        MailboxEdit::create(String::new(), Some(id("Work"))),
        MailboxEdit::Update {
            target: id("Work/2024"),
            name: String::new(),
            parent: Some(id("Work")),
        },
    ] {
        let (result, sent) = run(TREE, &[], &edit).await;
        assert_eq!(result.unwrap_err().class(), FailureClass::InvalidState);
        assert!(
            !sent.contains("CREATE") && !sent.contains("RENAME"),
            "{sent}"
        );
    }
}

#[tokio::test]
async fn a_slash_is_one_level_on_a_server_whose_delimiter_is_a_dot() {
    let dotted = "* LIST () \".\" \"INBOX\"\r\n* LIST () \".\" \"Work\"\r\na2 OK LIST done\r\n";
    let create = MailboxEdit::create("Q1/Q2", Some(id("Work")));
    let (result, sent) = run(
        dotted,
        &["a3 OK CREATE\r\n", "a4 OK SUBSCRIBE\r\n"],
        &create,
    )
    .await;

    assert_eq!(
        result.unwrap(),
        MailboxEditReceipt::resolved(id("Work.Q1/Q2"))
    );
    assert!(sent.contains("a3 CREATE \"Work.Q1/Q2\""), "{sent}");
}
