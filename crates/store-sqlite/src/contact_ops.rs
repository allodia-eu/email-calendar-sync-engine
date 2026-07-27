//! SQLite contact-source generation, people CAS, and recipient history.

use engine_core::{
    contact::ContactCard,
    ids::AccountId,
    people::{CanonicalEmail, PeopleSnapshot, Person, PersonSource},
    recipient::{RecipientCoverage, RecipientInteraction, RecipientObservation},
    sync::{ObjectKind, SyncScope},
};
use engine_store::{ContactSourceAvailability, ContactSourceSnapshot, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::convert;

/// Reads live source cards and their generation in one transaction.
pub(crate) fn contact_sources(conn: &mut Connection) -> Result<ContactSourceSnapshot> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let generation = generation(&tx)?;
    // Resolve the ContactCard scopes *first*, then read only their objects. Joining
    // `object` unfiltered would stream and JSON-parse every mail message and calendar
    // event in the database on every people-index rebuild and every `people_page`
    // call, just to discard them in Rust. The scope set is small and known.
    let mut scopes_stmt = tx
        .prepare("SELECT scope_key FROM sync_scope ORDER BY scope_key")
        .map_err(convert::backend)?;
    let scope_rows = scopes_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(convert::backend)?;
    let mut contact_scopes = Vec::new();
    for row in scope_rows {
        let key = row.map_err(convert::backend)?;
        let scope: SyncScope = serde_json::from_str(&key).map_err(convert::backend)?;
        if scope.object_kind() == Some(ObjectKind::ContactCard) {
            contact_scopes.push((key, scope));
        }
    }
    drop(scopes_stmt);

    let mut objects_stmt = tx
        .prepare("SELECT payload FROM object WHERE scope_key = ?1 ORDER BY provider_key")
        .map_err(convert::backend)?;
    let mut sources = Vec::new();
    for (key, scope) in &contact_scopes {
        let rows = objects_stmt
            .query_map([key], |row| row.get::<_, String>(0))
            .map_err(convert::backend)?;
        for row in rows {
            let payload = row.map_err(convert::backend)?;
            let card: ContactCard = serde_json::from_str(&payload).map_err(convert::backend)?;
            sources.push(PersonSource::new(
                scope.account().clone(),
                card.clone(),
                card.source_class,
                card.is_writable,
            ));
        }
    }
    drop(objects_stmt);
    tx.commit().map_err(convert::backend)?;
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ContactSourceSnapshot {
        generation,
        sources,
    })
}

