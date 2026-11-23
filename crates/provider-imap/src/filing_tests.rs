//! Offline tests for the submission transport resolution (`resolve_smtp`), which maps
//! the configured [`SmtpSecurity`] onto the live [`SmtpSender`] a provider holds.

use engine_provider::{Credential, Origin};
use tokio_rustls::TlsConnector;

use super::{SmtpSender, resolve_smtp};
use crate::config::ImapConfig;

/// A throwaway connector (accept-any); `resolve_smtp` only clones it into the sender.
fn connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

fn config() -> ImapConfig {
    ImapConfig::new("h:993", "h", Credential::password("u", "p"))
}

#[test]
fn resolve_smtp_maps_each_security_mode_to_its_sender() {
    let connector = connector();

    let plain = config().with_smtp("h:25");
    let sender = resolve_smtp(plain.smtp.as_ref().unwrap(), &connector, &plain);
    assert!(matches!(&sender, SmtpSender::Plaintext { addr } if addr == "h:25"));
    // The fixture's local MX takes no credential at all.
    assert!(sender.auth(&Credential::password("u", "p")).is_none());

    let tls = config().with_smtp_tls("h:465", "smtp.example.com");
    let sender = resolve_smtp(tls.smtp.as_ref().unwrap(), &connector, &tls);
    assert!(matches!(
        &sender,
        SmtpSender::ImplicitTls { addr, server_name, .. }
            if addr == "h:465" && server_name == "smtp.example.com"
    ));

    let starttls = config().with_smtp_starttls("h:587", "smtp.example.com");
    let sender = resolve_smtp(starttls.smtp.as_ref().unwrap(), &connector, &starttls);
    assert!(matches!(
        &sender,
        SmtpSender::StartTls { addr, server_name, .. }
            if addr == "h:587" && server_name == "smtp.example.com"
    ));
}

#[test]
fn an_authenticating_sender_names_its_own_host_and_port() {
    // A SASL `OAUTHBEARER` response describes the connection it rides on (RFC 7628
    // §3.1), and submission is a different host and port from the IMAP session that
    // files the sent copy — so these must come from the SMTP settings, not the dial.
    let config = ImapConfig::new(
        "imap.example.com:993",
        "imap.example.com",
        Credential::bearer_for("u@example.com", "tok"),
    )
    .with_smtp_tls("smtp.example.com:465", "smtp.example.com");
    let sender = resolve_smtp(config.smtp.as_ref().unwrap(), &connector(), &config);
    let presented = Credential::bearer_for("u@example.com", "tok");
    let auth = sender.auth(&presented).expect("a TLS sender authenticates");
    assert_eq!(auth.host, "smtp.example.com");
    assert_eq!(auth.port, Some(465));
    assert_eq!(auth.credential.username(), Some("u@example.com"));
}

#[test]
fn a_submission_tells_the_source_the_smtp_servers_name_port_and_how_tls_begins() {
    let connector = connector();
    let origin_of =
        |config: ImapConfig| match resolve_smtp(config.smtp.as_ref().unwrap(), &connector, &config)
        {
            SmtpSender::ImplicitTls { origin, .. } | SmtpSender::StartTls { origin, .. } => origin,
            SmtpSender::Plaintext { .. } => panic!("an authenticating sender"),
        };

    let tls = config().with_smtp_tls("127.0.0.1:11465", "SMTP.example.com");
    assert_eq!(
        origin_of(tls),
        Origin::new("submissions", "smtp.example.com", 11465)
    );
    let starttls = config().with_smtp_starttls("127.0.0.1:11587", "smtp.example.com");
    assert_eq!(
        origin_of(starttls),
        Origin::new("submission", "smtp.example.com", 11587)
    );
    // No port in the address: the default for how the connection is secured.
    let bare = config().with_smtp_tls("smtp.example.com", "smtp.example.com");
    assert_eq!(origin_of(bare).port(), 465);
    let bare = config().with_smtp_starttls("smtp.example.com", "smtp.example.com");
    assert_eq!(origin_of(bare).port(), 587);
}
