//! The fixture, end to end: what the generator claims about the mailbox it produces,
//! and what an [`Engine`] holding it actually answers.
//!
//! Small sizes on purpose. These assert the *shape* the benchmarks then measure at
//! scale — a property that holds at 400 messages holds at 400,000, and a suite that
//! built 400,000 to prove it would be a suite nobody runs.

use std::collections::{BTreeSet, HashMap, HashSet};

use engine_api::{AccountId, Engine, Keyword, Message, SystemKeyword};
use mailbox_fixture::{FixtureSpec, Pass, generate, populate, sync_folder};

fn account() -> AccountId {
    AccountId::try_from("fixture-account").expect("a valid account id")
}

fn spec(messages: usize) -> FixtureSpec {
    FixtureSpec::new(account(), messages)
}

#[test]
fn the_same_spec_generates_the_identical_mailbox() {
    // Without this the fixture is not a yardstick: two runs would measure two
    // different mailboxes and the difference between them would read as a regression.
    let one = generate(&spec(500));
    let two = generate(&spec(500));
    assert_eq!(one.len(), 500);
    for (a, b) in one.folders.iter().zip(&two.folders) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.messages, b.messages);
    }
    // A different seed is a different mailbox, sharing no keys.
    let other = generate(&spec(500).with_seed(99));
    assert_ne!(one.folders[0].messages, other.folders[0].messages);
}

#[test]
fn the_mailbox_has_the_shape_the_benchmarks_assume() {
    let fixture = generate(&spec(5_000));
    assert_eq!(fixture.len(), 5_000, "exactly the requested size");
    assert!(!fixture.is_empty());

    // Newest first, and every message dated — a windowed read ranks on this.
    let ordered = fixture.newest_first();
    assert_eq!(ordered.len(), 5_000);
    for pair in ordered.windows(2) {
        assert!(pair[0].received_at >= pair[1].received_at);
    }
    assert!(ordered.iter().all(|m| m.received_at.is_some()));

    // Keys are unique across the whole account, as a provider's are.
    let keys: HashSet<&str> = ordered.iter().map(|m| m.id.key().as_str()).collect();
    assert_eq!(keys.len(), 5_000);

    // Every folder is populated except Sent-by-arrival, and Sent fills with replies —
    // so the mailbox spans several scopes rather than being one big Inbox.
    assert!(
        fixture
            .folders
            .iter()
            .all(|folder| !folder.messages.is_empty()),
        "a folder with no mail would silently drop a scope from every read"
    );
}

#[test]
fn conversations_span_folders_and_carry_a_real_reference_chain() {
    let fixture = generate(&spec(5_000));
    let mut folders_per_thread: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    let mut longest = 0usize;
    let mut with_references = 0usize;

    for folder in &fixture.folders {
        for message in &folder.messages {
            let thread = message.thread_id().expect("every message is threaded");
            folders_per_thread
                .entry(thread.as_str())
                .or_default()
                .insert(folder.id.as_str());
            longest = longest.max(message.envelope.references.len());
            if !message.envelope.references.is_empty() {
                with_references += 1;
                assert_eq!(
                    message.envelope.in_reply_to.len(),
                    1,
                    "a reply names its parent"
                );
                assert_eq!(
                    message.envelope.references.last(),
                    message.envelope.in_reply_to.first(),
                    "the reference chain ends at the parent, as a real one does"
                );
            }
        }
    }

    assert!(with_references > 0, "the mailbox has replies");
    assert!(
        longest >= 3,
        "a chain, not a star: the deepest thread references {longest} ancestors"
    );
    let crossing = folders_per_thread
        .values()
        .filter(|folders| folders.len() > 1)
        .count();
    assert!(
        crossing > 0,
        "no conversation crosses a folder, so nothing exercises the cross-scope read"
    );
}

