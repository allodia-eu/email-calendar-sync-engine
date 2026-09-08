//! Provider-neutral organiser and invitee intent.

use core::fmt;

use engine_core::{
    people::{CanonicalEmail, CanonicalEmailError},
    scheduling::addresses_match,
};
use serde::{Deserialize, Serialize};

/// Why meeting invitees cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvitationError {
    /// A meeting needs at least one invitee.
    Empty,
    /// The organiser also appears as an invitee.
    OrganiserInvited,
    /// An address appears more than once.
    Duplicate(CalendarAddress),
    /// The provider's participant limit would be exceeded.
    TooMany {
        /// The provider's maximum.
        limit: usize,
        /// The number supplied.
        actual: usize,
    },
}

impl fmt::Display for InvitationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a meeting needs at least one invitee"),
            Self::OrganiserInvited => f.write_str("the organiser cannot also be an invitee"),
            Self::Duplicate(address) => write!(f, "invitee {address} appears more than once"),
            Self::TooMany { limit, actual } => {
                write!(f, "provider accepts at most {limit} invitees, got {actual}")
            }
        }
    }
}

impl std::error::Error for InvitationError {}

/// A validated email calendar address.
///
/// The first invitation surface supports email recipients only. Protocol adapters add
/// `mailto:` where their wire format requires it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CalendarAddress(CanonicalEmail);

impl CalendarAddress {
    /// Parses and canonicalises an email calendar address.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalEmailError`] when the value has no valid local part and domain.
    pub fn parse(value: &str) -> Result<Self, CanonicalEmailError> {
        CanonicalEmail::parse(value).map(Self)
    }

    /// Returns the canonical email address without a URI scheme.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn matches(&self, other: &Self) -> bool {
        addresses_match(self.as_str(), other.as_str())
    }
}

impl fmt::Display for CalendarAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for CalendarAddress {
    type Error = CanonicalEmailError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for CalendarAddress {
    type Error = CanonicalEmailError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// The account identity that organises a meeting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchedulingIdentity {
    address: CalendarAddress,
    name: Option<String>,
}

impl SchedulingIdentity {
    /// Creates an identity without a display name.
    #[must_use]
    pub fn new(address: CalendarAddress) -> Self {
        Self {
            address,
            name: None,
        }
    }

    /// Creates an identity with a display name.
    #[must_use]
    pub fn named(address: CalendarAddress, name: impl Into<String>) -> Self {
        Self {
            address,
            name: Some(name.into()),
        }
    }

    /// Returns the email calendar address.
    #[must_use]
    pub fn address(&self) -> &CalendarAddress {
        &self.address
    }

    /// Returns the display name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// An invitee's scheduling role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InviteeRole {
    /// Attendance is requested.
    Required,
    /// Attendance is optional.
    Optional,
}

/// One person invited to a meeting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Invitee {
    identity: SchedulingIdentity,
    role: InviteeRole,
}

impl Invitee {
    /// Creates a required invitee.
    #[must_use]
    pub fn required(identity: SchedulingIdentity) -> Self {
        Self {
            identity,
            role: InviteeRole::Required,
        }
    }

    /// Creates an optional invitee.
    #[must_use]
    pub fn optional(identity: SchedulingIdentity) -> Self {
        Self {
            identity,
            role: InviteeRole::Optional,
        }
    }

    /// Returns the invitee's identity.
    #[must_use]
    pub fn identity(&self) -> &SchedulingIdentity {
        &self.identity
    }

    /// Returns the invitee's role.
    #[must_use]
    pub fn role(&self) -> InviteeRole {
        self.role
    }
}

/// Scheduling data for a new meeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingDraft {
    organiser: SchedulingIdentity,
    invitees: Vec<Invitee>,
}

impl MeetingDraft {
    /// Creates a meeting organised by `organiser` for `invitees`.
    #[must_use]
    pub fn new(organiser: SchedulingIdentity, invitees: Vec<Invitee>) -> Self {
        Self {
            organiser,
            invitees,
        }
    }

    /// Returns the organiser identity selected by the caller.
    #[must_use]
    pub fn organiser(&self) -> &SchedulingIdentity {
        &self.organiser
    }

    /// Returns the invitees in caller order.
    #[must_use]
    pub fn invitees(&self) -> &[Invitee] {
        &self.invitees
    }

    /// Checks address uniqueness, organiser separation and a provider limit.
    ///
    /// # Errors
    ///
    /// Returns [`InvitationError`] when the meeting cannot be represented safely.
    pub fn validate(&self, limit: Option<usize>) -> Result<(), InvitationError> {
        if self.invitees.is_empty() {
            return Err(InvitationError::Empty);
        }
        if let Some(limit) = limit
            && self.invitees.len() > limit
        {
            return Err(InvitationError::TooMany {
                limit,
                actual: self.invitees.len(),
            });
        }
        let mut addresses: Vec<&CalendarAddress> = Vec::with_capacity(self.invitees.len());
        for invitee in &self.invitees {
            let address = invitee.identity().address();
            if address.matches(self.organiser.address()) {
                return Err(InvitationError::OrganiserInvited);
            }
            if addresses.iter().any(|known| known.matches(address)) {
                return Err(InvitationError::Duplicate(address.clone()));
            }
            addresses.push(address);
        }
        Ok(())
    }
}

