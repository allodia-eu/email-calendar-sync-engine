//! Building the synthetic mailbox.
//!
//! Mail is generated **conversation by conversation**, not message by message, because
//! every cost this fixture exists to measure is a function of the thread graph: the
//! union-find derivation walks it, the thread reads index into it, and a conversation
//! whose members sit in different folders is what makes a list read cross scopes.
//!
//! One conversation is a root plus zero or more replies. The root arrives in a folder
//! drawn from the arrival weights in `spec.rs`; each reply is either the owner's
//! (filed in Sent) or another participant's (filed beside the root). A reply carries
//! `In-Reply-To` of its parent and `References` of the whole ancestor chain, so the
//! reference graph has the shape a real one does rather than a star.
//!
//! Message-ids are minted so a conversation's root owns the lexicographically smallest
//! one in its component. That is exactly the id `engine_sync::derive_mail_threads`
//! computes, so the generator stamps [`ThreadRef::derived`] up front and a fixture is
//! usable without a derivation pass — `tests/fixture.rs` holds the two to each other.

use core::{cmp::Reverse, time::Duration};
use std::collections::BTreeSet;

use engine_core::{
    ids::{MailboxId, MessageId, MessageIdHeader, ThreadId},
    mail::{EmailAddress, Keyword, Mailbox, Message, SystemKeyword, ThreadRef},
    membership::Memberships,
    time::UtcDateTime,
};

use crate::{
    rng::Rng,
    spec::{FOLDERS, FixtureSpec, FolderSpec, OLDEST, SENT, SPAN_SECONDS},
    words::{BODY_WORDS, DOMAINS, FAMILY_NAMES, GIVEN_NAMES, SUBJECT_HEADS, SUBJECT_TAILS},
};

/// The account owner's own address — the sender of everything in Sent and the
/// recipient of everything else.
const OWNER: &str = "sam.owner@example.com";

/// One generated folder: the container object and the mail filed in it.
#[derive(Debug, Clone)]
pub struct Folder {
    /// The folder's provider key / mailbox id.
    pub id: MailboxId,
    /// The container object a mailbox-list sync would return.
    pub mailbox: Mailbox,
    /// The folder's messages, newest first.
    pub messages: Vec<Message>,
}

/// A generated mailbox: every folder of one account, with its mail.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// The folders, in a fixed order: Inbox, Sent, Archive, Trash, Junk, Projects.
    pub folders: Vec<Folder>,
}

impl Fixture {
    /// Every message across every folder, newest first — the order a windowed list
    /// read must reproduce.
    #[must_use]
    pub fn newest_first(&self) -> Vec<&Message> {
        let mut all: Vec<&Message> = self
            .folders
            .iter()
            .flat_map(|folder| folder.messages.iter())
            .collect();
        all.sort_by_key(|message| Reverse(message.received_at));
        all
    }

    /// The container objects a mailbox-list sync would return.
    #[must_use]
    pub fn mailboxes(&self) -> Vec<Mailbox> {
        self.folders
            .iter()
            .map(|folder| folder.mailbox.clone())
            .collect()
    }

    /// How many messages the fixture holds, across every folder.
    #[must_use]
    pub fn len(&self) -> usize {
        self.folders
            .iter()
            .map(|folder| folder.messages.len())
            .sum()
    }

