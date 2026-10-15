//! The point of no return of a submission, and the durable record written before it.
//!
//! A send can be cut off at any point: the app closed, the process killed or suspended, the
//! power gone. Whoever comes back for it has to know whether the message may have reached the
//! server, because the two answers are opposite: a send that cannot have left is retried, and
//! one that may have left must never be sent again without asking. Only the provider knows
//! where, on its transport, that line is, and only the outbox can make a record of crossing it
//! that survives the process. [`HandOver`] is where the two meet.
//!
//! A provider calls [`HandOver::commit`] immediately before the first byte that could cause
//! delivery, and writes that byte only once it returns `Ok`. Everything before it (connecting,
//! authenticating, uploading attachments, sending all of the message but its end) is safe to
//! repeat. Where the line falls on each transport is in `providers.md`.

use core::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use crate::error::{ProviderError, ProviderResult};

/// Where a submission's hand-over is durably recorded: the outbox, under the lease of the
/// attempt making it.
#[async_trait]
pub trait HandOverLog: Send + Sync {
    /// Records that the attempt is about to hand its message over.
    ///
    /// # Errors
    ///
    /// Why the record could not be written. The attempt then stops short of the server.
    async fn record_hand_over(&self) -> Result<(), String>;
}

/// The log for a caller with nothing to record into: a tool, an example, a test of the
/// provider alone. Every submission the engine itself makes goes through the outbox's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unrecorded;

#[async_trait]
impl HandOverLog for Unrecorded {
    async fn record_hand_over(&self) -> Result<(), String> {
        Ok(())
    }
}

/// One submission attempt's hand-over: the record it must write before the point of no
/// return, and the proof that it did.
///
/// A provider's [`submit_email`](crate::Provider::submit_email) receives one, and a
/// [`SubmissionReceipt`](crate::SubmissionReceipt) cannot be built without the [`HandedOver`]
/// that [`commit`](Self::commit) returns, so a send cannot report success without having
/// recorded its hand-over. The transports also take the proof (or the `HandOver` itself) at the
/// write that crosses the line, so the irreversible byte is unreachable without it.
pub struct HandOver<'a> {
    log: &'a dyn HandOverLog,
    committed: AtomicBool,
}

impl<'a> HandOver<'a> {
    /// A hand-over recorded into `log`.
    #[must_use]
    pub fn new(log: &'a dyn HandOverLog) -> Self {
        Self {
            log,
            committed: AtomicBool::new(false),
        }
    }

    /// Records the hand-over, once, and returns the proof. A second call returns the proof
    /// without writing again, so a transport that sends the same request twice (a throttled
    /// one, answered and replayed) records it once.
    ///
    /// # Errors
    ///
    /// A [`Retryable`](engine_core::error::FailureClass::Retryable) [`ProviderError`] when the
    /// record could not be written. The provider must then **not** write the irreversible
    /// byte: nothing durable says the message may have gone, so a recovery would send it
    /// again.
    pub async fn commit(&self) -> ProviderResult<HandedOver> {
        if !self.committed.load(Ordering::Acquire) {
            self.log.record_hand_over().await.map_err(|detail| {
                ProviderError::retryable(format!(
                    "the send stopped before reaching the server, because the outbox could \
                     not record that it was about to: {detail}"
                ))
            })?;
            self.committed.store(true, Ordering::Release);
        }
        Ok(HandedOver(()))
    }

    /// Whether [`commit`](Self::commit) has recorded the hand-over.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Acquire)
    }
}

impl core::fmt::Debug for HandOver<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HandOver")
            .field("committed", &self.is_committed())
            .finish_non_exhaustive()
    }
}

/// Proof that a submission recorded its hand-over. Only [`HandOver::commit`] makes one.
#[derive(Debug)]
pub struct HandedOver(());

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use engine_core::error::FailureClass;

    use super::*;

    #[derive(Default)]
    struct Counting {
        writes: Mutex<u32>,
        refuse: bool,
    }

    #[async_trait]
    impl HandOverLog for Counting {
        async fn record_hand_over(&self) -> Result<(), String> {
            if self.refuse {
                return Err("disk full".to_owned());
            }
            *self.writes.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_hand_over_is_recorded_once_however_often_it_is_committed() {
        let log = Counting::default();
        let hand_over = HandOver::new(&log);
        assert!(!hand_over.is_committed());
        hand_over.commit().await.unwrap();
        hand_over.commit().await.unwrap();
        assert!(hand_over.is_committed());
        assert_eq!(*log.writes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_record_that_cannot_be_written_stops_the_send_as_a_retryable_failure() {
        let log = Counting {
            refuse: true,
            ..Counting::default()
        };
        let hand_over = HandOver::new(&log);
        let err = hand_over.commit().await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Retryable);
        assert!(!err.requires_confirmation());
        assert!(err.detail().contains("disk full"), "{err}");
        assert!(!hand_over.is_committed());
    }

    #[tokio::test]
    async fn an_unrecorded_hand_over_always_commits() {
        let hand_over = HandOver::new(&Unrecorded);
        hand_over.commit().await.unwrap();
        assert!(format!("{hand_over:?}").contains("true"));
    }
}
