//! The contacts half of [`Capabilities`]: what an adapter promises about an
//! address book, beside reading one.
//!
//! Beside the capability set rather than inside it so that file carries the mail and
//! calendar domains alone, the way [`capability_calendar`](crate::capability_calendar)
//! already carries the calendar-write promises.

use crate::{Capabilities, WriteGuard};

impl Capabilities {
    /// Marks address-book/contact read and sync as supported.
    #[must_use]
    pub const fn with_contacts(mut self) -> Self {
        self.contacts = true;
        self
    }

    /// Marks source-targeted contact writes and their guard strength.
    #[must_use]
    pub const fn with_contact_writes(mut self, guard: WriteGuard) -> Self {
        self.contact_writes = Some(guard);
        self
    }

    /// Marks contact-group reads as supported.
    #[must_use]
    pub const fn with_contact_groups(mut self) -> Self {
        self.contact_groups = true;
        self
    }

    /// Marks authenticated, on-demand contact-photo fetch as supported.
    #[must_use]
    pub const fn with_contact_photos(mut self) -> Self {
        self.contact_photos = true;
        self
    }

    /// Whether address-book/contact read and sync is supported.
    #[must_use]
    pub const fn contacts(self) -> bool {
        self.contacts
    }

    /// Whether contact writes are supported.
    #[must_use]
    pub const fn contact_writes(self) -> bool {
        self.contact_writes.is_some()
    }

    /// Contact-write lost-update guard strength.
    #[must_use]
    pub const fn contact_write_guard(self) -> Option<WriteGuard> {
        self.contact_writes
    }

    /// Whether contact-group reads are supported.
    #[must_use]
    pub const fn contact_groups(self) -> bool {
        self.contact_groups
    }

    /// Whether authenticated contact-photo fetch is supported.
    #[must_use]
    pub const fn contact_photos(self) -> bool {
        self.contact_photos
    }
}
