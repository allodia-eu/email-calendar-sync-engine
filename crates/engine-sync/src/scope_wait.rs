//! Waiting out a scope another pass holds, for the calendar maintenance a host cannot usefully
//! retry itself.
//!
//! A sync that finds its scope held does not wait: the pass holding it brings the same changes
//! (`EventScopeSync::result`). Maintenance is different. Re-expanding over a wider window, or
//! fetching a range past the one a cursor was opened over, is something the holding pass does
//! **not** do on the caller's behalf, so giving up leaves the work undone, and a host told
//! "busy" can only sleep and ask again with no idea how long the pass has left. The engine
//! knows the one thing that bounds the wait: a lease is released when its pass ends, or lapses
//! within its TTL when the pass died without releasing it. So the engine waits.
//!
//! It polls rather than being woken: a lease can be held by another process on the same store,
//! which nothing in this one hears release it. The poll backs off to [`MAX_PAUSE`], so a
//! minutes-long backfill costs a claim attempt a second at most.

use core::{future::Future, time::Duration};

use crate::SyncError;

/// The first pause after finding a scope held.
const FIRST_PAUSE: Duration = Duration::from_millis(25);

/// The longest pause between attempts: how late a waiter can notice a released scope.
const MAX_PAUSE: Duration = Duration::from_secs(1);

/// Runs `attempt` until it does not fail with [`SyncError::is_busy`], pausing between tries.
///
/// `attempt` must have done nothing when it reports busy: a claim that was refused, never a
/// pass that wrote and then lost its lease. Each try runs from the top, so it re-reads whatever
/// the holder may have changed (a window it recorded) before deciding there is work at all.
///
/// It does not give up. A caller that wants to stop waiting drops the future.
pub(crate) async fn while_held<T, F, Fut>(mut attempt: F) -> Result<T, SyncError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SyncError>>,
{
    let mut pause = FIRST_PAUSE;
    loop {
        match attempt().await {
            Err(err) if err.is_busy() => {
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(MAX_PAUSE);
            }
            done => return done,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU32, Ordering};

    use engine_store::StoreError;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_held_scope_is_tried_again_until_it_is_free() {
        let tries = AtomicU32::new(0);
        let answer = while_held(|| async {
            if tries.fetch_add(1, Ordering::SeqCst) < 3 {
                Err(SyncError::Store(StoreError::ScopeHeld))
            } else {
                Ok("done")
            }
        })
        .await;
        assert_eq!(answer.unwrap(), "done");
        assert_eq!(tries.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn any_other_failure_is_returned_at_once() {
        let tries = AtomicU32::new(0);
        let answer: Result<(), _> = while_held(|| async {
            tries.fetch_add(1, Ordering::SeqCst);
            Err(SyncError::NoExpansionWindow)
        })
        .await;
        assert!(matches!(answer, Err(SyncError::NoExpansionWindow)));
        assert_eq!(tries.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_pause_backs_off_to_its_ceiling() {
        let start = tokio::time::Instant::now();
        let tries = AtomicU32::new(0);
        while_held(|| async {
            if tries.fetch_add(1, Ordering::SeqCst) < 8 {
                Err(SyncError::Store(StoreError::ScopeHeld))
            } else {
                Ok(())
            }
        })
        .await
        .unwrap();
        // 25 + 50 + 100 + 200 + 400 + 800 ms, then the 1 s ceiling twice.
        assert_eq!(start.elapsed(), Duration::from_millis(3575));
    }
}