    /// Whether the fixture holds no messages at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Generates the mailbox `spec` describes.
///
/// Deterministic in `spec.seed`: the same spec yields the identical mailbox, so a
/// measurement taken against it is comparable across machines and across releases.
///
/// # Panics
///
/// Panics if `spec.messages` is zero, or if a generated identifier is rejected by its
/// newtype — which would be a bug in the generator, not in the caller's input.
#[must_use]
pub fn generate(spec: &FixtureSpec) -> Fixture {
    assert!(spec.messages > 0, "a fixture needs at least one message");
    let mut rng = Rng::new(spec.seed);
    let mut filed: Vec<Vec<Message>> = FOLDERS.iter().map(|_| Vec::new()).collect();
    let oldest: UtcDateTime = OLDEST.parse().expect("OLDEST is a valid UTC instant");
    let step = SPAN_SECONDS / as_u64(spec.messages);

    let mut written = 0usize;
    let mut conversation = 0usize;
    while written < spec.messages {
        let size = thread_size(&mut rng).min(spec.messages - written);
        emit_conversation(&mut Conversation {
            index: conversation,
            size,
            root_folder: weighted_folder(&mut rng),
            first: written,
            step,
            oldest,
            rng: &mut rng,
            filed: &mut filed,
        });
        written += size;
        conversation += 1;
    }

    Fixture {
        folders: FOLDERS
            .iter()
            .zip(filed)
            .map(|(folder, mut messages)| {
                messages.sort_by_key(|message| Reverse(message.received_at));
                Folder {
                    id: mailbox_id(folder),
                    mailbox: container(folder, &messages),
                    messages,
                }
            })
            .collect(),
    }
}

/// The arguments of one conversation, bundled so the emitter stays one call rather
/// than an eight-parameter function.
struct Conversation<'a> {
    /// The conversation's index, which seeds its ids.
    index: usize,
    /// How many messages it holds.
    size: usize,
    /// The folder its root arrived in.
    root_folder: usize,
    /// The global index of its first message, which fixes the conversation's date.
    first: usize,
    /// Seconds between two consecutive global message indices.
    step: u64,
    /// The instant global index zero carries.
    oldest: UtcDateTime,
    rng: &'a mut Rng,
    filed: &'a mut [Vec<Message>],
}

/// Emits one conversation's messages into their folders.
fn emit_conversation(args: &mut Conversation<'_>) {
    let root_id = message_id_header(args.index, 0);
    let thread = ThreadRef::derived(
        ThreadId::try_from(root_id.as_str()).expect("a message-id is a valid thread id"),
    );
    let subject = format!(
        "{} {}",
        args.rng.pick(SUBJECT_HEADS),
        args.rng.pick(SUBJECT_TAILS)
    );
    let mut ancestors: Vec<MessageIdHeader> = Vec::with_capacity(args.size);

    for seq in 0..args.size {
        // The owner answers roughly two replies in five; the rest come from other
        // participants and stay beside the root. A root is never filed in Sent.
        let folder = if seq > 0 && args.rng.chance(40) {
            SENT
        } else {
            args.root_folder
        };
        // A message is dated from its global position, so the mailbox spans
        // SPAN_SECONDS end to end and a conversation's replies follow its root closely.
        let offset = as_u64(args.first + seq) * args.step + jitter(args.rng, args.step);
        let received = args
            .oldest
            .checked_add(Duration::from_secs(offset))
            .expect("the fixture window stays inside the representable range");
        let own_id = message_id_header(args.index, seq);

        let mut message = Message::new(
            MessageId::try_from(provider_key(args.index, seq).as_str())
                .expect("a generated key is a valid message id"),
            Memberships::of_one(mailbox_id(&FOLDERS[folder])),
        );
        message.thread = Some(thread.clone());
        message.envelope.message_id = vec![own_id.clone()];
        if let Some(parent) = ancestors.last() {
            message.envelope.in_reply_to = vec![parent.clone()];
            message.envelope.references.clone_from(&ancestors);
        }
        message.envelope.subject = Some(if seq == 0 {
            subject.clone()
        } else {
            format!("Re: {subject}")
        });
        let (from, to) = correspondents(args.rng, folder == SENT);
        message.envelope.from = vec![from];
        message.envelope.to = vec![to];
        message.received_at = Some(received);
        message.sent_at = Some(received);
        message.size = Some(message_size(args.rng));
        message.preview = Some(preview(args.rng));
        message.has_attachment = args.rng.chance(12);
        message.keywords = keywords(args.rng, seq, args.size);

        ancestors.push(own_id);
        args.filed[folder].push(message);
    }
}

/// Widens a count to the 64-bit arithmetic the date offsets use.
fn as_u64(value: usize) -> u64 {
    u64::try_from(value).expect("a message count fits in 64 bits")
}

/// A sub-step offset, so two adjacent messages never share an instant exactly.
fn jitter(rng: &mut Rng, step: u64) -> u64 {
    let bound = usize::try_from(step).unwrap_or(usize::MAX);
    if bound == 0 {
        0
    } else {
        as_u64(rng.below(bound))
    }
}