/// Invitee changes on an existing meeting.
///
/// `upsert` adds a new invitee or changes the role or display name of an existing one.
/// `remove` cancels an invitation for the named address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteePatch {
    upsert: Vec<Invitee>,
    remove: Vec<CalendarAddress>,
}

impl InviteePatch {
    /// An empty invitee patch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces an invitee by address.
    #[must_use]
    pub fn upsert(mut self, invitee: Invitee) -> Self {
        let address = invitee.identity().address();
        self.remove.retain(|removed| !removed.matches(address));
        if let Some(existing) = self
            .upsert
            .iter_mut()
            .find(|existing| existing.identity().address().matches(address))
        {
            *existing = invitee;
        } else {
            self.upsert.push(invitee);
        }
        self
    }

    /// Removes an invitee by address.
    #[must_use]
    pub fn remove(mut self, address: CalendarAddress) -> Self {
        self.upsert
            .retain(|invitee| !invitee.identity().address().matches(&address));
        if !self.remove.iter().any(|removed| removed.matches(&address)) {
            self.remove.push(address);
        }
        self
    }

    /// Returns invitees to add or replace.
    #[must_use]
    pub fn upserts(&self) -> &[Invitee] {
        &self.upsert
    }

    /// Returns addresses to remove.
    #[must_use]
    pub fn removals(&self) -> &[CalendarAddress] {
        &self.remove
    }

    /// Returns whether the patch changes no invitees.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upsert.is_empty() && self.remove.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(value: &str) -> CalendarAddress {
        CalendarAddress::parse(value).unwrap()
    }

    #[test]
    fn calendar_addresses_are_validated_and_canonicalised() {
        assert_eq!(
            address("Alice@BÜCHER.example").as_str(),
            "Alice@xn--bcher-kva.example"
        );
        assert!(CalendarAddress::parse("not-an-address").is_err());
    }

    #[test]
    fn meeting_intent_roundtrips_through_json() {
        let meeting = MeetingDraft::new(
            SchedulingIdentity::named(address("owner@example.com"), "Owner"),
            vec![Invitee::optional(SchedulingIdentity::new(address(
                "guest@example.com",
            )))],
        );
        let json = serde_json::to_string(&meeting).unwrap();
        assert_eq!(
            serde_json::from_str::<MeetingDraft>(&json).unwrap(),
            meeting
        );
        assert!(meeting.validate(None).is_ok());
    }

    #[test]
    fn a_meeting_rejects_duplicates_the_organiser_and_provider_overflow() {
        let organiser = SchedulingIdentity::new(address("owner@example.com"));
        assert_eq!(
            MeetingDraft::new(organiser.clone(), Vec::new()).validate(None),
            Err(InvitationError::Empty)
        );
        assert_eq!(
            MeetingDraft::new(
                organiser.clone(),
                vec![Invitee::required(organiser.clone())]
            )
            .validate(None),
            Err(InvitationError::OrganiserInvited)
        );
        let guest = Invitee::required(SchedulingIdentity::new(address("guest@example.com")));
        assert!(matches!(
            MeetingDraft::new(organiser, vec![guest.clone(), guest]).validate(Some(1)),
            Err(InvitationError::TooMany { .. })
        ));
    }

    #[test]
    fn scheduling_address_uniqueness_is_case_insensitive() {
        let organiser = SchedulingIdentity::new(address("Owner@example.com"));
        assert_eq!(
            MeetingDraft::new(
                organiser.clone(),
                vec![Invitee::required(SchedulingIdentity::new(address(
                    "owner@example.com",
                )))]
            )
            .validate(None),
            Err(InvitationError::OrganiserInvited)
        );
        assert!(matches!(
            MeetingDraft::new(
                organiser,
                vec![
                    Invitee::required(SchedulingIdentity::new(address("Guest@example.com"))),
                    Invitee::optional(SchedulingIdentity::new(address("guest@example.com"))),
                ]
            )
            .validate(None),
            Err(InvitationError::Duplicate(_))
        ));
    }

    #[test]
    fn invitee_patch_distinguishes_upserts_and_removals() {
        let patch = InviteePatch::new()
            .upsert(Invitee::required(SchedulingIdentity::new(address(
                "new@example.com",
            ))))
            .remove(address("old@example.com"));
        assert_eq!(patch.upserts().len(), 1);
        assert_eq!(patch.removals(), &[address("old@example.com")]);
        assert!(!patch.is_empty());
    }

    #[test]
    fn invitee_patch_resolves_case_variants_as_one_address() {
        let patch = InviteePatch::new()
            .upsert(Invitee::required(SchedulingIdentity::new(address(
                "Guest@example.com",
            ))))
            .upsert(Invitee::optional(SchedulingIdentity::new(address(
                "guest@example.com",
            ))));
        assert_eq!(patch.upserts().len(), 1);
        assert_eq!(patch.upserts()[0].role(), InviteeRole::Optional);

        let patch = patch.remove(address("GUEST@example.com"));
        assert!(patch.upserts().is_empty());
        assert_eq!(patch.removals().len(), 1);
    }
}
