//! Gated live integration: what is true only on an **IMAP4rev1** wire.
//!
//! Split from `live_imap_contract.rs` by dialect rather than by vendor, because the
//! dialect is what decides these — a server moves between this file and
//! `live_imap_rev2.rs` when it starts offering rev2 and the client enables it, without
//! either file being rewritten.
//!
//! Today that is the rev1 half of the Dovecot harness, which exists to *stay* rev1: the
//! same image runs a rev2 service beside it (`docker/dovecot/rev1.conf` vs `rev2.conf`),
//! and the client `ENABLE`s rev2 wherever it is offered, so nothing that advertises the
//! dialect can hold this half up.
//!
//! Skips when `DOVECOT_REV1_IMAP_ADDR` is unset, so the offline `cargo test --workspace`
//! stays green.

#[path = "common/imap_live.rs"]
mod imap_live;

use engine_provider::Provider;
use imap_live::{DOVECOT_REV1, connect, connect_to, find, folders};

/// The rev1 servers this suite runs against.
const REV1_SERVERS: [imap_live::Server; 1] = [DOVECOT_REV1];

#[tokio::test]
async fn a_modified_utf7_name_is_decoded_into_the_mailbox_identity() {
    for server in &REV1_SERVERS {
        let Some(provider) = connect(
            server,
            "a_modified_utf7_name_is_decoded_into_the_mailbox_identity",
        )
        .await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // rev1 encodes a non-ASCII mailbox name as modified UTF-7 (RFC 3501 §5.1.3). The
        // decoded form is the identity, so nothing above the transport ever sees
        // `&ANw-berweisungen` — and a client that skipped decoding here would show the
        // user that string as a folder name.
        let mailbox = find(&all, "Überweisungen");
        assert_eq!(mailbox.id.as_str(), "Überweisungen", "{}", server.label);

        // No name in the list still carries a shift sequence.
        for other in &all {
            assert!(
                !other.name.contains("&-") && !other.name.contains("&A"),
                "{} left {} encoded",
                server.label,
                other.name
            );
        }
    }
}

#[tokio::test]
async fn selecting_by_the_decoded_identity_reaches_the_mailbox() {
    for server in &REV1_SERVERS {
        let Some(provider) = connect_to(
            server,
            "Überweisungen",
            "selecting_by_the_decoded_identity_reaches_the_mailbox",
        )
        .await
        else {
            continue;
        };

        // The other half of the round trip: the transport has to put the modified-UTF-7
        // form back, or this `SELECT` names a mailbox the server never advertised. Sending
        // the decoded name unencoded fails here and nowhere else in the suite.
        let account = engine_core::ids::AccountId::try_from("live-harness").unwrap();
        provider
            .sync_email(&account, None)
            .await
            .unwrap_or_else(|err| panic!("{}: select a decoded name: {err}", server.label));
    }
}
