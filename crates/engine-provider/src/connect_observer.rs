//! What happened *while* an adapter established its connection.
//!
//! [`ConnectionInfo`](crate::ConnectionInfo) reports the **outcome** of a connect: it
//! is a sync, infallible fact accessor, so it can describe a connection only once one
//! exists. Everything that happened on the way there — the well-known `30x` hops JMAP
//! and CalDAV follow themselves, the TLS handshake, authentication, the endpoint
//! discovery settled on — used to be discarded when `connect` resolved. A
//! [`ConnectObserver`] receives those as they happen, so a host can log or surface
//! connection progress.
//!
//! The shape mirrors `engine-sync`'s `SyncObserver`: a trait, a blanket impl over
//! `Fn`, a no-op default ([`IgnoreConnectSteps`]), and a borrowed payload — the engine
//! allocates nothing and a host clones only what it keeps.
//!
//! # Steps, never states
//!
//! An adapter emits *steps*; it does not model a `Disconnected`/`Connecting`/`Connected`
//! state machine, because it cannot do so truthfully: three of the four adapters are
//! only constructible via a completed `connect()`, so `Connecting` is unobservable
//! through any accessor, and `connection_info()` is sync and infallible, so it cannot
//! detect a socket that has since died. A **host** owns that state machine — it is the
//! layer that knows a call just failed with
//! [`FailureClass::Retryable`](engine_core::error::FailureClass::Retryable) and that a
//! reconnect is in flight. The engine's job is to give it the inputs: the `connect()`
//! future, its `Ok`/`Err`, the [`FailureClass`](engine_core::error::FailureClass), the
//! [`ConnectionInfo`](crate::ConnectionInfo), and these steps.
//!
//! # What each adapter emits
//!
//! The asymmetry is the one [`ConnectionInfo`](crate::ConnectionInfo) already models: a
//! `reqwest` adapter cannot report a TLS version, and Graph has no connect exchange at
//! all.
//!
//! | Adapter | Steps |
//! |---|---|
//! | `provider-imap` | [`TlsEstablished`](ConnectStep::TlsEstablished) after the handshake, [`Authenticated`](ConnectStep::Authenticated) after `LOGIN` |
//! | `provider-jmap` | [`Redirected`](ConnectStep::Redirected) per well-known hop, [`Authenticated`](ConnectStep::Authenticated) on the session `2xx`, [`Discovered`](ConnectStep::Discovered) with the `apiUrl` |
//! | `provider-caldav` | [`Redirected`](ConnectStep::Redirected) per hop, [`Discovered`](ConnectStep::Discovered) with the calendar-home href |
//! | `provider-graph` | nothing — `GraphClient::connect` performs no I/O |

use std::borrow::Cow;

use crate::TlsVersion;

/// One thing that happened while a provider established its connection.
///
/// The URL-carrying variants are `#[non_exhaustive]`, so only this crate constructs
/// them: an adapter must go through [`ConnectStep::redirected`] /
/// [`ConnectStep::discovered`], which scrub URL userinfo. That makes redaction a
/// property of the type rather than a rule each adapter has to remember
/// (`north-star.md` forbids secrets reaching logs, and this feature exists to feed
/// them). A host matching those variants writes a trailing `..`.
///
/// ```
/// use engine_provider::ConnectStep;
///
/// let step = ConnectStep::redirected(
///     "https://alice:pw@example.com/.well-known/jmap",
///     "/jmap/session",
/// );
/// let ConnectStep::Redirected { from, to, .. } = &step else {
///     unreachable!()
/// };
/// assert_eq!(from, "https://example.com/.well-known/jmap");
/// assert_eq!(to, "/jmap/session");
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectStep<'a> {
    /// A redirect the adapter resolved itself (the well-known `30x` chain — both JMAP
    /// and CalDAV disable their HTTP client's redirect following so they can rebase a
    /// foreign advertised origin onto the connection).
    ///
    /// Build with [`ConnectStep::redirected`]; both URLs are userinfo-scrubbed.
    #[non_exhaustive]
    Redirected {
        /// The URL that answered with the redirect.
        from: Cow<'a, str>,
        /// The `Location` it pointed at.
        to: Cow<'a, str>,
    },
    /// The TLS handshake completed at this version. Only a `tokio-rustls` adapter can
    /// report this: `reqwest` exposes the peer certificate but never the negotiated
    /// protocol version (`docs/agent-guidance/tls.md`).
    TlsEstablished(TlsVersion),
    /// The server accepted the account's credentials.
    Authenticated,
    /// Discovery settled on the endpoint that will serve requests — a JMAP `apiUrl`, a
    /// CalDAV calendar-home href.
    ///
    /// Build with [`ConnectStep::discovered`]; the endpoint is userinfo-scrubbed.
    #[non_exhaustive]
    Discovered {
        /// The resolved endpoint.
        endpoint: Cow<'a, str>,
    },
}

