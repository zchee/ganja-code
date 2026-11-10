//! Bounded waits shared by the local backends' cleanup paths.

use std::future::Future;

use ganja_core::teammate::SETTLE;
use tokio::time::Instant;

/// Reuse the ordinary teammate grace interval for each cleanup operation.
/// A timeout is unconfirmed cleanup, not evidence that the resource is gone.
pub(crate) async fn wait<T>(
    teammate: &str,
    step: &str,
    work: impl Future<Output = T>,
) -> Option<T> {
    until(teammate, step, Instant::now() + SETTLE, work).await
}

/// Polling phases share one deadline rather than renewing it on each query.
pub(crate) async fn until<T>(
    teammate: &str,
    step: &str,
    deadline: Instant,
    work: impl Future<Output = T>,
) -> Option<T> {
    match tokio::time::timeout_at(deadline, work).await {
        Ok(result) => Some(result),
        Err(_) => {
            tracing::warn!(teammate, step, "teammate cleanup timed out; completion is unconfirmed");
            None
        }
    }
}
