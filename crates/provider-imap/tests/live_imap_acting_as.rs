//! Gated live proof of an administrator signing in as another account
//! ([`Credential::ActingAs`]): IMAP `AUTHENTICATE PLAIN` with an authorization identity
//! (RFC 4616), against a server configured for it and against one that ignores it.
//!
//! - **Dovecot**, rev1 and rev2 (`DOVECOT_REV1_IMAP_ADDR`, `DOVECOT_REV2_IMAP_ADDR`), whose harness
//!   configures a master user (`docker/dovecot/master-user.conf`). The administrator acting as
//!   alice reads *alice's* mailbox, and acting as bob reads bob's: the authorization identity, not
//!   the administrator, decides the mailbox. An account that is not a master user naming another
//!   account, and a wrong master password, are both refused as authentication failures. Dovecot
//!   bans nobody, so the refusals cost nothing.
//! - **Stalwart** (`STALWART_IMAP_ADDR`), which ignores the authorization identity. Alice naming
//!   bob signs in to alice's own mailbox; the recovery administrator (`STALWART_RECOVERY_ADMIN`,
//!   outside the directory) naming alice signs in to its own, both with `OK`. Stalwart's own
//!   impersonation syntax (`alice%admin` as the username) does take the recovery administrator to
//!   alice's mailbox, so it may act as alice: the request that asks is answered, and only the
//!   authorization identity is ignored (`docs/spec-deviations.md`). No sign-in here fails, so the
//!   Stalwart ban budget is untouched.
//!
//! Which mailbox opened is read from the seed: `01-plain.eml` (`<baseline-0001@test.local>`) is
//! in alice's INBOX on every harness, and in neither bob's nor the administrator's.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    sync::SyncUpdate,
};
use engine_provider::{Credential, Provider};
use provider_imap::{ImapAccount, ImapConfig, ImapError};

/// The seeded message only alice's INBOX holds.
const ALICE_ONLY: &str = "baseline-0001@test.local";
const ALICE: &str = "alice@test.local";
const BOB: &str = "bob@test.local";

/// The Dovecot harness's master user (`docker/dovecot/master-users`), throwaway.
const MASTER: &str = "master@test.local";
const MASTER_PW: &str = "dovecot-master-pw";
/// The image's static passdb password, which signs in any account as itself.
const DOVECOT_PW: &str = "dovecot-alice-pw";

const DOVECOT: [(&str, &str); 2] = [
    ("dovecot-rev1", "DOVECOT_REV1_IMAP_ADDR"),
    ("dovecot-rev2", "DOVECOT_REV2_IMAP_ADDR"),
];

fn address(label: &str, var: &str, test: &str) -> Option<String> {
    let addr = std::env::var(var).ok();
    if addr.is_none() {
        eprintln!("skipping {test} on {label}: {var} unset");
    }
    addr
}

/// Connects with `credential`, the way a host does.
async fn connect(addr: &str, credential: Credential) -> Result<ImapAccount<Tls>, ImapError> {
    let host = addr.rsplit_once(':').map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(addr, host, credential);
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    ImapAccount::connect(&config, connector).await
}

type Tls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Whether the INBOX this credential opens holds alice's seeded message.
async fn opens_alices_inbox(label: &str, addr: &str, credential: Credential) -> bool {
    let account = connect(addr, credential)
        .await
        .unwrap_or_else(|err| panic!("{label}: sign in: {err}"));
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let scope = provider
        .sync_email(&AccountId::try_from("live-acting-as").unwrap(), None)
        .await
        .unwrap_or_else(|err| panic!("{label}: sync the INBOX: {err}"));
    let SyncUpdate::Snapshot { objects, .. } = scope.update else {
        panic!("{label}: a first pass is a snapshot");
    };
    objects.iter().any(|message| {
        message
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == ALICE_ONLY)
    })
}

#[tokio::test]
async fn the_authorization_identity_decides_whose_mailbox_opens() {
    for (label, var) in DOVECOT {
        let test = "the_authorization_identity_decides_whose_mailbox_opens";
        let Some(addr) = address(label, var, test) else {
            continue;
        };
        let as_alice = Credential::acting_as(MASTER, MASTER_PW, ALICE);
        assert!(
            opens_alices_inbox(label, &addr, as_alice).await,
            "{label}: the administrator acting as alice must read alice's INBOX"
        );
        // The same administrator, another authorization identity: another mailbox.
        let as_bob = Credential::acting_as(MASTER, MASTER_PW, BOB);
        assert!(
            !opens_alices_inbox(label, &addr, as_bob).await,
            "{label}: acting as bob opened alice's INBOX"
        );
    }
}

#[tokio::test]
async fn naming_an_account_without_the_right_to_act_as_it_is_refused() {
    let refused = [
        // An account that is not a master user, naming another account's mailbox.
        (
            "a non-master",
            Credential::acting_as(BOB, DOVECOT_PW, ALICE),
        ),
        (
            "a wrong master password",
            Credential::acting_as(MASTER, "not-it", ALICE),
        ),
    ];
    for (label, var) in DOVECOT {
        let test = "naming_an_account_without_the_right_to_act_as_it_is_refused";
        let Some(addr) = address(label, var, test) else {
            continue;
        };
        for (case, credential) in &refused {
            let Err(err) = connect(&addr, credential.clone()).await else {
                panic!("{label}: {case} signed in");
            };
            assert!(
                matches!(err, ImapError::Auth(_)),
                "{label}: {case}: {err:?}"
            );
            assert_eq!(
                err.failure_class(),
                FailureClass::Authentication,
                "{label}: {case}"
            );
        }
    }
}

/// Stalwart's recovery administrator (`STALWART_RECOVERY_ADMIN` in
/// `docker/stalwart/docker-compose.yml`): configured outside the directory, and the account
/// every harness management call signs in as.
const STALWART_ADMIN: &str = "admin";
const STALWART_ADMIN_PW: &str = "harness-admin-pw";
/// Alice's harness password on Stalwart (`docker/stalwart/docker-compose.yml`).
const STALWART_ALICE_PW: &str = "harness-alice-pw";

#[tokio::test]
async fn stalwart_ignores_the_authorization_identity() {
    let test = "stalwart_ignores_the_authorization_identity";
    let Some(addr) = address("stalwart", "STALWART_IMAP_ADDR", test) else {
        return;
    };
    // The control: Stalwart's own impersonation, the target and the administrator joined in
    // the username, opens alice's mailbox with the administrator's password. So the
    // administrator may act as alice, and the seed marker tells the two mailboxes apart.
    let composite = Credential::password(format!("{ALICE}%{STALWART_ADMIN}"), STALWART_ADMIN_PW);
    assert!(
        opens_alices_inbox("stalwart", &addr, composite).await,
        "Stalwart's own impersonation must reach alice's INBOX"
    );
    // Alice naming bob, which she may not act as, is signed in to her own mailbox rather than
    // refused.
    let alice_as_bob = Credential::acting_as(ALICE, STALWART_ALICE_PW, BOB);
    assert!(
        opens_alices_inbox("stalwart", &addr, alice_as_bob).await,
        "Stalwart no longer signs alice in to her own mailbox when she names bob"
    );
    // The recovery administrator naming alice, which it may act as, is signed in, without an
    // error, to its own mailbox.
    let acting_as = Credential::acting_as(STALWART_ADMIN, STALWART_ADMIN_PW, ALICE);
    assert!(
        !opens_alices_inbox("stalwart", &addr, acting_as).await,
        "Stalwart now honours the authorization identity: update spec-deviations.md and the \
         warning on Credential::ActingAs"
    );
}
