//! The names a keyword goes by on a provider that has no free-form keywords.
//!
//! IMAP and JMAP store any keyword a host names. Gmail and Microsoft Graph do not: what they
//! offer instead is a **named** object the person sees (a Gmail label, an Outlook category).
//! A host that wants a keyword kept there registers a [`KeywordName`] with the adapter: the name
//! to create it under, and every name it may already exist under. The names are the host's to
//! choose, and usually its translations of one label, because the person reads it.
//!
//! Every name is recognised whichever one this device would create, so a device running in one
//! language reads a label another device created in another, and reuses it rather than creating
//! a second.

use engine_core::mail::Keyword;

/// A keyword, and the names it is stored under where the provider keeps it as a named label or
/// category rather than as a keyword.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordName {
    keyword: Keyword,
    create_as: String,
    known_as: Vec<String>,
}

impl KeywordName {
    /// `keyword`, created under `create_as` where it does not exist yet. `None` when the name
    /// is blank, which no provider accepts.
    #[must_use]
    pub fn new(keyword: Keyword, create_as: impl Into<String>) -> Option<Self> {
        let create_as = create_as.into().trim().to_owned();
        (!create_as.is_empty()).then(|| Self {
            keyword,
            create_as,
            known_as: Vec::new(),
        })
    }

    /// Adds another name the keyword may already be stored under. A blank name is ignored.
    #[must_use]
    pub fn also_known_as(mut self, name: impl Into<String>) -> Self {
        let name = name.into().trim().to_owned();
        if !name.is_empty() && !self.is_named(&name) {
            self.known_as.push(name);
        }
        self
    }

    /// The keyword the named label or category stands for.
    #[must_use]
    pub fn keyword(&self) -> &Keyword {
        &self.keyword
    }

    /// The name a label or category is created under when none of [`Self::names`] exists.
    #[must_use]
    pub fn create_as(&self) -> &str {
        &self.create_as
    }

    /// Every name the keyword is recognised under, [`Self::create_as`] first.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.create_as.as_str()).chain(self.known_as.iter().map(String::as_str))
    }

    /// Whether `name` is one of [`Self::names`], ignoring case: Gmail refuses two labels that
    /// differ only in case, and Outlook matches categories the same way.
    #[must_use]
    pub fn is_named(&self, name: &str) -> bool {
        let name = name.trim();
        self.names()
            .any(|known| known.to_lowercase() == name.to_lowercase())
    }
}

/// The keyword `name` stands for among `names`, if any.
#[must_use]
pub fn keyword_named<'a>(names: &'a [KeywordName], name: &str) -> Option<&'a Keyword> {
    names
        .iter()
        .find(|named| named.is_named(name))
        .map(KeywordName::keyword)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named() -> KeywordName {
        KeywordName::new(Keyword::new("project-x").unwrap(), "Project X")
            .unwrap()
            .also_known_as("Projekt X")
    }

    #[test]
    fn a_blank_name_is_refused_and_a_blank_alias_ignored() {
        assert!(KeywordName::new(Keyword::new("project-x").unwrap(), "  ").is_none());
        assert_eq!(named().also_known_as(" ").names().count(), 2);
    }

    #[test]
    fn every_name_is_recognised_ignoring_case_and_the_created_one_comes_first() {
        let named = named();
        assert_eq!(
            named.names().collect::<Vec<_>>(),
            ["Project X", "Projekt X"]
        );
        assert!(named.is_named("project x"));
        assert!(named.is_named(" PROJEKT X "));
        assert!(!named.is_named("Project"));
    }

    #[test]
    fn a_repeated_alias_is_kept_once() {
        assert_eq!(named().also_known_as("project x").names().count(), 2);
    }

    #[test]
    fn a_name_resolves_to_its_keyword() {
        let names = [named()];
        assert_eq!(
            keyword_named(&names, "projekt x").map(Keyword::as_str),
            Some("project-x")
        );
        assert!(keyword_named(&names, "Other").is_none());
    }
}
