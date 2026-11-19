//! Where a redirect may take a connection.
//!
//! JMAP and CalDAV both disable their HTTP client's redirect following, so each hop of
//! a discovery chain is theirs to resolve. [`redirect_target`] is the one rule both
//! apply, so the two cannot drift on what a `Location` means or on where a chain is
//! allowed to end up. Which origin may then *hold the credential* is
//! [`same_origin`](crate::same_origin)'s question, and it is a different one.

/// Resolves a redirect's `Location` against the URI that issued it, returning the next
/// hop to request.
///
/// RFC 9110 §10.2.2 makes `Location` a URI-reference resolved against the **effective
/// request URI** — the URL that just answered — not against whatever base the
/// connection was configured with. The distinction is invisible on a chain that never
/// leaves its origin and decisive on one that does: a provider whose apex sends
/// `https://mail.example.com/.well-known/jmap` and whose second hop answers a bare
/// `/jmap/session` is resolvable only if the second hop is read against the first's
/// target. Resolved against the original base it walks back to the apex, and a chain
/// that oscillates between two hosts exhausts its hop budget and fails as a redirect
/// loop.
///
/// A redirect is a server's instruction about where a resource *is*, so it is resolved
/// verbatim. It is not an advertised URL in a payload the server generated about
/// itself, which a caller may have reason to rebase onto the connection.
///
/// Returns `None` when `current` or the resolved target will not parse, when the target
/// names something other than `http`/`https` (nothing a discovery walk requests), and
/// when the hop would leave TLS: these requests carry the account's credentials, so an
/// `https` → `http` hop would put a password or bearer token on the wire in cleartext.
/// A chain that began in plaintext (the loopback fixtures) is left alone.
///
/// One `https` → `http` hop is taken over TLS instead: one to the **same host and port** that
/// just answered over TLS (a default port on both sides counts as the same). A server behind a
/// TLS-terminating proxy builds its redirects as if it were served in plaintext (Nextcloud's
/// `.well-known` rewrite does, out of the box), so the same place over TLS is where the hop
/// meant to go. The certificate is verified as on any other hop. Another host or port says
/// nothing about whether TLS is served there, and is refused, as is a hop that over TLS would
/// name the URL that issued it: that host sends its TLS visitors to plaintext, and following
/// it over TLS would only be redirected again.
///
/// Any `user:pw@` the `Location` carried is dropped. It is server-controlled, and the
/// resolved URL is both logged and kept (it becomes the base the JMAP session's
/// advertised URLs resolve against), so no reader downstream has to remember to scrub.
///
/// # Examples
///
/// ```
/// use engine_provider::redirect_target;
///
/// // A relative second hop stays on the host the first hop moved to.
/// assert_eq!(
///     redirect_target("https://mail.example.com/.well-known/jmap", "/jmap/session").as_deref(),
///     Some("https://mail.example.com/jmap/session")
/// );
/// // Leaving TLS for the same host stays on TLS; for another host it is refused.
/// assert_eq!(
///     redirect_target("https://mail.example.com/x", "http://mail.example.com/y").as_deref(),
///     Some("https://mail.example.com/y")
/// );
/// assert_eq!(
///     redirect_target("https://example.com/x", "http://mail.example.com/y"),
///     None
/// );
/// ```
#[must_use]
pub fn redirect_target(current: &str, location: &str) -> Option<String> {
    let current = url::Url::parse(current).ok()?;
    let mut next = current.join(location).ok()?;
    if !matches!(next.scheme(), "http" | "https") {
        return None;
    }
    if current.scheme() == "https" && next.scheme() != "https" {
        if next.host() != current.host() || next.port() != current.port() {
            return None;
        }
        next.set_scheme("https").ok()?;
        if next == current {
            return None;
        }
    }
    // Both setters only fail on a URL that cannot be a base, which the scheme check
    // above has already excluded.
    next.set_username("").ok()?;
    next.set_password(None).ok()?;
    Some(next.into())
}

