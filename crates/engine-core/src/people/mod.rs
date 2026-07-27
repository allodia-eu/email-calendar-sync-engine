//! Pure derivation of unified people from provider contact records.

mod canonical;
mod derive;
mod model;

pub use canonical::{CanonicalEmail, CanonicalEmailError};
pub use derive::{PeopleError, rebuild_people};
pub use model::{PeopleSnapshot, Person, PersonSource, PersonSourceId, SourcedValue};
