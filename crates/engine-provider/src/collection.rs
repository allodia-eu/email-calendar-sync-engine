//! Creating a calendar or an address book: the one verb per domain that makes a new container,
//! for any host.
//!
//! A transfer host creates a calendar on the target before it imports the source calendar's
//! events into it, and a sync host creates one when its user does. Both are the same act, so it
//! is one verb outside the `import` feature, reached like every new capability through an
//! accessor on [`Provider`](crate::Provider) that defaults to `None`.
//!
//! What a host asks for is what a person sees: a name, a description, a colour. Where the
//! calendar lives (a CalDAV collection's path, a JMAP or Graph id) is the target's to choose, so
//! the answer is the [`Calendar`] as the target now holds it, read back after the create. That
//! read-back is the receipt: a field the target did not keep is missing from it.

use async_trait::async_trait;
use engine_core::{calendar::Calendar, contact::AddressBook, ids::AccountId};

use crate::ProviderResult;

/// A calendar to create.
///
/// Built with [`new`](Self::new) and the `with_*` methods, so a part added later does not break a
/// caller. A time zone is not yet among them: a CalDAV calendar's is a `VTIMEZONE` definition,
/// which the engine builds from tzdb only once its iCalendar serializer does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewCalendar {
    /// The display name.
    pub name: String,
    /// A free-text description.
    pub description: Option<String>,
    /// A display colour, as `Calendar::color` holds one (`#RRGGBB`, `#RRGGBBAA`, or a CSS name).
    pub color: Option<String>,
}

impl NewCalendar {
    /// A calendar named `name`, with no description or colour.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            color: None,
        }
    }

    /// Describes the calendar (builder-style).
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Colours the calendar (builder-style).
    #[must_use]
    pub fn with_color(mut self, color: impl Into<String>) -> Self {
        self.color = Some(color.into());
        self
    }
}

/// Creates calendars.
#[async_trait]
pub trait CalendarCollections: Send + Sync {
    /// Creates a new calendar and answers it as the target now holds it.
    ///
    /// Always a new calendar: one already named `new.name` is not reused, since a name is not an
    /// identity. A create whose answer was lost is
    /// [`requires_confirmation`](crate::ProviderError::requires_confirmation): the calendar may
    /// exist, so look for it in the calendar list before creating it again.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`](crate::ProviderError): the target refused the
    /// calendar (a permission, a quota), or could not be asked.
    async fn create_calendar(
        &self,
        account: &AccountId,
        new: &NewCalendar,
    ) -> ProviderResult<Calendar>;
}

/// A boxed creator creates calendars, as a boxed importer imports.
#[async_trait]
impl<T: CalendarCollections + ?Sized> CalendarCollections for Box<T> {
    async fn create_calendar(
        &self,
        account: &AccountId,
        new: &NewCalendar,
    ) -> ProviderResult<Calendar> {
        (**self).create_calendar(account, new).await
    }
}

/// A shared creator creates calendars, as for [`Box`].
#[async_trait]
impl<T: CalendarCollections + ?Sized> CalendarCollections for std::sync::Arc<T> {
    async fn create_calendar(
        &self,
        account: &AccountId,
        new: &NewCalendar,
    ) -> ProviderResult<Calendar> {
        (**self).create_calendar(account, new).await
    }
}

/// An address book to create.
///
/// Built with [`new`](Self::new) and [`with_description`](Self::with_description), so a part
/// added later does not break a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewAddressBook {
    /// The display name.
    pub name: String,
    /// A free-text description.
    pub description: Option<String>,
}

impl NewAddressBook {
    /// An address book named `name`, with no description.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
        }
    }

    /// Describes the address book (builder-style).
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Creates address books, as [`CalendarCollections`] creates calendars.
#[async_trait]
pub trait AddressBookCollections: Send + Sync {
    /// Creates a new address book and answers it as the target now holds it: always a new one,
    /// and a create whose answer was lost is
    /// [`requires_confirmation`](crate::ProviderError::requires_confirmation).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`](crate::ProviderError): the target refused the
    /// address book, or could not be asked.
    async fn create_address_book(
        &self,
        account: &AccountId,
        new: &NewAddressBook,
    ) -> ProviderResult<AddressBook>;
}

/// A boxed creator creates address books.
#[async_trait]
impl<T: AddressBookCollections + ?Sized> AddressBookCollections for Box<T> {
    async fn create_address_book(
        &self,
        account: &AccountId,
        new: &NewAddressBook,
    ) -> ProviderResult<AddressBook> {
        (**self).create_address_book(account, new).await
    }
}

/// A shared creator creates address books.
#[async_trait]
impl<T: AddressBookCollections + ?Sized> AddressBookCollections for std::sync::Arc<T> {
    async fn create_address_book(
        &self,
        account: &AccountId,
        new: &NewAddressBook,
    ) -> ProviderResult<AddressBook> {
        (**self).create_address_book(account, new).await
    }
}
