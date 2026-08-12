//! Gated live integration: what is true only on an **IMAP4rev2** wire.
//!
//! The counterpart to `live_imap_rev1.rs`, split by dialect rather than by vendor. A
//! server belongs here once it advertises `IMAP4rev2` and the client's `ENABLE` is
//! confirmed — today the Stalwart harness; Dovecot joins when its early-access rev2 is
//! turned on in `docker/dovecot/harness.conf`, with no test rewritten.
//!
//! What is worth asserting here is what rev2 changes and rev1 cannot show: names arrive as
//! UTF-8, and the extensions rev2 folded into the base protocol (RFC 9051 Appendix E) are
//! usable **without** the server advertising them individually — which is the entire
//! reason the client enables the dialect rather than each extension.
//!
//! Skips when `STALWART_IMAP_ADDR` is unset, so the offline `cargo test --workspace` stays
//! green.

#[path = "common/imap_live.rs"]
mod imap_live;

use engine_core::mail::MailboxRole;
use imap_live::{STALWART, connect, connect_to, find, folders};

/// The rev2 servers this suite runs against.
const REV2_SERVERS: [imap_live::Server; 1] = [STALWART];

#[tokio::test]
async fn a_non_ascii_name_arrives_as_utf8_and_needs_no_decoding() {
    for server in &REV2_SERVERS {
        let Some(provider) = connect(
            server,
            "a_non_ascii_name_arrives_as_utf8_and_needs_no_decoding",
        )
        .await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // rev2 mailbox names are UTF-8 (RFC 9051 §5.1, Appendix E item 16). The same folder
        // reaches the model under the same identity as on rev1 — but by the opposite
        // route, with nothing decoded. Decoding here unconditionally would be wrong: a
        // rev2 name may legitimately contain `&`.
        let mailbox = find(&all, "Überweisungen");
        assert_eq!(mailbox.id.as_str(), "Überweisungen", "{}", server.label);
    }
}

#[tokio::test]
async fn selecting_a_utf8_name_reaches_the_mailbox_unencoded() {
    for server in &REV2_SERVERS {
        let Some(provider) = connect_to(
            server,
            "Überweisungen",
            "selecting_a_utf8_name_reaches_the_mailbox_unencoded",
        )
        .await
        else {
            continue;
        };

        // The outgoing half: on rev2 the transport must send the name as-is. Encoding it to
        // modified UTF-7 here would name a mailbox this server does not have.
        let account = engine_core::ids::AccountId::try_from("live-harness").unwrap();
        engine_provider::Provider::sync_email(&provider, &account, None)
            .await
            .unwrap_or_else(|err| panic!("{}: select a UTF-8 name: {err}", server.label));
    }
}

#[tokio::test]
async fn the_folder_roles_arrive_without_a_special_use_return_option() {
    for server in &REV2_SERVERS {
        let Some(provider) = connect(
            server,
            "the_folder_roles_arrive_without_a_special_use_return_option",
        )
        .await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // rev2 folds SPECIAL-USE's attributes into the base `LIST` response (RFC 9051
        // §7.3.1) and defines no `RETURN (SPECIAL-USE)` option to ask with — so the client
        // stops asking, and the roles must still be here. This is the assertion that would
        // catch "rev2 enables everything it folded in" being read as "…so keep asking".
        assert_eq!(
            find(&all, "Sent Items").role,
            Some(MailboxRole::Sent),
            "{}",
            server.label
        );
        assert_eq!(
            find(&all, "Deleted Items").role,
            Some(MailboxRole::Trash),
            "{}",
            server.label
        );
    }
}
