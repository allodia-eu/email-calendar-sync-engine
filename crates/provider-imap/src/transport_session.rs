//! What one authenticated session agreed to, and what that means for the commands it
//! sends.
//!
//! Split from [`crate::transport`] (which owns the line protocol) because this is the
//! other half of a connection: the dialect and extension set the `CAPABILITY`/`ENABLE`
//! handshake settled on, and the one place that reaches the wire — how a mailbox name is
//! encoded. Needs no stream bounds, so the unbounded provider builder can consult it.

use crate::{capability::Extension, transport::Connection, transport_command::quote};

impl<S> Connection<S> {
    /// Whether this session may keep a standing `IDLE` (RFC 2177) — the precondition a
    /// [`crate::watch::ImapWatcher`] checks before opening one, and what
    /// [`ImapProvider::build`](crate::provider) reads to advertise
    /// [`Capabilities::idle`](engine_provider::Capabilities::idle). A plain field read, so
    /// it needs no stream bounds (the unbounded provider builder consults it).
    pub(crate) fn idle_available(&self) -> bool {
        self.negotiated.has(Extension::Idle)
    }

    /// Whether this session negotiated QRESYNC (RFC 7162), so a delta can reconcile flag
    /// changes and expunges with `CHANGEDSINCE`/`VANISHED` instead of re-snapshotting.
    pub(crate) fn qresync_enabled(&self) -> bool {
        self.negotiated.has(Extension::Qresync)
    }

    /// The dialect and usable extensions this session negotiated, for the connect trace.
    pub(crate) fn negotiated_summary(&self) -> (&'static str, Vec<&'static str>) {
        (
            self.negotiated.dialect(),
            self.negotiated.available_extensions(),
        )
    }

    /// Whether this session's wire encodes mailbox names as modified UTF-7 — what
    /// [`crate::mail::mailbox_from_list`] needs in order to read a `LIST` row.
    pub(crate) fn names_are_modified_utf7(&self) -> bool {
        self.negotiated.names_are_modified_utf7()
    }

    /// One mailbox name as this session's wire wants it: modified UTF-7 on IMAP4rev1
    /// (RFC 3501 §5.1.3), UTF-8 as-is on IMAP4rev2 (RFC 9051 §5.1).
    ///
    /// Every command that names a mailbox goes through here, so the encoding lives at the
    /// transport boundary and a [`MailboxId`](engine_core::ids::MailboxId) can be the
    /// decoded name on both dialects.
    fn wire_name(&self, mailbox: &str) -> String {
        if self.names_are_modified_utf7() {
            crate::utf7::encode(mailbox)
        } else {
            mailbox.to_owned()
        }
    }

    /// [`wire_name`](Self::wire_name), quoted for inclusion in a command.
    pub(crate) fn quoted_name(&self, mailbox: &str) -> String {
        quote(&self.wire_name(mailbox))
    }
}
