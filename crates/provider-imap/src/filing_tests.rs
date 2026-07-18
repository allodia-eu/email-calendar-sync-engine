//! Offline tests for the submission transport resolution (`resolve_smtp`), which maps
//! the configured [`SmtpSecurity`] onto the live [`SmtpSender`] a provider holds.

use tokio_rustls::TlsConnector;

use super::{SmtpSender, resolve_smtp};
use crate::config::ImapConfig;

/// A throwaway connector (accept-any); `resolve_smtp` only clones it into the sender.
fn connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

#[test]
fn resolve_smtp_maps_each_security_mode_to_its_sender() {
    let connector = connector();

    let plain = ImapConfig::new("h:993", "h", "u", "p").with_smtp("h:25");
    let sender = resolve_smtp(plain.smtp.as_ref().unwrap(), &connector, &plain);
    assert!(matches!(sender, SmtpSender::Plaintext { addr } if addr == "h:25"));

    let tls = ImapConfig::new("h:993", "h", "u", "p").with_smtp_tls("h:465", "smtp.example.com");
    let sender = resolve_smtp(tls.smtp.as_ref().unwrap(), &connector, &tls);
    assert!(matches!(
        sender,
        SmtpSender::ImplicitTls { addr, server_name, username, .. }
            if addr == "h:465" && server_name == "smtp.example.com" && username == "u"
    ));

    let starttls =
        ImapConfig::new("h:993", "h", "u", "p").with_smtp_starttls("h:587", "smtp.example.com");
    let sender = resolve_smtp(starttls.smtp.as_ref().unwrap(), &connector, &starttls);
    assert!(matches!(
        sender,
        SmtpSender::StartTls { addr, server_name, password, .. }
            if addr == "h:587" && server_name == "smtp.example.com" && password == "p"
    ));
}
