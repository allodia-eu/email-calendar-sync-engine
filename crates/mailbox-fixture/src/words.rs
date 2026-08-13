//! The fixture's vocabulary.
//!
//! Fixed word lists rather than generated gibberish, because two of the costs under
//! measurement are text-shaped: the FTS5 index the apply path writes, and the JSON
//! payload every list read deserializes. Random bytes would compress and tokenize
//! nothing like mail does. Every domain here is reserved by RFC 2606.

/// Given names for the synthetic correspondents.
pub(crate) const GIVEN_NAMES: &[&str] = &[
    "Anna", "Bram", "Chiara", "Diego", "Elin", "Felix", "Greta", "Hugo", "Ingrid", "Jonas",
    "Katrin", "Lucas", "Marta", "Noor", "Olaf", "Petra", "Quinn", "Rafael", "Sanne", "Tomas",
    "Ulrike", "Viktor", "Wanda", "Yusuf", "Zoe",
];

/// Family names for the synthetic correspondents.
pub(crate) const FAMILY_NAMES: &[&str] = &[
    "Andersen", "Bakker", "Costa", "Duarte", "Eriksson", "Fischer", "Garcia", "Hoffmann",
    "Iversen", "Jansen", "Kowalski", "Laurent", "Moreau", "Novak", "Olsen", "Petrov", "Rossi",
    "Schmidt", "Tanaka", "Vargas",
];

/// The reserved domains correspondents are drawn from (RFC 2606 §3).
pub(crate) const DOMAINS: &[&str] = &["example.com", "example.net", "example.org"];

/// Subject openers — the first half of a generated subject line.
pub(crate) const SUBJECT_HEADS: &[&str] = &[
    "Quarterly review",
    "Invoice",
    "Deployment window",
    "Onboarding checklist",
    "Team offsite",
    "Contract renewal",
    "Incident report",
    "Design feedback",
    "Budget forecast",
    "Shipping notice",
    "Access request",
    "Roadmap update",
    "Interview schedule",
    "Server migration",
    "Weekly digest",
    "Password rotation",
    "Support ticket",
    "Travel itinerary",
];

/// Subject tails — the qualifier appended to a [`SUBJECT_HEADS`] entry.
pub(crate) const SUBJECT_TAILS: &[&str] = &[
    "for the Amsterdam office",
    "before Friday",
    "— action needed",
    "(draft 3)",
    "next quarter",
    "for the platform team",
    "and follow-up notes",
    "this week",
    "— please confirm",
    "with the revised numbers",
];

/// The body vocabulary previews and reply text are assembled from.
pub(crate) const BODY_WORDS: &[&str] = &[
    "about",
    "agenda",
    "already",
    "attached",
    "available",
    "before",
    "between",
    "confirm",
    "deadline",
    "delivery",
    "discussed",
    "document",
    "everyone",
    "feedback",
    "following",
    "forward",
    "further",
    "meeting",
    "morning",
    "planning",
    "possible",
    "proposal",
    "question",
    "regarding",
    "release",
    "reminder",
    "requested",
    "response",
    "schedule",
    "shortly",
    "summary",
    "supplier",
    "thanks",
    "timeline",
    "tomorrow",
    "updated",
    "version",
    "yesterday",
];

#[cfg(test)]
mod tests {
    use super::{BODY_WORDS, DOMAINS, FAMILY_NAMES, GIVEN_NAMES, SUBJECT_HEADS, SUBJECT_TAILS};

    #[test]
    fn every_domain_is_reserved_for_documentation() {
        // The AGENTS.md rule, asserted where the vocabulary lives rather than only in the
        // repo-wide script: a domain added here is one nobody re-checks.
        for domain in DOMAINS {
            assert!(
                matches!(*domain, "example.com" | "example.net" | "example.org"),
                "{domain} is not an RFC 2606 reserved name"
            );
        }
    }

    #[test]
    fn the_lists_are_non_empty_so_picking_cannot_panic() {
        for (name, len) in [
            ("given names", GIVEN_NAMES.len()),
            ("family names", FAMILY_NAMES.len()),
            ("domains", DOMAINS.len()),
            ("subject heads", SUBJECT_HEADS.len()),
            ("subject tails", SUBJECT_TAILS.len()),
            ("body words", BODY_WORDS.len()),
        ] {
            assert!(len > 0, "{name} is empty");
        }
    }
}
