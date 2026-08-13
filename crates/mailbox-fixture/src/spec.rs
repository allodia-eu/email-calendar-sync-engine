//! What a fixture mailbox is asked to look like, and the folders it spans.

use engine_core::{ids::AccountId, mail::MailboxRole};

/// The five-year window a fixture's mail is spread over, in seconds.
///
/// Fixed rather than scaled with size, so a larger mailbox is a *denser* one — which
/// is the shape that stresses a date-ordered read. Scaling the span instead would
/// keep every window's row count constant and hide the cost being measured.
pub(crate) const SPAN_SECONDS: u64 = 5 * 365 * 24 * 60 * 60;

/// The instant the oldest message in a fixture carries; the newest lands
/// [`SPAN_SECONDS`] later, on 2026-08-01.
///
/// A constant, not the wall clock: a benchmark whose data drifts with the calendar
/// cannot be compared against a baseline captured last month.
pub(crate) const OLDEST: &str = "2021-08-02T00:00:00Z";

/// How large a mailbox to build, and with which pseudo-random stream.
///
/// Two runs of the same spec produce the identical mailbox, so a number measured
/// against it is comparable across machines and across releases.
#[derive(Debug, Clone)]
pub struct FixtureSpec {
    /// The account the folders and messages belong to.
    pub account: AccountId,
    /// How many messages to generate, across every folder.
    pub messages: usize,
    /// The seed for the fixture's deterministic generator.
    pub seed: u64,
}

impl FixtureSpec {
    /// A spec for `messages` messages on `account`, with the default seed.
    #[must_use]
    pub fn new(account: AccountId, messages: usize) -> Self {
        Self {
            account,
            messages,
            seed: 0x5EED,
        }
    }

    /// Replaces the generator seed, for a second mailbox that is shaped the same but
    /// shares no keys with the first.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

/// One folder of the fixture mailbox: its identity and how much of the mail it holds.
#[derive(Debug, Clone)]
pub(crate) struct FolderSpec {
    /// The provider key and mailbox id.
    pub(crate) id: &'static str,
    /// The display name.
    pub(crate) name: &'static str,
    /// The normalized role, if the folder has one.
    pub(crate) role: Option<MailboxRole>,
    /// The relative weight with which a *new* conversation lands here. Replies are
    /// placed separately (see the module docs of `generate`), so these are the
    /// weights of arriving mail, not the final message shares.
    pub(crate) arrival_weight: usize,
}

/// The folders every fixture mailbox has.
///
/// Six is the shape a real account has and the shape the read path pays for: the
/// windowed read walks *every* mail scope before it sorts, so the count matters as
/// much as the total. Sent takes no arriving conversation — it fills with the
/// owner's replies — and `Projects` is a custom folder with no role, so a fixture
/// exercises both the role-driven and the plain paths.
pub(crate) const FOLDERS: &[FolderSpec] = &[
    FolderSpec {
        id: "INBOX",
        name: "Inbox",
        role: Some(MailboxRole::Inbox),
        arrival_weight: 60,
    },
    FolderSpec {
        id: "Sent",
        name: "Sent",
        role: Some(MailboxRole::Sent),
        arrival_weight: 0,
    },
    FolderSpec {
        id: "Archive",
        name: "Archive",
        role: Some(MailboxRole::Archive),
        arrival_weight: 26,
    },
    FolderSpec {
        id: "Trash",
        name: "Trash",
        role: Some(MailboxRole::Trash),
        arrival_weight: 6,
    },
    FolderSpec {
        id: "Junk",
        name: "Junk",
        role: Some(MailboxRole::Junk),
        arrival_weight: 5,
    },
    FolderSpec {
        id: "Projects",
        name: "Projects",
        role: None,
        arrival_weight: 3,
    },
];

/// The index of the Sent folder in [`FOLDERS`] — where a reply the owner wrote is filed.
pub(crate) const SENT: usize = 1;

#[cfg(test)]
mod tests {
    use engine_core::ids::AccountId;

    use super::{FOLDERS, FixtureSpec, SENT};

    #[test]
    fn sent_is_the_only_folder_no_conversation_arrives_in() {
        // The index constant and the weight table have to agree, or replies would be
        // filed into a folder that also receives arriving mail and the fixture's
        // cross-folder threads would stop being cross-folder.
        assert_eq!(FOLDERS[SENT].id, "Sent");
        for (index, folder) in FOLDERS.iter().enumerate() {
            assert_eq!(
                folder.arrival_weight == 0,
                index == SENT,
                "{} has the wrong arrival weight",
                folder.id
            );
        }
    }

    #[test]
    fn folder_ids_are_distinct() {
        let mut ids: Vec<&str> = FOLDERS.iter().map(|folder| folder.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "two folders share an id");
    }

    #[test]
    fn a_spec_carries_its_size_and_takes_a_seed() {
        let account = AccountId::try_from("acct-1").unwrap();
        let spec = FixtureSpec::new(account.clone(), 1_000);
        assert_eq!(spec.messages, 1_000);
        assert_eq!(spec.with_seed(7).seed, 7);
    }
}
