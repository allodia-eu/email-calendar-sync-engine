//! Why an exchange with a server failed, and whether the request may have reached it.

use core::time::Duration;

/// A failed exchange: the HTTP client's own error, or a server that went quiet for longer
/// than its [`Deadlines`](engine_provider::Deadlines) allow.
///
/// It also says whether the request **may have been received**, which is the one thing an
/// adapter needs beyond the cause. An idempotent request is retryable either way. A
/// submission is retryable only when the server cannot have had all of it: a failed connect
/// or handshake, or a body the server stopped taking before its end. Past that point the
/// server may have acted on it, so a submission is a question for the user, never a retry.
///
/// Where the client cannot say how far a request got, this answers that it may have been
/// received. That is the answer that never sends a message twice.
#[derive(Debug)]
pub struct SendError {
    cause: Cause,
    reached: Reached,
}

#[derive(Debug)]
enum Cause {
    /// The HTTP client failed: a refused or reset connection, a TLS failure, a body that did
    /// not decode, or the dial bound the shared client carries.
    Http(reqwest::Error),
    /// The server sent or took nothing for `waited`. `what` names what did not happen.
    Silent {
        what: &'static str,
        waited: Duration,
    },
}

/// How far the request got before the exchange failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reached {
    /// The server cannot have had the whole request.
    NotWhole,
    /// The server may have had the whole request.
    Possibly,
}

impl SendError {
    /// A failure the HTTP client reported for a request that `reached` as far as stated.
    ///
    /// A connect failure, the dial bound included, is never past the server, whatever the
    /// caller believed about the body.
    pub(crate) fn http(err: reqwest::Error, reached: Reached) -> Self {
        let reached = if err.is_connect() || err.is_builder() {
            Reached::NotWhole
        } else {
            reached
        };
        Self {
            cause: Cause::Http(err),
            reached,
        }
    }

    /// A server that sent or took nothing for `waited`.
    pub(crate) fn silent(what: &'static str, waited: Duration, reached: Reached) -> Self {
        Self {
            cause: Cause::Silent { what, waited },
            reached,
        }
    }

    /// Whether the exchange ran out of time: a bound on a silent server, the shared client's
    /// dial bound, or any other timeout the HTTP client reports.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        match &self.cause {
            Cause::Http(err) => err.is_timeout(),
            Cause::Silent { .. } => true,
        }
    }

    /// Whether a body arrived and did not decode, which a retry of the same request will not
    /// change.
    #[must_use]
    pub fn is_decode(&self) -> bool {
        matches!(&self.cause, Cause::Http(err) if err.is_decode())
    }

    /// Whether the server may have received the whole request, and so may have acted on it.
    ///
    /// `false` only when it cannot have: the connection or its TLS handshake never completed,
    /// or the server stopped taking the request body before its end. An adapter reads this on
    /// a submission, where `true` means the message may have been accepted.
    #[must_use]
    pub fn may_have_been_received(&self) -> bool {
        self.reached == Reached::Possibly
    }
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.cause {
            Cause::Http(err) => core::fmt::Display::fmt(err, f),
            Cause::Silent { what, waited } => write!(f, "{what} for {}s", waited.as_secs()),
        }
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            Cause::Http(err) => Some(err),
            Cause::Silent { .. } => None,
        }
    }
}

/// A client error outside any exchange, such as a client that could not be built, or one
/// whose request is not known to have stopped short of the server.
impl From<reqwest::Error> for SendError {
    fn from(err: reqwest::Error) -> Self {
        Self::http(err, Reached::Possibly)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{Reached, SendError};

    #[test]
    fn a_silent_server_is_a_timeout_and_says_for_how_long() {
        let minute = Duration::from_mins(1);
        let err = SendError::silent("the server sent nothing", minute, Reached::Possibly);
        assert!(err.is_timeout());
        assert!(!err.is_decode());
        assert!(err.may_have_been_received());
        assert_eq!(err.to_string(), "the server sent nothing for 60s");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn a_body_the_server_stopped_taking_was_not_received() {
        let minute = Duration::from_mins(1);
        let err = SendError::silent("the server took nothing", minute, Reached::NotWhole);
        assert!(!err.may_have_been_received());
    }

    #[tokio::test]
    async fn a_connect_failure_never_reached_the_server_whatever_the_caller_believed() {
        // Bound and released, so nothing listens on the port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let refused = crate::client(&engine_tls::TlsClientConfig::bundled())
            .build()
            .expect("client")
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("nothing listens there");
        let err = SendError::http(refused, Reached::Possibly);
        assert!(!err.may_have_been_received(), "{err}");
        assert!(!err.is_timeout(), "{err}");
        assert!(std::error::Error::source(&err).is_some());
        assert!(!err.to_string().is_empty());
    }
}