/// Loads the current people generation.
pub(crate) fn people_snapshot(conn: &Connection) -> Result<PeopleSnapshot> {
    let next_id: i64 = conn
        .query_row(
            "SELECT next_person_id FROM contact_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(convert::backend)?;
    let next_id = u64::try_from(next_id).map_err(convert::backend)?;
    let mut stmt = conn
        .prepare("SELECT payload FROM person ORDER BY ordinal")
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(convert::backend)?;
    let people = rows
        .map(|row| {
            let payload = row.map_err(convert::backend)?;
            serde_json::from_str::<Person>(&payload).map_err(convert::backend)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut stmt = conn
        .prepare("SELECT retired_id, current_id FROM person_alias ORDER BY retired_id")
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .map_err(convert::backend)?;
    let mut aliases = std::collections::BTreeMap::new();
    for row in rows {
        let (retired, current) = row.map_err(convert::backend)?;
        aliases.insert(person_id(retired)?, person_id(current)?);
    }
    Ok(PeopleSnapshot {
        people,
        aliases,
        next_id,
    })
}

/// Replaces people under a source-generation compare-and-swap.
pub(crate) fn replace_people(
    conn: &mut Connection,
    expected: u64,
    people: &PeopleSnapshot,
) -> Result<bool> {
    let tx = conn.transaction().map_err(convert::backend)?;
    if generation(&tx)? != expected {
        return Ok(false);
    }
    for table in ["person_email", "person_source", "person_alias", "person"] {
        tx.execute(&format!("DELETE FROM {table}"), [])
            .map_err(convert::backend)?;
    }
    for (ordinal, person) in people.people.iter().enumerate() {
        let id = id_to_i64(person.id.get())?;
        let ordinal = i64::try_from(ordinal).map_err(convert::backend)?;
        let payload = serde_json::to_string(person).map_err(convert::backend)?;
        tx.execute(
            "INSERT INTO person (id, ordinal, display_name, payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, ordinal, person.display_name, payload],
        )
        .map_err(convert::backend)?;
        for source in &person.sources {
            tx.execute(
                "INSERT INTO person_source (person_id, account, contact)
                 VALUES (?1, ?2, ?3)",
                params![id, source.account.as_str(), source.contact.as_str()],
            )
            .map_err(convert::backend)?;
        }
        for email in &person.emails {
            tx.execute(
                "INSERT INTO person_email (person_id, email) VALUES (?1, ?2)",
                params![id, email.value.as_str()],
            )
            .map_err(convert::backend)?;
        }
    }
    for (retired, current) in &people.aliases {
        tx.execute(
            "INSERT INTO person_alias (retired_id, current_id) VALUES (?1, ?2)",
            params![id_to_i64(retired.get())?, id_to_i64(current.get())?],
        )
        .map_err(convert::backend)?;
    }
    tx.execute(
        "UPDATE contact_state SET next_person_id = ?1 WHERE singleton = 1",
        [id_to_i64(people.next_id)?],
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(true)
}

/// Inserts observations without reviving an existing suppressed identity.
pub(crate) fn insert_observations(
    tx: &Transaction<'_>,
    observations: &[RecipientObservation],
) -> Result<()> {
    for observation in observations {
        tx.execute(
            "INSERT OR IGNORE INTO recipient_observation
             (account, source_message, email, name, sent_at, suppressed)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![
                observation.account.as_str(),
                observation.source_message.as_str(),
                observation.email.as_str(),
                observation.name,
                observation.sent_at.map(|instant| instant.to_string()),
            ],
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Bumps the source generation inside a contact-card apply transaction.
pub(crate) fn bump_generation(tx: &Transaction<'_>) -> Result<u64> {
    tx.execute(
        "UPDATE contact_state SET generation = generation + 1 WHERE singleton = 1",
        [],
    )
    .map_err(convert::backend)?;
    generation(tx)
}

/// Aggregates non-suppressed observations.
pub(crate) fn recipient_interactions(
    conn: &Connection,
    account: Option<&AccountId>,
) -> Result<Vec<RecipientInteraction>> {
    let sql = if account.is_some() {
        "SELECT email, MIN(name), COUNT(*), MAX(sent_at)
         FROM recipient_observation
         WHERE suppressed = 0 AND account = ?1
         GROUP BY email ORDER BY email"
    } else {
        "SELECT email, MIN(name), COUNT(*), MAX(sent_at)
         FROM recipient_observation
         WHERE suppressed = 0
         GROUP BY email ORDER BY email"
    };
    let mut stmt = conn.prepare(sql).map_err(convert::backend)?;
    let collect = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    };
    let rows = match account {
        Some(account) => stmt
            .query_map([account.as_str()], collect)
            .map_err(convert::backend)?
            .collect::<rusqlite::Result<Vec<_>>>(),
        None => stmt
            .query_map([], collect)
            .map_err(convert::backend)?
            .collect::<rusqlite::Result<Vec<_>>>(),
    }
    .map_err(convert::backend)?;
    rows.into_iter()
        .map(|(email, name, count, sent_at)| {
            Ok(RecipientInteraction::new(
                CanonicalEmail::parse(&email).map_err(convert::backend)?,
                name,
                u64::try_from(count).map_err(convert::backend)?,
                sent_at
                    .map(|value| value.parse().map_err(convert::backend))
                    .transpose()?,
            ))
        })
        .collect()
}

/// Suppresses rows using one of the fixed safe predicates.
pub(crate) fn suppress_email(conn: &Connection, email: &CanonicalEmail) -> Result<usize> {
    conn.execute(
        "UPDATE recipient_observation SET suppressed = 1
         WHERE email = ?1 AND suppressed = 0",
        [email.as_str()],
    )
    .map_err(convert::backend)
}

pub(crate) fn suppress_account(conn: &Connection, account: &AccountId) -> Result<usize> {
    conn.execute(
        "UPDATE recipient_observation SET suppressed = 1
         WHERE account = ?1 AND suppressed = 0",
        [account.as_str()],
    )
    .map_err(convert::backend)
}

pub(crate) fn suppress_all(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE recipient_observation SET suppressed = 1 WHERE suppressed = 0",
        [],
    )
    .map_err(convert::backend)
}

/// Inserts a one-time interaction backfill and its version atomically.
/// Reads the account's interaction-index version, if a backfill has run.
pub(crate) fn recipient_index_version(
    conn: &Connection,
    account: &AccountId,
) -> Result<Option<u32>> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT version FROM recipient_index_state WHERE account = ?1",
            [account.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    Ok(current.map(|value| u32::try_from(value).unwrap_or(u32::MAX)))
}

pub(crate) fn apply_recipient_backfill(
    conn: &mut Connection,
    account: &AccountId,
    version: u32,
    observations: &[RecipientObservation],
) -> Result<bool> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let current: Option<i64> = tx
        .query_row(
            "SELECT version FROM recipient_index_state WHERE account = ?1",
            [account.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    if current.is_some_and(|current| current >= i64::from(version)) {
        return Ok(false);
    }
    insert_observations(&tx, observations)?;
    tx.execute(
        "INSERT INTO recipient_index_state (account, version) VALUES (?1, ?2)
         ON CONFLICT(account) DO UPDATE SET version = excluded.version",
        params![account.as_str(), i64::from(version)],
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(true)
}

/// Upserts one account's observation-coverage statement.
pub(crate) fn set_recipient_coverage(
    conn: &Connection,
    coverage: &RecipientCoverage,
) -> Result<()> {
    let window = serde_json::to_string(&coverage.window).map_err(convert::backend)?;
    conn.execute(
        "INSERT INTO recipient_coverage
         (account, window_json, sent_collection_present) VALUES (?1, ?2, ?3)
         ON CONFLICT(account) DO UPDATE SET
             window_json = excluded.window_json,
             sent_collection_present = excluded.sent_collection_present",
        params![
            coverage.account.as_str(),
            window,
            i64::from(coverage.sent_collection_identified),
        ],
    )
    .map_err(convert::backend)?;
    Ok(())
}

/// Reads recipient coverage in stable account order.
pub(crate) fn recipient_coverage(
    conn: &Connection,
    account: Option<&AccountId>,
) -> Result<Vec<RecipientCoverage>> {
    let sql = if account.is_some() {
        "SELECT account, window_json, sent_collection_present
         FROM recipient_coverage WHERE account = ?1 ORDER BY account"
    } else {
        "SELECT account, window_json, sent_collection_present
         FROM recipient_coverage ORDER BY account"
    };
    let mut stmt = conn.prepare(sql).map_err(convert::backend)?;
    let collect = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    };
    let rows = match account {
        Some(account) => stmt
            .query_map([account.as_str()], collect)
            .map_err(convert::backend)?
            .collect::<rusqlite::Result<Vec<_>>>(),
        None => stmt
            .query_map([], collect)
            .map_err(convert::backend)?
            .collect::<rusqlite::Result<Vec<_>>>(),
    }
    .map_err(convert::backend)?;
    rows.into_iter()
        .map(|(account, window, sent)| {
            Ok(RecipientCoverage {
                account: AccountId::try_from(account.as_str()).map_err(convert::backend)?,
                window: serde_json::from_str(&window).map_err(convert::backend)?,
                sent_collection_identified: sent != 0,
            })
        })
        .collect()
}

/// Upserts source availability.
pub(crate) fn set_source_availability(
    conn: &Connection,
    scope: &SyncScope,
    availability: &ContactSourceAvailability,
) -> Result<()> {
    let (available, reason) = match availability {
        ContactSourceAvailability::Available => (1_i64, None),
        ContactSourceAvailability::Unavailable { reason } => (0_i64, Some(reason.as_str())),
    };
    conn.execute(
        "INSERT INTO contact_source_availability (scope_key, available, reason)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(scope_key) DO UPDATE SET
             available = excluded.available, reason = excluded.reason",
        params![convert::scope_key(scope), available, reason],
    )
    .map_err(convert::backend)?;
    Ok(())
}

/// Reads source availability for one account.
pub(crate) fn source_availability(
    conn: &Connection,
    account: &AccountId,
) -> Result<Vec<(SyncScope, ContactSourceAvailability)>> {
    let mut stmt = conn
        .prepare(
            "SELECT scope_key, available, reason
             FROM contact_source_availability ORDER BY scope_key",
        )
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(convert::backend)?;
    let mut result = Vec::new();
    for row in rows {
        let (scope, available, reason) = row.map_err(convert::backend)?;
        let scope: SyncScope = serde_json::from_str(&scope).map_err(convert::backend)?;
        if scope.account() != account {
            continue;
        }
        let availability = if available != 0 {
            ContactSourceAvailability::Available
        } else {
            ContactSourceAvailability::Unavailable {
                reason: reason.unwrap_or_else(|| "source unavailable".into()),
            }
        };
        result.push((scope, availability));
    }
    Ok(result)
}

fn generation(tx: &Transaction<'_>) -> Result<u64> {
    let value: i64 = tx
        .query_row(
            "SELECT generation FROM contact_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(convert::backend)?;
    u64::try_from(value).map_err(convert::backend)
}

fn id_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(convert::backend)
}

fn person_id(value: i64) -> Result<engine_core::ids::PersonId> {
    let value = u64::try_from(value).map_err(convert::backend)?;
    engine_core::ids::PersonId::new(value).map_err(convert::backend)
}
