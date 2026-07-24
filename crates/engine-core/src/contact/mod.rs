//! Provider-neutral contacts normalized on the JSContact data model.
//!
//! Provider records remain account-scoped and lossless. Cross-account
//! coalescing lives in [`crate::people`], never in these source objects.

mod address_book;
mod model;
mod property;
mod write;

pub use address_book::AddressBook;
pub use model::{
    Anniversary, ContactAddress, ContactCard, ContactEmail, ContactKind, ContactLanguage,
    ContactMember, ContactName, ContactNickname, ContactNote, ContactOnlineService, ContactPhone,
    ContactRelation, ContactResource, ContactSourceClass, NameComponent, NameComponentKind,
    Organization, OrganizationUnit, PersonalInfo, Title,
};
pub use property::{ContactProperty, PropertyId};
pub use write::{ContactDraft, ContactField, ContactFieldSet, ContactPatch, FieldPatch};
