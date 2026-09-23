//! Awaiting the blocking task every store call runs on.

use engine_store::{Result, StoreError};
use tokio::task::JoinHandle;

/// The answer of a blocking store task.
///
/// A task that panicked panics here too, with its own payload. A task that was **cancelled**
/// was never run: a runtime shutting down cancels the blocking tasks it has not started, so
/// the call fails with an error instead. Taken as a panic, every store call queued at shutdown
/// becomes a crash report for an orderly exit.
pub(crate) async fn joined<R>(task: JoinHandle<Result<R>>) -> Result<R> {
    match task.await {
        Ok(answer) => answer,
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(_) => Err(StoreError::Backend(
            "the store call was cancelled before it ran: the runtime is shutting down".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use engine_store::StoreError;

    use super::joined;

    #[tokio::test]
    async fn a_task_that_finished_hands_back_its_answer() {
        let task = tokio::task::spawn_blocking(|| Ok::<_, StoreError>(7));
        assert_eq!(joined(task).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn a_cancelled_task_is_a_store_error_not_a_panic() {
        // A runtime shutting down cancels the blocking tasks it has not started, and the call
        // waiting on one is handed `JoinError::Cancelled`. The call fails; nothing crashed.
        let task = tokio::spawn(std::future::pending::<engine_store::Result<()>>());
        task.abort();
        let answer = joined(task).await;
        assert!(
            matches!(answer, Err(StoreError::Backend(_))),
            "a cancelled store call reads as a failed one: {answer:?}",
        );
    }

    #[tokio::test]
    #[should_panic(expected = "the store task itself failed")]
    async fn a_task_that_panicked_still_panics_with_its_own_message() {
        let task = tokio::task::spawn_blocking(|| -> engine_store::Result<()> {
            panic!("the store task itself failed")
        });
        let _ = joined(task).await;
    }
}