impl<'a> ConnectStep<'a> {
    /// A resolved redirect hop, with any credentials scrubbed from both URLs.
    #[must_use]
    pub fn redirected(from: &'a str, to: &'a str) -> Self {
        Self::Redirected {
            from: scrub_userinfo(from),
            to: scrub_userinfo(to),
        }
    }

    /// The endpoint discovery settled on, with any credentials scrubbed.
    #[must_use]
    pub fn discovered(endpoint: &'a str) -> Self {
        Self::Discovered {
            endpoint: scrub_userinfo(endpoint),
        }
    }
}

/// Removes the whole `userinfo@` component from a URL's authority, borrowing
/// unchanged when there is none.
///
/// A redirect `Location` may carry `https://user:pw@host/path`, and these steps exist
/// to be logged. The **entire** userinfo goes, not just the password: a bare
/// `https://token@host/` userinfo is itself a secret, so there is no half of it worth
/// keeping.
///
/// Only the authority is inspected, so a `@` elsewhere is left alone: a path
/// (`https://host/path@x`) or a relative href (`/dav/cal/alice%40host/`, which every
/// CalDAV redirect target looks like) round-trips untouched and unallocated.
fn scrub_userinfo(url: &str) -> Cow<'_, str> {
    let Some(scheme_end) = url.find("://") else {
        return Cow::Borrowed(url);
    };
    let authority_start = scheme_end + "://".len();
    let rest = &url[authority_start..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // The *last* `@` ends the userinfo (RFC 3986 §3.2.1 permits `:` and pct-encoded
    // bytes inside it), matching how URL parsers split the authority.
    let Some(at) = authority.rfind('@') else {
        return Cow::Borrowed(url);
    };
    let mut scrubbed = String::with_capacity(url.len() - at - 1);
    scrubbed.push_str(&url[..authority_start]);
    scrubbed.push_str(&authority[at + 1..]);
    scrubbed.push_str(&rest[authority_end..]);
    Cow::Owned(scrubbed)
}

