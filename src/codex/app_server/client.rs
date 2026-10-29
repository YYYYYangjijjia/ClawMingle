//! Typed JSON-RPC client wrapping [`StdioTransport`].
//!
//! Responsibilities:
//! - correlate outbound requests with incoming responses via an auto-
//!   incrementing `id` + oneshot map;
//! - fan out server notifications to subscribers (via `tokio::sync::broadcast`);
//! - route server-initiated requests (approvals, elicitations) to a single
//!   consumer via an mpsc channel;
//! - signal when the reader task exits so the supervisor can respawn the child.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value as JsonValue;
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{debug, warn};

use super::{
    protocol::{JsonRpcError, Message},
    transport::{self, StdioTransport},
};

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<JsonValue, JsonRpcError>>>>>;
static CONNECTION_IDS: AtomicU64 = AtomicU64::new(1);

/// Build a JSON-RPC 2.0 message: the `jsonrpc` version field followed by the
/// given `(key, value)` parts.
fn jsonrpc_envelope<const N: usize>(parts: [(&'static str, JsonValue); N]) -> JsonValue {
    let mut map = serde_json::Map::new();
    map.insert("jsonrpc".to_string(), JsonValue::from("2.0"));
    for (key, value) in parts {
        map.insert(key.to_string(), value);
    }
    JsonValue::Object(map)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    #[tokio::test]
    async fn dropping_shutdown_caller_still_reaps_the_connection() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-server");
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let transport = Arc::new(
            StdioTransport::spawn(&executable, directory.path(), directory.path(), None, None)
                .unwrap(),
        );
        let (notifications, _) = broadcast::channel(4);
        let (requests, _) = mpsc::channel(4);
        let (client, _) = JsonRpcClient::start(transport.clone(), notifications, requests, None)
            .await
            .unwrap();
        // Force cancellation after shutdown has consumed its reader handle but
        // before it can kill/wait the child. Cleanup must retain both owners.
        let gate = client.stderr_handle.lock().await;
        let owner = client.clone();
        let caller = tokio::spawn(async move { owner.shutdown("fixture shutdown").await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.reader_handle.lock().await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        drop(gate);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !client.shutdown_confirmed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(transport.take_child().await.is_none());
        assert!(client.reader_stop.lock().await.is_none());
        assert!(client.stderr_handle.lock().await.is_none());
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Notification {
    pub(crate) method: String,
    pub(crate) params: JsonValue,
}

#[derive(Debug)]
pub(crate) struct ServerRequest {
    pub(crate) connection_id: u64,
    pub(crate) id: JsonValue,
    pub(crate) method: String,
    pub(crate) params: JsonValue,
}

pub(crate) struct JsonRpcClient {
    pub(crate) connection_id: u64,
    transport: Arc<StdioTransport>,
    pending: PendingMap,
    next_id: AtomicI64,
    shutdown_lock: Mutex<()>,
    reaped: AtomicBool,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    reader_stop: Mutex<Option<oneshot::Sender<()>>>,
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
}

impl JsonRpcClient {
    /// Construct a client and start its reader. Returns the client plus a
    /// receiver that fires when the reader task exits (child EOF).
    pub(crate) async fn start(
        transport: Arc<StdioTransport>,
        notifications_tx: broadcast::Sender<Notification>,
        server_requests_tx: mpsc::Sender<ServerRequest>,
        social_tools: Option<Arc<super::social_tool_policy::Registry>>,
    ) -> Result<(Arc<Self>, oneshot::Receiver<()>)> {
        let connection_id = CONNECTION_IDS.fetch_add(1, Ordering::Relaxed);
        let reader = transport
            .take_stdout()
            .await
            .context("transport stdout already taken")?;
        let stderr = transport.take_stderr().await;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let notifications_for_reader = notifications_tx;

        let (exit_tx, exit_rx) = oneshot::channel();
        let (reader_stop, mut stop_rx) = oneshot::channel();
        let raw_handle = tokio::spawn({
            let pending = pending_for_reader;
            let notifications = notifications_for_reader;
            let server_requests = server_requests_tx.clone();
            async move {
                // Drive the reader loop. `transport::spawn_reader` returns a
                // JoinHandle that completes on EOF; we await that here so we
                // can fire `exit_tx` when it finishes.
                let mut inner = transport::spawn_reader(reader, move |msg| match msg {
                    Message::Response { id, outcome } => {
                        let id = match id.as_i64() {
                            Some(v) => v,
                            None => {
                                warn!(?id, "response with non-integer id; dropping");
                                return;
                            }
                        };
                        let pending = pending.clone();
                        tokio::spawn(async move {
                            let slot = pending.lock().await.remove(&id);
                            if let Some(tx) = slot {
                                let _ = tx.send(outcome);
                            } else {
                                warn!(id, "unmatched response id");
                            }
                        });
                    }
                    Message::Notification { method, params } => {
                        if let Some(registry) = &social_tools {
                            registry.observe_native(connection_id, &method, &params);
                        }
                        let _ = notifications.send(Notification { method, params });
                    }
                    Message::Request { id, method, params } => {
                        let tx = server_requests.clone();
                        tokio::spawn(async move {
                            if let Err(err) = tx
                                .send(ServerRequest {
                                    connection_id,
                                    id,
                                    method,
                                    params,
                                })
                                .await
                            {
                                warn!(error = %err, "server request channel closed");
                            }
                        });
                    }
                });
                tokio::select! {
                    _ = &mut inner => {},
                    _ = &mut stop_rx => { inner.abort(); let _ = inner.await; }
                }
                let _ = exit_tx.send(());
            }
        });

        let stderr_handle = stderr.map(transport::spawn_stderr_logger);

        let client = Arc::new(Self {
            connection_id,
            transport,
            pending,
            next_id: AtomicI64::new(1),
            shutdown_lock: Mutex::new(()),
            reaped: AtomicBool::new(false),
            reader_handle: Mutex::new(Some(raw_handle)),
            reader_stop: Mutex::new(Some(reader_stop)),
            stderr_handle: Mutex::new(stderr_handle),
        });
        Ok((client, exit_rx))
    }

    pub(crate) async fn request<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params_value = serde_json::to_value(params).context("serialize params")?;
        let message = jsonrpc_envelope([
            ("id", JsonValue::from(id)),
            ("method", JsonValue::from(method)),
            ("params", params_value),
        ]);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(err) = self.transport.write_message(message).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }
        let outcome = rx
            .await
            .map_err(|_| anyhow!("app-server transport dropped before response to {method}"))?;
        match outcome {
            Ok(value) => serde_json::from_value(value)
                .with_context(|| format!("deserialize response for {method}")),
            Err(err) => Err(anyhow!(
                "app-server request `{}` failed: {} ({})",
                method,
                err.message,
                err.code
            )),
        }
    }

    /// Fire-and-forget: parameter-less notification (e.g. `initialized`).
    pub(crate) async fn notify_empty(&self, method: &str) -> Result<()> {
        let msg = jsonrpc_envelope([("method", JsonValue::from(method))]);
        self.transport.write_message(msg).await
    }

    pub(crate) async fn respond_ok<R: Serialize>(&self, id: JsonValue, result: &R) -> Result<()> {
        let result = serde_json::to_value(result).context("serialize response result")?;
        let msg = jsonrpc_envelope([("id", id), ("result", result)]);
        self.transport.write_message(msg).await
    }

    pub(crate) async fn respond_err(&self, id: JsonValue, err: JsonRpcError) -> Result<()> {
        let err = serde_json::to_value(err).context("serialize error response")?;
        let msg = jsonrpc_envelope([("id", id), ("error", err)]);
        self.transport.write_message(msg).await
    }

    /// Wake every outstanding request with a disconnect error.
    pub(crate) async fn drain_pending_with_disconnect(&self, reason: &str) {
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(JsonRpcError {
                code: -32000,
                message: format!("app-server disconnected: {reason}"),
                data: None,
            }));
        }
    }

    pub(crate) fn shutdown_confirmed(&self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }

    pub(crate) async fn shutdown(self: &Arc<Self>, reason: &str) {
        // Cleanup owns its connection until the reader and child are reaped.
        // Dropping a caller while it awaits shutdown must not detach a reader
        // or lose a Child already removed from the transport.
        let client = self.clone();
        let reason = reason.to_owned();
        let cleanup = tokio::spawn(async move { client.shutdown_inner(&reason).await });
        let _ = cleanup.await;
    }

    async fn shutdown_inner(&self, reason: &str) {
        let _shutdown = self.shutdown_lock.lock().await;
        debug!(reason, "shutting down JsonRpcClient");
        self.drain_pending_with_disconnect(reason).await;
        if let Some(stop) = self.reader_stop.lock().await.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.reader_handle.lock().await.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.stderr_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(mut child) = self.transport.take_child().await {
            let _ = child.kill().await;
            if child.wait().await.is_ok() {
                self.reaped.store(true, Ordering::Release);
            }
        }
    }
}
