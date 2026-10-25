//! Retry ownership is independent of the HTTP waiter's lifetime.
use super::ApiError;
use axum::http::StatusCode;
use futures_util::FutureExt;
use serde_json::{json, Value};
use std::{collections::HashMap, future::Future, sync::Arc};
use tokio::sync::{watch, Mutex};

#[derive(Clone, Debug)]
pub(super) struct Cached {
    pub status: StatusCode,
    pub body: Value,
}
struct Flight {
    fingerprint: String,
    result: watch::Receiver<Option<Cached>>,
}
#[derive(Default)]
struct Cache {
    entries: HashMap<String, Flight>,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct RetryBroker {
    cache: Mutex<Cache>,
}
impl RetryBroker {
    pub async fn submit<F, E>(
        self: &Arc<Self>,
        key: String,
        fingerprint: String,
        execute: E,
    ) -> Result<watch::Receiver<Option<Cached>>, ApiError>
    where
        F: Future<Output = Cached> + Send + 'static,
        E: FnOnce() -> F + Send + 'static,
    {
        let mut cache = self.cache.lock().await;
        if let Some(flight) = cache.entries.get(&key) {
            if flight.fingerprint != fingerprint {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "API_IDEMPOTENCY_CONFLICT",
                    "This key belongs to a different request.",
                ));
            }
            return Ok(flight.result.clone());
        }
        if cache.entries.len() >= 4096 || cache.bytes >= 64 * 1024 * 1024 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "API_RETRY_CACHE_FULL",
                "Query existing receipts before starting more operations.",
            ));
        }
        let (sender, receiver) = watch::channel(None);
        cache.entries.insert(
            key,
            Flight {
                fingerprint,
                result: receiver.clone(),
            },
        );
        let broker = self.clone();
        tokio::spawn(async move {
            let result = match std::panic::AssertUnwindSafe(execute()).catch_unwind().await {
                Ok(result) => result,
                Err(_) => Cached {
                    status: StatusCode::CONFLICT,
                    body: json!({"error":{"code":"API_EXECUTION_RESULT_UNKNOWN","message":"Execution stopped unexpectedly. Query task/source receipts before issuing a new mutation."}}),
                },
            };
            broker.cache.lock().await.bytes += result.body.to_string().len();
            sender.send_replace(Some(result));
        });
        Ok(receiver)
    }
}
pub(super) async fn wait(
    mut receiver: watch::Receiver<Option<Cached>>,
) -> Result<Cached, ApiError> {
    loop {
        if let Some(result) = receiver.borrow().clone() {
            return Ok(result);
        }
        receiver.changed().await.map_err(|_| {
            ApiError::new(
                StatusCode::CONFLICT,
                "API_EXECUTION_RESULT_UNKNOWN",
                "Execution ownership was lost. Query actual receipts before retrying.",
            )
        })?;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn disconnected_waiter_cannot_repeat_an_actual_write() {
        let broker = Arc::new(RetryBroker::default());
        let writes = Arc::new(AtomicUsize::new(0));
        let (written_tx, written_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let counter = writes.clone();
        let receiver = broker
            .submit("request".into(), "original".into(), move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                written_tx.send(()).unwrap();
                resume_rx.await.unwrap();
                Cached {
                    status: StatusCode::OK,
                    body: json!({"sessionId":"real-session"}),
                }
            })
            .await
            .unwrap();
        let waiter = tokio::spawn(wait(receiver));
        written_rx.await.unwrap();
        waiter.abort();
        let counter = writes.clone();
        let retry = broker
            .submit("request".into(), "original".into(), move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Cached {
                    status: StatusCode::OK,
                    body: json!({"sessionId":"duplicate"}),
                }
            })
            .await
            .unwrap();
        resume_tx.send(()).unwrap();
        let result = wait(retry).await.unwrap();
        assert_eq!(result.body["sessionId"], "real-session");
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        let conflict = broker
            .submit("request".into(), "changed".into(), || async {
                Cached {
                    status: StatusCode::OK,
                    body: json!({}),
                }
            })
            .await;
        assert_eq!(conflict.unwrap_err().status, StatusCode::CONFLICT);
    }
}
