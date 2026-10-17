//! How long any provider waits on a server that has stopped answering.
//!
//! A server that accepted the connection and then says nothing (a paused process behind a port
//! forwarder, a captive portal, a stalled middlebox, a NAT mapping that died) reports nothing
//! either, and a wait on it lasts forever. Every transport the engine speaks bounds every such
//! wait by one of the six categories here, so IMAP, SMTP, JMAP, Graph, Google and DAV give up
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
    greeting: Duration,
    setup: Duration,
    reply: Duration,
    stall: Duration,
    submission: Duration,
}

impl Deadlines {
    /// The bounds every transport uses.
    pub const STANDARD: Self = Self {
        dial: Duration::from_secs(15),
        greeting: Duration::from_secs(15),
        setup: Duration::from_secs(30),
        reply: Duration::from_mins(1),
        stall: Duration::from_mins(1),
        submission: Duration::from_mins(10),
    };

    /// How long a TCP connect, and then the TLS handshake over it, may each take (15 s). The
    /// name lookup is part of the connect.
    ///
    /// Both are a few round trips to a server that is up, which a congested mobile link
    /// completes in seconds, and fifteen seconds still covers the first three retransmissions
    /// of a lost connection request. Unbounded, a connect waits out the operating system's own
    /// retries (over two minutes on Linux) and a handshake waits forever. A send waits on this
    /// before it can be reported as failed and retried, so it is no longer than a server that
    /// is up needs.
    #[must_use]
    pub const fn dial(self) -> Duration {
        self.dial
    }

    /// How long a server may take to send its first line on a connection it has just accepted
    /// (15 s): an IMAP or an SMTP greeting.
    ///
    /// A server owes the greeting before the client has asked anything, so there is no work
    /// it could be doing on the client's behalf. A greeting delayed against spam is a matter of
    /// seconds, and is a practice of the port that receives mail from other servers rather
    /// than of a submission port. Fifteen seconds of nothing at this point is a paused process
    /// or a middlebox, not a slow server.
    #[must_use]
    pub const fn greeting(self) -> Duration {
        self.greeting
    }

    /// How long a submission server may take over each reply it owes before the message (30 s):
    /// SMTP's replies to `EHLO`, `STARTTLS`, `AUTH`, `MAIL`, `RCPT` and `DATA`, after the
    /// greeting.
    ///
    /// Each is a short exchange a server answers in well under a second; some cost the server
    /// a lookup (a credential checked against a directory or an identity provider, a recipient
    /// against an address book), which is why this is twice the greeting's bound. It is
    /// shorter than [`reply`](Self::reply) because a sender is waiting on it, and safe to be,
    /// because nothing has been handed over at any of these points: a send that gives up here
    /// is retried, never left in doubt. RFC 5321 §4.5.3.2 asks for five minutes per command,
    /// but writes it for relays between busy transfer agents.
    #[must_use]
    pub const fn setup(self) -> Duration {
        self.setup
    }

    /// How long a server may stay silent when it owes an answer to a request it has been sent
    /// in full (1 min): every line of an IMAP command's response, and the head of an HTTP
    /// response. A greeting and an SMTP reply before the message have their own, shorter
    /// bounds ([`greeting`](Self::greeting), [`setup`](Self::setup)).
    ///
    /// A server answers most requests in well under a second, but a server that is working
    /// can be silent for a while before a read's answer: an IMAP `SEARCH` over a large mailbox,
    /// a calendar `REPORT` over years of events. A minute of nothing is no longer a server at
    /// work.
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
        assert_eq!(bounds.dial(), Duration::from_secs(15));
        assert_eq!(bounds.greeting(), Duration::from_secs(15));
        assert_eq!(bounds.setup(), Duration::from_secs(30));
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

    #[test]
    fn a_send_hears_of_a_silent_server_sooner_than_a_read_does() {
        let bounds = Deadlines::STANDARD;
        assert!(bounds.greeting() <= bounds.setup());
        assert!(bounds.setup() < bounds.reply());
    }
}