/// A sink an adapter notifies as it establishes a connection.
///
/// Carried on the adapter's config (`ImapConfig`, `JmapConfig`, and `CalDavConfig`
/// each take one via `with_connect_observer`), not passed to `connect` — so a host
/// that rebuilds a provider from its config after a dropped session observes the
/// redial for free, with no extra plumbing.
///
/// Implementations must be cheap and non-blocking (record into a shared snapshot, push
/// onto a channel); the connect awaits nothing on them. The blanket impl over
/// `Fn(&ConnectStep)` lets a caller pass a closure directly.
pub trait ConnectObserver: Send + Sync {
    /// Receives one step of the connect phase.
    fn step(&self, step: &ConnectStep<'_>);
}

impl<F: Fn(&ConnectStep<'_>) + Send + Sync> ConnectObserver for F {
    fn step(&self, step: &ConnectStep<'_>) {
        self(step);
    }
}

/// A [`ConnectObserver`] that ignores every step — the default, for a connect whose
/// caller wants no progress.
#[derive(Debug, Clone, Copy, Default)]
pub struct IgnoreConnectSteps;

impl ConnectObserver for IgnoreConnectSteps {
    fn step(&self, _step: &ConnectStep<'_>) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Renders a step the way a host's log line would, so the assertions below read as
    /// "what would have reached the log".
    fn render(step: &ConnectStep<'_>) -> String {
        match step {
            ConnectStep::Redirected { from, to, .. } => format!("redirect {from} -> {to}"),
            ConnectStep::TlsEstablished(version) => format!("tls {version:?}"),
            ConnectStep::Authenticated => "authenticated".to_owned(),
            ConnectStep::Discovered { endpoint, .. } => format!("discovered {endpoint}"),
        }
    }

    #[test]
    fn a_closure_observes_every_step() {
        // The blanket `Fn` impl: a host passes a closure, not a named type.
        let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let observer = |step: &ConnectStep<'_>| seen.lock().unwrap().push(render(step));

        observer.step(&ConnectStep::redirected("https://h/.well-known/jmap", "/s"));
        observer.step(&ConnectStep::TlsEstablished(TlsVersion::Tls1_3));
        observer.step(&ConnectStep::Authenticated);
        observer.step(&ConnectStep::discovered("https://h/jmap/"));

        assert_eq!(
            *seen.lock().unwrap(),
            [
                "redirect https://h/.well-known/jmap -> /s",
                "tls Tls1_3",
                "authenticated",
                "discovered https://h/jmap/",
            ]
        );
    }

    #[test]
    fn the_default_observer_ignores_every_step() {
        IgnoreConnectSteps.step(&ConnectStep::Authenticated);
        IgnoreConnectSteps.step(&ConnectStep::TlsEstablished(TlsVersion::Tls1_2));
        IgnoreConnectSteps.step(&ConnectStep::redirected("https://a/", "https://b/"));
        IgnoreConnectSteps.step(&ConnectStep::discovered("https://b/api"));
    }

    #[test]
    fn a_step_is_debug_for_a_host_that_just_logs_it() {
        let shown = format!("{:?}", ConnectStep::redirected("https://a/", "https://b/"));
        assert!(shown.contains("Redirected"), "{shown}");
    }

    #[test]
    fn credentials_never_reach_an_observer() {
        // The invariant this feature turns on: a redirect `Location` (or an advertised
        // `apiUrl`) may carry userinfo, and these steps are built to be logged.
        let step = ConnectStep::redirected(
            "https://alice:hunter2@example.com/.well-known/caldav",
            "https://token@dav.example.com/cal/",
        );
        let rendered = render(&step);
        assert_eq!(
            rendered,
            "redirect https://example.com/.well-known/caldav -> https://dav.example.com/cal/"
        );
        assert!(!rendered.contains("hunter2") && !rendered.contains("token"));

        let discovered = ConnectStep::discovered("https://u:p@api.example.com/jmap/");
        assert_eq!(
            render(&discovered),
            "discovered https://api.example.com/jmap/"
        );
    }

    #[test]
    fn a_url_without_userinfo_is_borrowed_unchanged() {
        // Zero-copy on the overwhelmingly common path, and — the correctness half —
        // an `@` outside the authority is not userinfo and must survive.
        for clean in [
            "https://example.com/jmap/",
            "https://example.com/path@x",           // `@` in the path
            "/dav/cal/alice%40test.local/",         // a relative CalDAV href
            "https://example.com:8443/a?q=x@y#f@g", // `@` in query and fragment
            "mailto:alice@example.com",             // no `://` authority at all
        ] {
            assert!(
                matches!(scrub_userinfo(clean), Cow::Borrowed(kept) if kept == clean),
                "{clean} should be borrowed unchanged"
            );
        }
    }

    #[test]
    fn userinfo_is_scrubbed_from_every_authority_shape() {
        for (raw, want) in [
            ("https://u:p@h/x", "https://h/x"),
            ("https://token@h/x", "https://h/x"),
            ("https://@h/x", "https://h/x"), // empty userinfo
            ("https://u:p@h", "https://h"),  // no path
            ("https://u:p@h:8443/x", "https://h:8443/x"), // port survives
            ("https://u:p@h/a@b", "https://h/a@b"), // only the authority's `@` goes
            ("https://u@h?q=1", "https://h?q=1"), // authority ends at `?`
            ("https://u@h#f", "https://h#f"), // authority ends at `#`
            ("http://u:p@h/x", "http://h/x"), // any scheme
        ] {
            assert_eq!(scrub_userinfo(raw), want, "scrubbing {raw}");
        }
    }
}