#[tokio::test]
async fn thread_ids_match_what_derivation_computes() {
    // The generator stamps each message with the thread id it expects derivation to
    // assign, which is what lets a fixture skip the derivation pass. If the two ever
    // disagree, every thread number the benchmarks report is measuring a mailbox no
    // sync could produce — so hold them to each other here.
    let engine = Engine::open_in_memory().expect("open");
    let spec = spec(400);
    let fixture = populate(&engine, &spec).await.expect("populate");

    let declared: HashMap<String, String> = fixture
        .newest_first()
        .iter()
        .map(|message| {
            (
                message.id.key().as_str().to_owned(),
                message
                    .thread_id()
                    .expect("generated messages are threaded")
                    .as_str()
                    .to_owned(),
            )
        })
        .collect();

    let report = engine
        .derive_mail_threads(&account())
        .await
        .expect("derive");
    assert_eq!(
        report.messages_assigned, 0,
        "derivation rewrote {} message(s), so the generator's ids are not the ones it computes",
        report.messages_assigned
    );
    assert_eq!(
        report.threads,
        declared.values().collect::<HashSet<_>>().len(),
        "derivation found a different number of conversations than the generator built"
    );
}

#[tokio::test]
async fn a_populated_engine_answers_the_reads_the_benchmarks_measure() {
    let engine = Engine::open_in_memory().expect("open");
    let spec = spec(400);
    let fixture = populate(&engine, &spec).await.expect("populate");

    // The folder list reached the store, so every folder is a scope a read walks.
    let mailboxes = engine.mailboxes(&account()).await.expect("mailboxes");
    assert_eq!(mailboxes.len(), fixture.folders.len());

    // The windowed read returns the newest N, in the generator's own order.
    let page = engine.mail_window(&[account()], 25).await.expect("window");
    let expected: Vec<&str> = fixture
        .newest_first()
        .iter()
        .take(25)
        .map(|m| m.id.key().as_str())
        .collect();
    let actual: Vec<&str> = page.iter().map(|m| m.mail.key.as_str()).collect();
    assert_eq!(actual, expected);

    // Every message landed, not just the window.
    assert_eq!(engine.messages(&account()).await.expect("all").len(), 400);

    // A thread read returns every member of the conversations it is asked for — whatever folder
    // the generator filed them in, and however far outside the window they fall. That is what
    // lets a windowed list expand one conversation into its whole history; the read returns the
    // conversations themselves and the host drops the rows it already holds.
    let threads: Vec<String> = page
        .iter()
        .filter_map(|m| m.mail.thread_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();
    let wanted: HashSet<&str> = threads.iter().map(String::as_str).collect();
    let expected: HashSet<&str> = fixture
        .newest_first()
        .iter()
        .filter(|m| m.thread_id().is_some_and(|id| wanted.contains(id.as_str())))
        .map(|m| m.id.key().as_str())
        .collect();
    let members = engine
        .mail_on_threads(&[account()], threads.iter().map(String::as_str))
        .await
        .expect("thread members");
    let got: HashSet<&str> = members.iter().map(|m| m.mail.key.as_str()).collect();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn a_delta_pass_rewrites_only_what_it_carries() {
    // The write half of the fixture, which the flag-only and page benchmarks drive. A
    // delta must upsert its messages and tombstone nothing — a pass that reconciled
    // would empty the folder and every later measurement would be of an empty store.
    let engine = Engine::open_in_memory().expect("open");
    let spec = spec(400);
    let fixture = populate(&engine, &spec).await.expect("populate");
    let folder = fixture
        .folders
        .iter()
        .enumerate()
        .max_by_key(|(_, f)| f.messages.len())
        .map(|(index, _)| index)
        .expect("a fixture has folders");

    let mut edited: Message = fixture.folders[folder].messages[0].clone();
    edited.keywords.insert(Keyword::system(SystemKeyword::Seen));
    let key = edited.id.key().clone();

    let applied = sync_folder(&engine, &spec, &fixture, folder, Pass::Delta(vec![edited]))
        .await
        .expect("apply the delta");
    assert_eq!(applied.upserted, 1);
    assert_eq!(applied.tombstoned, 0, "a delta tombstones nothing");
    assert_eq!(
        engine.messages(&account()).await.expect("all").len(),
        400,
        "the rest of the mailbox is untouched"
    );

    let resolved = engine
        .messages_by_keys(&account(), std::slice::from_ref(&key))
        .await
        .expect("resolve");
    assert!(!resolved[0].is_unread(), "the keyword change landed");
}
