//! Conversions across the SQL boundary, plus the small lease-time helpers.
//!
//! Everything the store persists is mapped here between the contract's domain
//! types (`SyncScope`, `UtcDateTime`, `PendingOpState`, fencing generations) and
//! the `TEXT`/`INTEGER` columns the schema (`schema.rs`) stores them in. Keeping
//! the mapping in one place means the row shapes have a single source of truth.

use core::time::Duration;

use engine_core::sync::SyncScope;
use engine_core::time::UtcDateTime;
use engine_core::write::PendingOpId;
use engine_store::{PendingOpState, Result, StoreError};

/// Wraps any backend failure (rusqlite, serde, integer range) as a redacted
/// [`StoreError::Backend`]; the concrete cause stays at the SQL layer.
pub(crate) fn backend(err: impl core::fmt::Display) -> StoreError {
    StoreError::Backend(err.to_string())
}

/// The stable primary-key text for a scope.
///
/// `SyncScope` is an enum of string/enum fields, so its JSON form is canonical
/// and unambiguous (no map keys to reorder) — and serialization cannot fail.
pub(crate) fn scope_key(scope: &SyncScope) -> String {
    serde_json::to_string(scope).expect("SyncScope serialization is infallible")
}

/// Renders an instant to its canonical `…Z` text form for storage.
pub(crate) fn instant_to_text(instant: UtcDateTime) -> String {
    instant.to_string()
}

/// Parses a stored instant back from its canonical text form.
pub(crate) fn parse_instant(text: &str) -> Result<UtcDateTime> {
    text.parse().map_err(backend)
}

/// Parses an optional stored instant (a `NULL` lease expiry means "not held").
pub(crate) fn parse_opt_instant(text: Option<String>) -> Result<Option<UtcDateTime>> {
    match text {
        Some(value) => Ok(Some(parse_instant(&value)?)),
        None => Ok(None),
    }
}

/// True if a lease is held and has not expired at `now` (mirrors the reference
/// store: liveness is by expiry, supremacy is by fencing token).
pub(crate) fn is_live(expiry: Option<UtcDateTime>, now: UtcDateTime) -> bool {
    expiry.is_some_and(|e| e > now)
}

/// Computes a lease expiry from the current instant and a TTL.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] if the expiry would overflow representable
/// time (a real clock never reaches it).
pub(crate) fn expiry_after(now: UtcDateTime, ttl: Duration) -> Result<UtcDateTime> {
    now.checked_add(ttl)
        .ok_or_else(|| StoreError::Backend("lease ttl overflow".to_owned()))
}

/// Encodes a pending-op lifecycle state as the text stored in its column.
pub(crate) fn state_to_text(state: PendingOpState) -> &'static str {
    match state {
        PendingOpState::Pending => "Pending",
        PendingOpState::InFlight => "InFlight",
        PendingOpState::NeedsConfirmation => "NeedsConfirmation",
        PendingOpState::Succeeded => "Succeeded",
        PendingOpState::Failed => "Failed",
    }
}

/// Decodes a stored pending-op state.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on an unrecognized state string (corruption).
pub(crate) fn parse_state(text: &str) -> Result<PendingOpState> {
    Ok(match text {
        "Pending" => PendingOpState::Pending,
        "InFlight" => PendingOpState::InFlight,
        "NeedsConfirmation" => PendingOpState::NeedsConfirmation,
        "Succeeded" => PendingOpState::Succeeded,
        "Failed" => PendingOpState::Failed,
        other => {
            return Err(StoreError::Backend(format!(
                "unknown pending-op state: {other}"
            )));
        }
    })
}

/// Narrows a fencing generation to the `i64` SQLite stores (generations are tiny;
/// this never fails in practice).
pub(crate) fn generation_to_i64(generation: u64) -> Result<i64> {
    i64::try_from(generation).map_err(backend)
}

/// Widens a stored generation back to the `u64` the fencing token uses.
pub(crate) fn generation_from_i64(generation: i64) -> Result<u64> {
    u64::try_from(generation).map_err(backend)
}

/// Narrows a [`PendingOpId`] to the `i64` rowid it maps to.
pub(crate) fn op_id_to_i64(id: PendingOpId) -> Result<i64> {
    i64::try_from(id.get()).map_err(backend)
}

/// Rebuilds a [`PendingOpId`] from a stored rowid.
pub(crate) fn op_id_from_i64(id: i64) -> Result<PendingOpId> {
    Ok(PendingOpId::new(generation_from_i64(id)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::ids::AccountId;
    use engine_core::sync::{JmapDataType, SyncScope};

    fn instant(text: &str) -> UtcDateTime {
        text.parse().expect("valid instant")
    }

    fn scope(data_type: JmapDataType) -> SyncScope {
        SyncScope::JmapType {
            account: AccountId::try_from("a").expect("valid account"),
            data_type,
        }
    }

    #[test]
    fn scope_key_is_stable_and_distinguishes_scopes() {
        assert_eq!(
            scope_key(&scope(JmapDataType::Email)),
            scope_key(&scope(JmapDataType::Email))
        );
        assert_ne!(
            scope_key(&scope(JmapDataType::Email)),
            scope_key(&scope(JmapDataType::Mailbox))
        );
    }

    #[test]
    fn instants_round_trip_through_text() {
        let t = instant("2026-03-01T09:00:00Z");
        assert_eq!(parse_instant(&instant_to_text(t)).unwrap(), t);
        assert!(parse_instant("not-a-time").is_err());
        assert_eq!(parse_opt_instant(None).unwrap(), None);
        assert_eq!(
            parse_opt_instant(Some(instant_to_text(t))).unwrap(),
            Some(t)
        );
    }

    #[test]
    fn lease_liveness_and_expiry() {
        let now = instant("2026-01-01T00:00:00Z");
        assert!(expiry_after(now, Duration::from_secs(30)).unwrap() > now);
        // Past the end of representable time, the expiry overflows to an error.
        assert!(expiry_after(instant("9999-12-31T23:59:59Z"), Duration::from_secs(30)).is_err());
        assert!(is_live(Some(instant("2026-01-01T00:00:30Z")), now));
        assert!(!is_live(Some(now), now)); // expiry must be strictly after now
        assert!(!is_live(None, now));
    }

    #[test]
    fn pending_op_states_round_trip_and_reject_garbage() {
        for state in [
            PendingOpState::Pending,
            PendingOpState::InFlight,
            PendingOpState::NeedsConfirmation,
            PendingOpState::Succeeded,
            PendingOpState::Failed,
        ] {
            assert_eq!(parse_state(state_to_text(state)).unwrap(), state);
        }
        assert!(parse_state("Unknown").is_err());
    }

    #[test]
    fn integer_conversions_round_trip() {
        assert_eq!(
            generation_from_i64(generation_to_i64(5).unwrap()).unwrap(),
            5
        );
        let id = PendingOpId::new(9);
        assert_eq!(op_id_from_i64(op_id_to_i64(id).unwrap()).unwrap(), id);
    }
}
