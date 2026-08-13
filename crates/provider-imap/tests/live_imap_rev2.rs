//! Gated live integration: what is true only on an **IMAP4rev2** wire.
//!
//! The counterpart to `live_imap_rev1.rs`, split by dialect rather than by vendor. A
//! server belongs here once it advertises `IMAP4rev2` and the client's `ENABLE` is
//! confirmed — the Stalwart harness, and Dovecot's rev2 service beside it.
//!
//! What is worth asserting here is what rev2 changes and rev1 cannot show: names arrive as
//! UTF-8, and the extensions rev2 folded into the base protocol (RFC 9051 Appendix E) are
//! usable **without** the server advertising them individually — which is the entire
//! reason the client enables the dialect rather than each extension.
//!
//! **Two rev2 servers, not one, and that is the point.** Every claim here is a claim about
//! a dialect, and one implementation cannot establish one: Stalwart and Dovecot already
//! read RFC 9051 differently on whether an extended `LIST` still has to ask for the
//! SPECIAL-USE attributes, and the disagreement is invisible from either alone.
//!
//! Skips per server when its address variable is unset, so the offline
//! `cargo test --workspace` stays green.

#[path = "common/imap_live.rs"]
mod imap_live;

use engine_core::mail::MailboxRole;
use imap_live::{DOVECOT_REV2, STALWART, connect, connect_to, find, folders};

/// The rev2 servers this suite runs against.
const REV2_SERVERS: [imap_live::Server; 2] = [STALWART, DOVECOT_REV2];

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
async fn the_folder_roles_survive_the_dialect() {
    for server in &REV2_SERVERS {
        let Some(provider) = connect(server, "the_folder_roles_survive_the_dialect").await else {
            continue;
        };
        let all = folders(&provider).await;

        // rev2 folds SPECIAL-USE's attributes into the base `LIST` response (RFC 9051
        // §7.3.1) and defines no `RETURN (SPECIAL-USE)` option to ask with, which reads
        // like a rev2 session need never ask for them. **Dovecot's rev2 disagrees**: it
        // advertises RFC 6154 as well and keeps RFC 6154's rule, so an extended `LIST`
        // that does not ask comes back with every role stripped — no `\Sent`, and so no
        // folder for `place.rs` to file a sent copy in. Stalwart volunteers them either
        // way and can never fail this; it is here to prove the other half, that asking a
        // server which folded them in is still accepted.
        //
        // Asserted by role and never by name: the harnesses call these folders different
        // things ("Sent" vs "Sent Items"), and the role is what the client acts on.
        for role in [MailboxRole::Sent, MailboxRole::Trash, MailboxRole::Drafts] {
            let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
            assert!(
                all.iter().any(|m| m.role.as_ref() == Some(&role)),
                "{}: no {role:?} folder among {names:?}",
                server.label
            );
        }
    }
}
