//! The addresses a calendar server treats as the signed-in user.
//!
//! A server schedules for a user only under the addresses it recognises as theirs: RFC 6638
//! §3.1 has it act on an event as the user's organiser or attendee copy only when `ORGANIZER`
//! or an `ATTENDEE` matches the principal's `calendar-user-address-set`. An invitation to an
//! address the server does not hold for this user gets no reply from the server, and no
//! `SCHEDULE-STATUS` saying so. A host that files mail arriving at one address into a calendar
//! held under another reads this before trusting the server to answer.
//!
//! Where the answer comes from is the adapter's business: the CalDAV principal's address set,
//! JMAP's `ParticipantIdentity` objects, or the mailbox address a Graph or Google calendar
//! schedules as. The last two name only the mailbox's primary address and none of its aliases,
//! so their answer is [`CalendarUserAddresses::Including`]: enough to recognise an address,
//! never to rule one out.
//!
//! # In a store that is not the credential's
//!
//! A provider bound to a shared account (`crate::shared`) answers for **that store**: its
//! calendar user is the store's owner, not the principal the credential signs in as. A meeting
//! created there is organised as the owner, so an answer naming the credential's own addresses
//! would have a host recognise the wrong person's invitations. Where the user may read the
//! store but not schedule as its owner, the honest answer names nobody.
//! [`CalendarUserAddresses::is_for_store`] states the rule once, and every adapter's live test
//! against a bound store holds its answer to it.

use engine_core::scheduling::addresses_match;

/// Which calendar user addresses the server recognises as this account's user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarUserAddresses {
    /// The server named these addresses, bare (no `mailto:`) and in its own order. Identifiers
    /// that are not email addresses (a `urn:uuid:`, an `https:` URI) are left out, so the list
    /// is empty when the server named only those.
    Known(Vec<String>),
    /// The server schedules as these addresses and may schedule as others it does not name:
    /// Exchange answers for a mailbox's proxy addresses, and Graph and Google expose only the
    /// primary one.
    Including(Vec<String>),
    /// The server described the user and named no address set. RFC 6638 §2.4.1 reads that as
    /// a principal not enabled for scheduling: the server schedules for no address at all.
    NotEnabled,
    /// No answer: the adapter does not ask, or the server could not be asked.
    Unknown,
}

impl CalendarUserAddresses {
    /// Builds the set from calendar user address URIs as a server wrote them, keeping the
    /// `mailto:` ones (any case) and dropping the rest.
    #[must_use]
    pub fn from_uris<'a>(uris: impl IntoIterator<Item = &'a str>) -> Self {
        Self::Known(uris.into_iter().filter_map(mailto_address).collect())
    }

    /// The recognised addresses, or an empty slice when there are none or none are known.
    #[must_use]
    pub fn addresses(&self) -> &[String] {
        match self {
            Self::Known(addresses) | Self::Including(addresses) => addresses,
            Self::NotEnabled | Self::Unknown => &[],
        }
    }

    /// Whether the server is known to recognise `address` as this user, compared the way
    /// iCalendar addresses are ([`addresses_match`]). `false` when the answer is
    /// [`Unknown`](Self::Unknown): not knowing is not recognition.
    #[must_use]
    pub fn recognises(&self, address: &str) -> bool {
        self.addresses()
            .iter()
            .any(|known| addresses_match(known, address))
    }

    /// Whether the server is known **not** to schedule as `address`: it named its whole set and
    /// `address` is not in it, or it schedules for nobody. `false` for
    /// [`Including`](Self::Including) and [`Unknown`](Self::Unknown), whose answer cannot rule
    /// an address out.
    #[must_use]
    pub fn excludes(&self, address: &str) -> bool {
        match self {
            Self::Known(_) => !self.recognises(address),
            Self::NotEnabled => true,
            Self::Including(_) | Self::Unknown => false,
        }
    }
}

impl CalendarUserAddresses {
    /// Whether this answer keeps the bound-store rule for a store owned by `owner` and opened
    /// with `credential`'s sign-in (see the module docs): it recognises the owner or nobody,
    /// and never `credential` unless that **is** the owner. In the user's own store the owner
    /// is the user, so the answer must recognise them, though it may name more of their
    /// addresses.
    ///
    /// An answer recognising nobody passes, since it claims nothing: a store the user may only
    /// read, an adapter that does not ask.
    #[must_use]
    pub fn is_for_store(&self, owner: &str, credential: &str) -> bool {
        if self.addresses().is_empty() {
            return true;
        }
        self.recognises(owner)
            && (addresses_match(owner, credential) || !self.recognises(credential))
    }
}

/// The bare address of a `mailto:` URI, or `None` for any other scheme or an empty address.
///
/// RFC 6068 lets the address be percent-encoded and followed by `?` header fields; a calendar
/// user address carries neither in practice, but both are undone so a server that writes them
/// still matches.
fn mailto_address(uri: &str) -> Option<String> {
    let uri = uri.trim();
    let rest = uri
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
        .map(|_| &uri[7..])?;
    let address = rest.split('?').next().unwrap_or_default();
    let decoded = percent_decode(address)?;
    (!decoded.is_empty()).then_some(decoded)
}

/// Undoes `%XX` escapes; `None` when an escape is malformed or the result is not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
#[path = "calendar_user_tests.rs"]
mod tests;
