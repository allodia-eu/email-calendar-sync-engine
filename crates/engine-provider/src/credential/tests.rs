//! The credential types: nothing secret or personal in `Debug`, a buffer zeroed on drop, and
//! subjects that never collide by accident.

use zeroize::ZeroizeOnDrop;

use super::{
    Credential, CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject,
    FixedCredential, Origin, Secret,
};

const ADDRESS: &str = "alice@example.com";
const PASSWORD: &str = "hunter2-correct-horse";
const TOKEN: &str = "ya29.a-token-nobody-may-print";

fn origin() -> Origin {
    Origin::new("imaps", "imap.example.com", 993)
}

#[test]
fn a_secret_prints_nothing_of_itself() {
    let secret = Secret::new(PASSWORD);
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(secret.expose(), PASSWORD);
}

#[test]
fn a_secret_is_zeroized_on_drop() {
    fn zeroized_on_drop<T: ZeroizeOnDrop>() {}
    zeroized_on_drop::<Secret>();
}

#[test]
fn clones_of_a_secret_share_one_buffer() {
    // One buffer is what makes "zeroed when the last clone drops" mean every copy.
    let secret = Secret::new(TOKEN);
    let clone = secret.clone();
    assert!(core::ptr::eq(secret.expose(), clone.expose()));
}

#[test]
fn a_credential_prints_neither_its_secret_nor_its_username() {
    for (credential, variant) in [
        (Credential::password(ADDRESS, PASSWORD), "Password"),
        (Credential::bearer_for(ADDRESS, TOKEN), "Bearer"),
    ] {
        let shown = format!("{credential:?}");
        assert!(shown.starts_with(variant), "{shown}");
        assert!(shown.contains("username: <present>"), "{shown}");
        for leaked in [ADDRESS, "alice", PASSWORD, TOKEN] {
            assert!(!shown.contains(leaked), "{leaked} leaked: {shown}");
        }
    }
    let shown = format!("{:?}", Credential::bearer(TOKEN));
    assert!(shown.contains("username: <absent>"), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
}

#[test]
fn a_fixed_credential_prints_nothing_secret_or_personal() {
    let fixed = FixedCredential::unique(Credential::password(ADDRESS, PASSWORD));
    let shown = format!("{fixed:?}");
    for leaked in [ADDRESS, PASSWORD] {
        assert!(!shown.contains(leaked), "{leaked} leaked: {shown}");
    }
}

#[test]
fn the_username_reads_out_of_either_shape() {
    assert_eq!(
        Credential::password(ADDRESS, PASSWORD).username(),
        Some(ADDRESS)
    );
    assert_eq!(
        Credential::bearer_for(ADDRESS, TOKEN).username(),
        Some(ADDRESS)
    );
    assert_eq!(Credential::bearer(TOKEN).username(), None);
    #[expect(deprecated, reason = "the name the IMAP alias keeps compiling")]
    let old = Credential::oauth2(ADDRESS, TOKEN);
    assert!(
        matches!(&old, Credential::Bearer { username: Some(u), token } if u == ADDRESS && token.expose() == TOKEN)
    );
}

#[test]
fn subjects_minted_for_fixed_credentials_are_never_shared() {
    let one = FixedCredential::unique(Credential::bearer(TOKEN));
    let two = FixedCredential::unique(Credential::bearer(TOKEN));
    assert_ne!(one.subject(), two.subject());
    // A clone is the same source, so it keeps its subject.
    assert_eq!(one.clone().subject(), one.subject());
}

#[test]
fn a_minted_subject_never_equals_one_a_host_names() {
    let minted = CredentialSubject::unique();
    // Whatever the host calls its subject: any number the counter can have reached in this
    // process, the minted subject's own rendering, or nothing.
    let names = (0..10_000).map(|n: u32| n.to_string());
    for name in names.chain([format!("{minted:?}"), String::new()]) {
        assert_ne!(CredentialSubject::new(name.as_str()), minted, "{name}");
    }
    assert_eq!(
        CredentialSubject::new("acct-1"),
        CredentialSubject::new("acct-1")
    );
    assert_eq!(
        format!("{:?}", CredentialSubject::new("acct-1")),
        "CredentialSubject(\"acct-1\")"
    );
}

#[test]
fn an_origin_compares_lowercase() {
    let origin = Origin::new("IMAPS", "IMAP.Example.COM", 993);
    assert_eq!(origin, self::origin());
    assert_eq!(
        (origin.scheme(), origin.host(), origin.port()),
        ("imaps", "imap.example.com", 993)
    );
    assert_ne!(origin, Origin::new("imaps", "imap.example.com", 994));
    assert_ne!(origin, Origin::new("imap", "imap.example.com", 993));
}

#[test]
fn a_request_carries_its_protocol_and_origin() {
    let origin = origin();
    let request = CredentialRequest::new(CredentialProtocol::Smtp, &origin);
    assert_eq!(request.protocol, CredentialProtocol::Smtp);
    assert_eq!(request.origin, &origin);
}

#[tokio::test]
async fn a_fixed_credential_answers_the_same_every_time_and_never_replaces() {
    let subject = CredentialSubject::new("acct-1");
    let fixed = FixedCredential::new(subject.clone(), Credential::password(ADDRESS, PASSWORD));
    let origin = origin();
    let request = CredentialRequest::new(CredentialProtocol::Imap, &origin);

    assert_eq!(fixed.subject(), &subject);
    assert!(fixed.permits(&origin), "the default permits every origin");
    for _ in 0..2 {
        let answer = fixed.credential(&request).await.expect("a fixed answer");
        assert!(
            matches!(&answer, Credential::Password { password, .. } if password.expose() == PASSWORD)
        );
    }
    let refused = fixed.credential(&request).await.expect("a fixed answer");
    let replacement = fixed.replace(&refused, &request).await.expect("no failure");
    assert!(replacement.is_none(), "a fixed credential never replaces");
}