/// What a host says of a `Location` [`redirect_target`] refused, so every adapter says it the
/// same way.
#[must_use]
pub fn redirect_refused(location: &str) -> String {
    format!("redirect to {location:?} not followed: it does not resolve, or it leaves HTTPS")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_redirect_resolves_against_the_uri_that_issued_it() {
        // The RFC 9110 §10.2.2 rule, and the whole point of the helper: hop two is a
        // bare path served by the host hop one moved us to, so it must stay there.
        assert_eq!(
            super::redirect_target(
                "https://example.com/.well-known/jmap",
                "https://mail.example.com/.well-known/jmap"
            )
            .as_deref(),
            Some("https://mail.example.com/.well-known/jmap")
        );
        assert_eq!(
            super::redirect_target("https://mail.example.com/.well-known/jmap", "/jmap/session")
                .as_deref(),
            Some("https://mail.example.com/jmap/session"),
        );
        // A relative reference resolves against the current URI's directory.
        assert_eq!(
            super::redirect_target("https://mail.example.com/jmap/hop", "session").as_deref(),
            Some("https://mail.example.com/jmap/session")
        );
    }

    #[test]
    fn a_redirect_off_tls_to_the_same_host_is_taken_over_tls() {
        // A server behind a TLS-terminating proxy builds its redirects as if it were
        // served in plaintext. The host has just answered over TLS, so the same place
        // over TLS is where the hop meant to go.
        assert_eq!(
            super::redirect_target(
                "https://dav.example.com/.well-known/caldav",
                "http://dav.example.com/remote.php/dav/"
            )
            .as_deref(),
            Some("https://dav.example.com/remote.php/dav/")
        );
        // Port 80 is the plaintext default, so it names no port of its own.
        assert_eq!(
            super::redirect_target(
                "https://dav.example.com/.well-known/caldav",
                "http://dav.example.com:80/remote.php/dav/"
            )
            .as_deref(),
            Some("https://dav.example.com/remote.php/dav/")
        );
        // A proxy on a port of its own that passes the port through: the port that answered
        // over TLS is the one the hop names.
        assert_eq!(
            super::redirect_target(
                "https://dav.example.com:8443/.well-known/caldav",
                "http://dav.example.com:8443/remote.php/dav/"
            )
            .as_deref(),
            Some("https://dav.example.com:8443/remote.php/dav/")
        );
    }

    #[test]
    fn a_redirect_off_tls_to_another_port_or_to_itself_is_refused() {
        // Port 443 never answered, so nothing says TLS is served there.
        assert_eq!(
            super::redirect_target(
                "https://dav.example.com:8443/.well-known/caldav",
                "http://dav.example.com/remote.php/dav/"
            ),
            None
        );
        // A host sending its TLS visitors to the same URL in plaintext: taken over TLS, the hop
        // would only redirect again.
        assert_eq!(
            super::redirect_target(
                "https://dav.example.com/.well-known/caldav",
                "http://dav.example.com/.well-known/caldav"
            ),
            None
        );
    }

    #[test]
    fn a_redirect_off_tls_is_refused() {
        // The request carries the account's credentials, so a hop that leaves TLS
        // would put them on the wire in cleartext. Another host, or a port of its
        // own, says nothing about whether TLS is served there.
        assert_eq!(
            super::redirect_target(
                "https://example.com/.well-known/jmap",
                "http://mail.example.com/jmap/session"
            ),
            None
        );
        assert_eq!(
            super::redirect_target(
                "https://mail.example.com/.well-known/jmap",
                "http://mail.example.com:8080/jmap/session"
            ),
            None
        );
        // A plaintext chain (the loopback test fixtures) may stay plaintext.
        assert_eq!(
            super::redirect_target("http://127.0.0.1:8080/.well-known/jmap", "/jmap/session")
                .as_deref(),
            Some("http://127.0.0.1:8080/jmap/session")
        );
    }

    #[test]
    fn a_redirect_leaving_http_altogether_is_refused() {
        // Nothing a discovery walk requests is anything but `http`/`https`, and a
        // plaintext chain must not be talked into naming a local file.
        assert_eq!(
            super::redirect_target("http://127.0.0.1:8080/x", "file:///etc/passwd"),
            None
        );
    }

    #[test]
    fn an_unresolvable_redirect_is_refused() {
        assert_eq!(super::redirect_target("not a url", "/jmap/session"), None);
        // `Url::join` rejects a target it cannot make sense of against a valid base.
        assert_eq!(
            super::redirect_target("https://mail.example.com/x", "http://[::bad"),
            None
        );
    }

    #[test]
    fn a_redirect_target_is_stripped_of_the_userinfo_it_carried() {
        // A `Location` is server-controlled and may name credentials. The resolved URL
        // is logged and kept as the session base, so the userinfo goes here rather than
        // at every reader.
        assert_eq!(
            super::redirect_target("https://example.com/x", "https://u:pw@mail.example.com/y")
                .as_deref(),
            Some("https://mail.example.com/y")
        );
    }
}
