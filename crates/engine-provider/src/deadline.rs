//! How long any provider waits on a server that has stopped answering.
//!
//! A server that accepted the connection and then says nothing (a paused process behind a port
//! forwarder, a captive portal, a stalled middlebox, a NAT mapping that died) reports nothing
//! either, and a wait on it lasts forever. Every transport the engine speaks bounds every such
//! wait by one of the four categories here, so IMAP, SMTP, JMAP, Graph, Google and DAV give up
//! on a silent server at the same points and after the same time.
//!
//! A bound that fires fails the operation the way a connection lost at that point already
//! does. For an idempotent read that is a retryable failure. For a submission it depends on
//! whether the request could have reached the server: retryable before, a question for the
//! user after, because the message may have been accepted. Nothing that has been handed over
//! is ever sent a second time on a timeout.
//!
//! A stream that is silent on purpose (an IMAP `IDLE`, a JMAP event stream) is bounded by its
//! own keep-alive instead, never by these.

use core::time::Duration;

/// The bounds on waiting for a server, by what is being waited for.
///
/// One value, [`STANDARD`](Self::STANDARD), used by every transport. There is deliberately no
/// way for a host to choose others: each figure is a judgement about servers, not about a
/// device or a user, and a bound a host tuned per platform would make one provider's silence
/// a failure on one device and a hang on another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    dial: Duration,
    reply: Duration,
    stall: Duration,
    submission: Duration,
}

impl Deadlines {
    /// The bounds every transport uses.
    pub const STANDARD: Self = Self {
        dial: Duration::from_secs(30),
        reply: Duration::from_mins(1),
        stall: Duration::from_mins(1),
        submission: Duration::from_mins(10),
    };

    /// How long a TCP connect, and then the TLS handshake over it, may each take (30 s).
    ///
    /// Both are a few round trips to a server that is up, so half a minute leaves room for a
    /// congested mobile link and none for a server that is not answering. Unbounded, a
    /// connect waits out the operating system's own retries (over two minutes on Linux) and a
    /// handshake waits forever.
    #[must_use]
    pub const fn dial(self) -> Duration {
        self.dial
    }

    /// How long a server may stay silent when it owes an answer to a request it has been sent
    /// in full (1 min): an IMAP or SMTP greeting, every IMAP command's response, every SMTP
    /// reply before the message, and the head of an HTTP response.
    ///
    /// A server answers an interactive client in well under a second, and a greeting delayed
    /// against spam is a matter of seconds, so a minute of nothing is not a slow server. RFC
    /// 5321 §4.5.3.2 asks for five minutes per SMTP command, but writes it for relays between
    /// busy transfer agents.
    #[must_use]
    pub const fn reply(self) -> Duration {
        self.reply
    }

    /// How long a transfer under way may go without progress (1 min): each read of a body the
    /// server is sending (an IMAP literal, an HTTP response body), and each piece of a body
    /// the client is sending (an SMTP message, an IMAP `APPEND` literal, an HTTP request body).
    ///
    /// A bound on silence rather than on the whole transfer, because a body can run to tens
    /// of megabytes over a slow link, and it moves steadily however long it takes. A socket
    /// whose path died mid-transfer (a phone changing radio, a NAT dropping the mapping)
    /// reports nothing and holds the transfer until the operating system gives up
    /// retransmitting, which is many minutes.
    #[must_use]
    pub const fn stall(self) -> Duration {
        self.stall
    }

    /// How long a server may take to answer a submission it has been sent in full (10 min):
    /// SMTP's reply to the final `.`, and the response to a JMAP `EmailSubmission/set`, a
    /// Graph `sendMail` or a Gmail `messages.send`.
    ///
    /// Ten minutes, as RFC 5321 §4.5.3.2.6 asks of SMTP, because the server may still be
    /// filtering the message, and this is the one wait whose expiry cannot say what happened:
    /// the message may have been accepted, so the send becomes a question for the user rather
    /// than a retry. A shorter bound would ask that question of a server that was merely slow.
    #[must_use]
    pub const fn submission(self) -> Duration {
        self.submission
    }
}

impl Default for Deadlines {
    fn default() -> Self {
        Self::STANDARD
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::Deadlines;

    #[test]
    fn the_standard_bounds_are_the_documented_ones() {
        let bounds = Deadlines::default();
        assert_eq!(bounds, Deadlines::STANDARD);
        assert_eq!(bounds.dial(), Duration::from_secs(30));
        assert_eq!(bounds.reply(), Duration::from_mins(1));
        assert_eq!(bounds.stall(), Duration::from_mins(1));
        assert_eq!(bounds.submission(), Duration::from_mins(10));
    }

    #[test]
    fn a_dial_gives_up_before_a_reply_and_a_reply_before_a_submission() {
        // The HTTP funnel counts an ordinary request's reply from the send, connect included,
        // which is only sound while the connect gives up first.
        let bounds = Deadlines::STANDARD;
        assert!(bounds.dial() < bounds.reply());
        assert!(bounds.dial() < bounds.stall());
        assert!(bounds.reply() < bounds.submission());
    }
}
