//! The vCard versions a card is written in and an address book takes.

use core::fmt;

/// A vCard version: the `VERSION` a card states, and what an address book says it takes (CardDAV
/// `supported-address-data`, RFC 6352 §6.2.2).
///
/// An import carries a card in the version its source wrote, and a target takes the versions it
/// names, so both sides of the question "will this card go in?" are this one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum VcardVersion {
    /// vCard 2.1, the version before the IETF's, still written by older phones.
    V2_1,
    /// vCard 3.0 (RFC 2426), the one every CardDAV server must take (RFC 6352 §6.2.2).
    V3,
    /// vCard 4.0 (RFC 6350).
    V4,
}

impl VcardVersion {
    /// The version a `VERSION` value names (`2.1`, `3.0`, `4.0`), or `None` for any other.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim() {
            "2.1" => Some(Self::V2_1),
            "3.0" => Some(Self::V3),
            "4.0" => Some(Self::V4),
            _ => None,
        }
    }

    /// The `VERSION` value of this version.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V2_1 => "2.1",
            Self::V3 => "3.0",
            Self::V4 => "4.0",
        }
    }
}

impl fmt::Display for VcardVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::VcardVersion;

    #[test]
    fn a_version_reads_and_writes_as_its_token() {
        for version in [VcardVersion::V2_1, VcardVersion::V3, VcardVersion::V4] {
            assert_eq!(VcardVersion::from_token(version.as_str()), Some(version));
            assert_eq!(version.to_string(), version.as_str());
        }
        assert_eq!(VcardVersion::from_token(" 4.0 "), Some(VcardVersion::V4));
        assert_eq!(VcardVersion::from_token("5.0"), None);
        assert!(VcardVersion::V3 < VcardVersion::V4);
    }
}
