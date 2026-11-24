//! How a [`GraphClient`] roots its URLs, what its `Debug` leaves out, and where it rebases a link.

use super::*;

#[test]
fn client_roots_urls_at_the_principal_and_redacts_debug() {
    // Default — the signed-in user's own mailbox roots at /me.
    let me = GraphClient::connect(
        "super-secret-token",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(me.url("/messages"), format!("{GRAPH_BASE}/me/messages"));
    // A shared mailbox roots requests at /users/{address} — the documented shape
    // `…/users/info@example.org/mailFolders('Inbox')/messages`.
    let shared = GraphClient::for_mailbox(
        "t",
        MailboxPrincipal::user("info@example.org").unwrap(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        shared.url("/mailFolders('Inbox')/messages"),
        format!("{GRAPH_BASE}/users/info@example.org/mailFolders('Inbox')/messages")
    );
    // The Debug rendering must not leak the bearer token.
    assert!(!format!("{me:?}").contains("super-secret-token"));
}

#[test]
fn rebase_targets_a_custom_base_but_is_a_noop_at_the_default() {
    // At the default base, an absolute Graph link is left untouched.
    let prod = GraphClient::connect(
        "t",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    let link = format!("{GRAPH_BASE}/me/messages/delta?$deltatoken=x");
    assert_eq!(prod.rebase(&link), link);
    // A custom base catches the absolute link (a replay server / proxy) …
    let custom = GraphClient::with_base(
        "t",
        "http://127.0.0.1:9",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        custom.rebase(&link),
        "http://127.0.0.1:9/me/messages/delta?$deltatoken=x"
    );
    // … but only `graph.microsoft.com` links; anything else passes through.
    assert_eq!(custom.rebase("http://elsewhere/x"), "http://elsewhere/x");
}