/// A conversation length: mostly singletons, with a long tail of real threads.
///
/// The shares are the ones a personal mailbox shows — the overwhelming majority of
/// mail is never replied to, and the handful of long conversations are what a thread
/// read and a thread merge actually cost.
fn thread_size(rng: &mut Rng) -> usize {
    match rng.below(100) {
        0..=64 => 1,
        65..=84 => 2,
        85..=94 => 3 + rng.below(3),
        _ => 6 + rng.below(15),
    }
}

/// Picks the folder an arriving conversation lands in, by arrival weight.
fn weighted_folder(rng: &mut Rng) -> usize {
    let total: usize = FOLDERS.iter().map(|folder| folder.arrival_weight).sum();
    let mut draw = rng.below(total);
    for (index, folder) in FOLDERS.iter().enumerate() {
        if draw < folder.arrival_weight {
            return index;
        }
        draw -= folder.arrival_weight;
    }
    0
}

/// The provider key of one message: its conversation and its position within it.
fn provider_key(conversation: usize, seq: usize) -> String {
    format!("m{conversation:07}-{seq:03}")
}

/// The `Message-ID` a message owns.
///
/// Zero-padded so lexicographic order matches arrival order **within** a conversation
/// — which is what makes the root's id the smallest in its component, and therefore
/// the id derivation assigns to the thread.
fn message_id_header(conversation: usize, seq: usize) -> MessageIdHeader {
    MessageIdHeader::new(format!("t{conversation:07}-{seq:03}@example.com"))
        .expect("a generated message-id is well formed")
}

/// The mailbox id of a folder.
fn mailbox_id(folder: &FolderSpec) -> MailboxId {
    MailboxId::try_from(folder.id).expect("a folder id is a valid mailbox id")
}

/// The container object for a folder, carrying the unread count its mail implies.
fn container(folder: &FolderSpec, messages: &[Message]) -> Mailbox {
    let mut mailbox = Mailbox::new(mailbox_id(folder), folder.name);
    mailbox.role.clone_from(&folder.role);
    mailbox.unread_count = Some(
        u32::try_from(
            messages
                .iter()
                .filter(|message| message.is_unread())
                .count(),
        )
        .unwrap_or(u32::MAX),
    );
    mailbox
}

/// The sender and recipient of one message: the account owner sends everything filed
/// in Sent and receives everything else.
fn correspondents(rng: &mut Rng, outgoing: bool) -> (EmailAddress, EmailAddress) {
    let owner = EmailAddress::named("Sam Owner", OWNER);
    let other = EmailAddress::named(
        format!("{} {}", rng.pick(GIVEN_NAMES), rng.pick(FAMILY_NAMES)),
        format!(
            "{}.{}@{}",
            rng.pick(GIVEN_NAMES).to_lowercase(),
            rng.pick(FAMILY_NAMES).to_lowercase(),
            rng.pick(DOMAINS)
        ),
    );
    if outgoing {
        (owner, other)
    } else {
        (other, owner)
    }
}

/// A raw-message size: most mail is small, and attachments give it a heavy tail.
fn message_size(rng: &mut Rng) -> u64 {
    let kilobytes = match rng.below(100) {
        0..=69 => 2 + rng.below(14),
        70..=94 => 16 + rng.below(84),
        _ => 100 + rng.below(1_900),
    };
    as_u64(kilobytes) * 1024
}

/// A list-row preview: a sentence of body vocabulary, near the 256-character cap the
/// `Message::preview` contract sets.
fn preview(rng: &mut Rng) -> String {
    let mut text = String::with_capacity(200);
    while text.len() < 180 {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(rng.pick(BODY_WORDS));
    }
    text
}

/// The keywords a message carries: most mail has been read, a message with a reply
/// below it was answered, and a few are flagged.
fn keywords(rng: &mut Rng, seq: usize, size: usize) -> BTreeSet<Keyword> {
    let mut keywords = BTreeSet::new();
    if rng.chance(78) {
        keywords.insert(Keyword::system(SystemKeyword::Seen));
    }
    if rng.chance(4) {
        keywords.insert(Keyword::system(SystemKeyword::Flagged));
    }
    if seq + 1 < size {
        keywords.insert(Keyword::system(SystemKeyword::Answered));
    }
    keywords
}
