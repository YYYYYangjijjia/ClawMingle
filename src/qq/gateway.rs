use std::{
    collections::VecDeque,
    future::pending,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::{
    qq::{
        api::QqApiClient,
        group::{GroupMessageEvent, GroupMessageSource},
        types::{
            C2CMessageEvent, DISPATCH_EVENT, GatewayEnvelope, HEARTBEAT_ACK_EVENT, HEARTBEAT_EVENT,
            HELLO_EVENT, HelloPayload, IDENTIFY_EVENT, INTENT_GROUP_AND_C2C, INVALID_SESSION_EVENT,
            RECONNECT_EVENT, RESUME_EVENT, ReadyPayload,
        },
    },
    util::layout::DataLayout,
};

/// Outbound side of the gateway's only callback into the application: every
/// decoded `C2C_MESSAGE_CREATE` event is forwarded here. The gateway never
/// awaits the handling of an event, so the consumer must spawn per event to
/// preserve the original detached-task concurrency.
type C2CEventSender = mpsc::UnboundedSender<C2CMessageEvent>;

enum GatewayEvents {
    C2C(C2CEventSender),
    Group(mpsc::Sender<GroupGatewayEvent>),
    Chat(mpsc::Sender<InboundGatewayEvent>),
}

/// A receiver must acknowledge only after the canonical message record has
/// been persisted. Losing this acknowledgement stops the group connection;
/// an unconfirmed gateway sequence is never used to resume past the message.
#[derive(Debug)]
pub struct GroupGatewayEvent {
    pub message: GroupMessageEvent,
    pub persisted: oneshot::Sender<()>,
}

/// Both chat scopes share a single QQ connection. The application chooses
/// isolated conversation identities before acknowledging either message kind.
// Keep the transport payloads directly accessible; the containing channel is
// bounded, and boxing only the group variant would complicate its public API.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum InboundMessage {
    Group(GroupMessageEvent),
    Private(C2CMessageEvent),
}

#[derive(Debug)]
pub struct InboundGatewayEvent {
    pub message: InboundMessage,
    pub persisted: oneshot::Sender<()>,
}

type PendingAcknowledgement = (u64, Option<oneshot::Receiver<()>>);
const MAX_PENDING_ACKNOWLEDGEMENTS: usize = 4096;

async fn next_persisted_sequence(queue: &mut VecDeque<PendingAcknowledgement>) -> Result<u64> {
    match queue.front_mut() {
        Some((seq, Some(ack))) => {
            ack.await.map_err(|_| DurableConsumerUnavailable)?;
            Ok(*seq)
        }
        Some((seq, None)) => Ok(*seq),
        None => pending::<Result<u64>>().await,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("QQ event consumer is closed, full, or failed to persist; gateway stopped")]
struct DurableConsumerUnavailable;

impl GatewayEvents {
    fn dispatch(
        &self,
        name: &str,
        value: serde_json::Value,
        bot_id: Option<&str>,
    ) -> Result<Option<oneshot::Receiver<()>>> {
        match (self, name) {
            (Self::C2C(events), "C2C_MESSAGE_CREATE") => {
                let event = serde_json::from_value::<C2CMessageEvent>(value)
                    .map_err(|_| anyhow!("invalid QQ C2C event"))?;
                if events.send(event).is_err() {
                    warn!("c2c event consumer is gone; dropping message from gateway");
                }
            }
            (Self::Group(events), "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE") => {
                let event = decode_group_event(name, value, bot_id)?;
                let (persisted, acknowledgement) = oneshot::channel();
                events
                    .try_send(GroupGatewayEvent {
                        message: event,
                        persisted,
                    })
                    .map_err(|_| DurableConsumerUnavailable)?;
                return Ok(Some(acknowledgement));
            }
            (
                Self::Chat(events),
                "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" | "C2C_MESSAGE_CREATE",
            ) => {
                let message = if name == "C2C_MESSAGE_CREATE" {
                    InboundMessage::Private(
                        serde_json::from_value(value)
                            .map_err(|_| anyhow!("invalid QQ C2C event"))?,
                    )
                } else {
                    InboundMessage::Group(decode_group_event(name, value, bot_id)?)
                };
                let (persisted, acknowledgement) = oneshot::channel();
                // Never block the heartbeat on application work. Overflow or a
                // lost durable acknowledgement is a visible terminal fault.
                events
                    .try_send(InboundGatewayEvent { message, persisted })
                    .map_err(|_| DurableConsumerUnavailable)?;
                return Ok(Some(acknowledgement));
            }
            _ => {}
        }
        Ok(None)
    }
}

fn decode_group_event(
    name: &str,
    value: serde_json::Value,
    bot_id: Option<&str>,
) -> Result<GroupMessageEvent> {
    let mut event = serde_json::from_value::<GroupMessageEvent>(value)
        .map_err(|_| anyhow!("invalid QQ group event"))?;
    event.source = if name == "GROUP_AT_MESSAGE_CREATE" {
        GroupMessageSource::At
    } else {
        GroupMessageSource::All
    };
    event.bot_id = bot_id.map(str::to_owned);
    Ok(event)
}

const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
/// Floor for the server-provided heartbeat interval. Guards against a zero
/// (interval panic) or absurdly small value that would make the half-open
/// check fire before any ACK could realistically round-trip.
const MIN_HEARTBEAT_MS: u64 = 1_000;
/// Reconnect only after this many consecutive heartbeats go unacknowledged, so
/// a single slow ACK or a tick/ACK scheduling race never triggers a spurious
/// reconnect while a truly dead connection is still detected within ~2 ticks.
const MAX_MISSED_HEARTBEAT_ACKS: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GatewaySessionState {
    session_id: Option<String>,
    last_seq: Option<u64>,
    #[serde(default)]
    bot_id: Option<String>,
}

struct GatewaySessionStore {
    path: PathBuf,
    state: RwLock<GatewaySessionState>,
}

impl GatewaySessionStore {
    async fn load_or_init(data_dir: &Path) -> Result<Self> {
        let layout = DataLayout::new(data_dir);
        tokio::fs::create_dir_all(layout.qq_dir()).await?;
        let path = layout.gateway_session_file();
        let state = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str::<GatewaySessionState>(&raw)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(_) => GatewaySessionState::default(),
        };
        let store = Self {
            path,
            state: RwLock::new(state),
        };
        store.persist().await?;
        Ok(store)
    }

    async fn snapshot(&self) -> GatewaySessionState {
        self.state.read().await.clone()
    }

    async fn set_last_seq(&self, last_seq: Option<u64>) -> Result<()> {
        let mut state = self.state.write().await;
        state.last_seq = last_seq;
        drop(state);
        self.persist().await
    }

    async fn set_session_id(&self, session_id: Option<String>) -> Result<()> {
        let mut state = self.state.write().await;
        state.session_id = session_id;
        drop(state);
        self.persist().await
    }

    async fn set_bot_id(&self, bot_id: Option<String>) -> Result<()> {
        self.state.write().await.bot_id = bot_id;
        self.persist().await
    }

    async fn clear(&self) -> Result<()> {
        let mut state = self.state.write().await;
        *state = GatewaySessionState::default();
        drop(state);
        self.persist().await
    }

    async fn persist(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&*self.state.read().await)?;
        crate::util::fs::atomic_write(&self.path, &raw)
            .context("failed to persist QQ gateway session")?;
        Ok(())
    }
}

pub fn spawn_gateway(data_dir: PathBuf, qq_client: Arc<QqApiClient>, events: C2CEventSender) {
    // Preserve the detached C2C entry point. A caller chooses one gateway
    // entry point per Bot; the group entry point uses the same connection loop.
    drop(spawn_gateway_inner(
        data_dir,
        qq_client,
        GatewayEvents::C2C(events),
    ));
}

/// Start one bounded group transport, without starting any C2C application.
/// Abort the handle to disconnect. A full or closed consumer queue ends the
/// task with an error. A receiver acknowledges durable message storage using
/// `GroupGatewayEvent::persisted`; heartbeat work continues while it is pending.
pub fn spawn_group_gateway(
    data_dir: PathBuf,
    qq_client: Arc<QqApiClient>,
    events: mpsc::Sender<GroupGatewayEvent>,
) -> tokio::task::JoinHandle<Result<()>> {
    spawn_gateway_inner(data_dir, qq_client, GatewayEvents::Group(events))
}

/// Start one bounded connection for both private and group messages. Each
/// forwarded message holds back the durable resume cursor until its receiver
/// confirms application persistence; heartbeats continue independently.
pub fn spawn_chat_gateway(
    data_dir: PathBuf,
    qq_client: Arc<QqApiClient>,
    events: mpsc::Sender<InboundGatewayEvent>,
) -> tokio::task::JoinHandle<Result<()>> {
    spawn_gateway_inner(data_dir, qq_client, GatewayEvents::Chat(events))
}

fn spawn_gateway_inner(
    data_dir: PathBuf,
    qq_client: Arc<QqApiClient>,
    events: GatewayEvents,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let session_store = GatewaySessionStore::load_or_init(&data_dir)
            .await
            .map_err(|_| anyhow!("failed to initialize QQ gateway session store"))?;
        let mut reconnect_delay = Duration::from_secs(1);
        loop {
            match connect_once(&qq_client, &events, &session_store, &mut reconnect_delay).await {
                Ok(()) => reconnect_delay = Duration::from_secs(1),
                Err(err) if err.downcast_ref::<DurableConsumerUnavailable>().is_some() => {
                    error!(
                        "QQ event consumer is closed, full, or failed to persist; gateway stopped"
                    );
                    return Err(err);
                }
                Err(_) => {
                    // Neither a websocket error nor an upstream API response
                    // may expose a host, message payload or authentication data.
                    warn!(
                        retry_seconds = reconnect_delay.as_secs(),
                        "QQ gateway connection ended; reconnecting"
                    );
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                }
            }
        }
    })
}

async fn connect_once(
    qq_client: &QqApiClient,
    events: &GatewayEvents,
    session_store: &GatewaySessionStore,
    reconnect_delay: &mut Duration,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let token = qq_client.get_access_token().await?;
    let gateway_url = qq_client.get_gateway_url().await?;
    info!("connecting to QQ gateway");
    let request = gateway_url
        .into_client_request()
        .map_err(|_| anyhow!("failed to build QQ websocket request"))?;
    let (mut websocket, _) = connect_async(request).await?;
    let session = session_store.snapshot().await;
    let mut last_seq = session.last_seq;
    let mut session_id = session.session_id;
    let mut bot_id = session.bot_id;
    let mut heartbeat: Option<tokio::time::Interval> = None;
    // Half-open detection: incremented when a heartbeat is sent, reset to 0 on
    // its ACK. If it reaches MAX_MISSED_HEARTBEAT_ACKS the connection is
    // silently dead (no RST) and we reconnect rather than block forever on
    // websocket.next().
    let mut unacked_heartbeats: u32 = 0;
    let mut pending_acknowledgements = VecDeque::new();

    loop {
        tokio::select! {
            sequence = next_persisted_sequence(&mut pending_acknowledgements) => {
                let sequence = sequence?;
                session_store.set_last_seq(Some(sequence)).await?;
                pending_acknowledgements.pop_front();
            }
            _ = async {
                if let Some(interval) = &mut heartbeat {
                    interval.tick().await;
                } else {
                    pending::<()>().await;
                }
            } => {
                if unacked_heartbeats >= MAX_MISSED_HEARTBEAT_ACKS {
                    return Err(anyhow!("qq gateway heartbeat not acknowledged; connection is half-open"));
                }
                let payload = serde_json::json!({
                    "op": HEARTBEAT_EVENT,
                    "d": last_seq,
                });
                websocket.send(Message::Text(payload.to_string())).await?;
                unacked_heartbeats += 1;
            }
            message = websocket.next() => {
                let Some(message) = message else {
                    return Err(anyhow!("qq gateway websocket closed"));
                };
                let message = message?;
                match message {
                    Message::Text(text) => {
                        let payload = serde_json::from_str::<GatewayEnvelope>(&text)
                            .map_err(|_| anyhow!("failed to parse QQ gateway payload"))?;
                        if let Some(seq) = payload.s {
                            last_seq = Some(seq);
                        }
                        let mut message_acknowledgement = None;
                        match payload.op {
                            HELLO_EVENT => {
                                let hello: HelloPayload = serde_json::from_value(payload.d)?;
                                // Clamp the server-provided interval: tokio::time::interval
                                // panics on a zero period, and heartbeat_interval is untrusted
                                // network data from the gateway HELLO frame. A floor also keeps
                                // a tiny value from tripping the half-open check every cycle.
                                let heartbeat_ms = hello.heartbeat_interval.max(MIN_HEARTBEAT_MS);
                                let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_ms));
                                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                heartbeat = Some(interval);
                                if let (Some(existing_session), Some(seq)) = (session_id.as_deref(), last_seq) {
                                    info!("resuming QQ gateway session");
                                    websocket.send(Message::Text(serde_json::json!({
                                        "op": RESUME_EVENT,
                                        "d": {
                                            "token": format!("QQBot {}", token),
                                            "session_id": existing_session,
                                            "seq": seq,
                                        }
                                    }).to_string())).await?;
                                } else {
                                    let intents = INTENT_GROUP_AND_C2C;
                                    info!("identifying qq gateway with intents {}", intents);
                                    websocket.send(Message::Text(serde_json::json!({
                                        "op": IDENTIFY_EVENT,
                                        "d": {
                                            "token": format!("QQBot {}", token),
                                            "intents": intents,
                                            "shard": [0, 1],
                                        }
                                    }).to_string())).await?;
                                }
                            }
                            DISPATCH_EVENT => {
                                match payload.t.as_deref() {
                                    Some("READY") => {
                                        let ready: ReadyPayload = serde_json::from_value(payload.d)?;
                                        info!("QQ gateway ready");
                                        bot_id = ready.user.map(|user| user.id);
                                        session_store.set_bot_id(bot_id.clone()).await?;
                                        session_id = Some(ready.session_id.clone());
                                        session_store.set_session_id(session_id.clone()).await?;
                                        // Connection is healthy again; reset the reconnect backoff
                                        // so the next unexpected drop retries promptly.
                                        *reconnect_delay = Duration::from_secs(1);
                                    }
                                    Some("RESUMED") => {
                                        info!("qq gateway session resumed");
                                        *reconnect_delay = Duration::from_secs(1);
                                    }
                                    Some(name) => {
                                        message_acknowledgement = events.dispatch(name, payload.d, bot_id.as_deref())?;
                                    }
                                    None => {}
                                }
                            }
                            HEARTBEAT_ACK_EVENT => {
                                unacked_heartbeats = 0;
                            }
                            RECONNECT_EVENT => {
                                return Err(anyhow!("qq gateway requested reconnect"));
                            }
                            INVALID_SESSION_EVENT => {
                                let can_resume = serde_json::from_value::<bool>(payload.d).unwrap_or(false);
                                warn!("qq gateway invalid session, can_resume={}", can_resume);
                                if !can_resume {
                                    session_store.clear().await?;
                                    qq_client.invalidate_access_token().await;
                                }
                                return Err(anyhow!("qq gateway invalid session"));
                            }
                            other => {
                                info!("received gateway op {}", other);
                            }
                        }
                        if let Some(seq) = payload.s {
                            if matches!(events, GatewayEvents::Group(_) | GatewayEvents::Chat(_)) {
                                if pending_acknowledgements.len() >= MAX_PENDING_ACKNOWLEDGEMENTS {
                                    return Err(DurableConsumerUnavailable.into());
                                }
                                pending_acknowledgements.push_back((seq, message_acknowledgement));
                            } else {
                                session_store.set_last_seq(Some(seq)).await?;
                            }
                        } else if message_acknowledgement.is_some() {
                            return Err(anyhow!("QQ message has no gateway sequence"));
                        }
                    }
                    Message::Ping(payload) => {
                        websocket.send(Message::Pong(payload)).await?;
                    }
                    Message::Close(_) => {
                        return Err(anyhow!("QQ gateway closed"));
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_wire(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "group_openid": "group-1",
            "author": { "member_openid": "member-1" },
            "content": "hello", "timestamp": "2026-10-22T12:00:00+08:00"
        })
    }

    #[tokio::test]
    async fn group_dispatch_retains_source_bot_identity_and_duplicate_ids() {
        let (sender, mut receiver) = mpsc::channel(2);
        let events = GatewayEvents::Group(sender);
        let _ = events
            .dispatch(
                "GROUP_MESSAGE_CREATE",
                group_wire("message-1"),
                Some("bot-1"),
            )
            .unwrap();
        let _ = events
            .dispatch(
                "GROUP_AT_MESSAGE_CREATE",
                group_wire("message-1"),
                Some("bot-1"),
            )
            .unwrap();
        let all_envelope = receiver.recv().await.unwrap();
        let at_envelope = receiver.recv().await.unwrap();
        let all = all_envelope.message;
        let at = at_envelope.message;
        assert_eq!(all.source, GroupMessageSource::All);
        assert!(!all.is_at_bot());
        assert_eq!(at.source, GroupMessageSource::At);
        assert!(at.is_at_bot());
        assert_eq!(at.bot_id.as_deref(), Some("bot-1"));
        assert_eq!(all.id, at.id);
    }

    #[test]
    fn group_consumer_overflow_and_closed_channel_are_visible_terminal_errors() {
        let (sender, receiver) = mpsc::channel(1);
        let events = GatewayEvents::Group(sender);
        let _ = events
            .dispatch("GROUP_MESSAGE_CREATE", group_wire("message-1"), None)
            .unwrap();
        let error = events
            .dispatch("GROUP_MESSAGE_CREATE", group_wire("message-2"), None)
            .unwrap_err();
        assert!(error.downcast_ref::<DurableConsumerUnavailable>().is_some());
        drop(receiver);
        let error = events
            .dispatch("GROUP_MESSAGE_CREATE", group_wire("message-3"), None)
            .unwrap_err();
        assert!(error.downcast_ref::<DurableConsumerUnavailable>().is_some());
    }

    #[tokio::test]
    async fn group_gateway_does_not_forward_private_messages() {
        let (sender, mut receiver) = mpsc::channel(1);
        let events = GatewayEvents::Group(sender);
        let _ = events
            .dispatch("C2C_MESSAGE_CREATE", serde_json::json!({}), None)
            .unwrap();
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn ready_bot_identity_survives_resume_state_reload() {
        let ready: ReadyPayload = serde_json::from_value(serde_json::json!({
            "session_id": "sess-1", "user": {"id": "bot-1"}
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = GatewaySessionStore::load_or_init(dir.path()).await.unwrap();
        store
            .set_bot_id(ready.user.map(|user| user.id))
            .await
            .unwrap();
        assert_eq!(reload(dir.path()).await.bot_id.as_deref(), Some("bot-1"));
    }

    #[tokio::test]
    async fn acknowledgements_advance_only_the_confirmed_prefix_and_are_cancel_safe() {
        let (first_sent, first_received) = oneshot::channel();
        let (third_sent, third_received) = oneshot::channel();
        let mut queue = VecDeque::from([
            (10, Some(first_received)),
            (11, None),
            (12, Some(third_received)),
        ]);
        third_sent.send(()).unwrap();
        // Other select branches (including heartbeats) remain available while
        // the first persisted acknowledgement has not arrived.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(1),
                next_persisted_sequence(&mut queue)
            )
            .await
            .is_err()
        );
        assert_eq!(queue.len(), 3);
        first_sent.send(()).unwrap();
        for expected in [10, 11, 12] {
            assert_eq!(next_persisted_sequence(&mut queue).await.unwrap(), expected);
            queue.pop_front();
        }
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn dropped_persistence_acknowledgement_is_a_terminal_fault() {
        let (sender, receiver) = oneshot::channel();
        let mut queue = VecDeque::from([(10, Some(receiver))]);
        drop(sender);
        let error = next_persisted_sequence(&mut queue).await.unwrap_err();
        assert!(error.downcast_ref::<DurableConsumerUnavailable>().is_some());
    }

    fn private_wire(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "author": { "user_openid": "user-1", "bot": false },
            "content": "private hello", "timestamp": "2026-10-23T12:00:00+08:00"
        })
    }

    #[tokio::test]
    async fn unified_gateway_routes_both_chat_scopes_with_separate_durable_acks() {
        let (sender, mut receiver) = mpsc::channel(2);
        let events = GatewayEvents::Chat(sender);
        let private_ack = events
            .dispatch("C2C_MESSAGE_CREATE", private_wire("private-1"), None)
            .unwrap()
            .unwrap();
        let group_ack = events
            .dispatch(
                "GROUP_AT_MESSAGE_CREATE",
                group_wire("group-1"),
                Some("bot-1"),
            )
            .unwrap()
            .unwrap();
        let private = receiver.recv().await.unwrap();
        let group = receiver.recv().await.unwrap();
        match private.message {
            InboundMessage::Private(message) => {
                assert_eq!(message.author.user_openid, "user-1");
                assert!(!message.author.bot);
                assert!(message.timestamp.is_some());
            }
            _ => panic!("private message must retain its scope"),
        }
        match group.message {
            InboundMessage::Group(message) => {
                assert!(message.is_at_bot());
                assert_eq!(message.bot_id.as_deref(), Some("bot-1"));
            }
            _ => panic!("group message must retain its scope"),
        }
        private.persisted.send(()).unwrap();
        group.persisted.send(()).unwrap();
        private_ack.await.unwrap();
        group_ack.await.unwrap();
    }

    #[tokio::test]
    async fn unified_private_cursor_waits_for_private_storage_before_later_group_ack() {
        let directory = tempfile::tempdir().unwrap();
        let store = GatewaySessionStore::load_or_init(directory.path())
            .await
            .unwrap();
        let (sender, mut receiver) = mpsc::channel(2);
        let events = GatewayEvents::Chat(sender);
        let private_ack = events
            .dispatch("C2C_MESSAGE_CREATE", private_wire("private-1"), None)
            .unwrap();
        let group_ack = events
            .dispatch("GROUP_MESSAGE_CREATE", group_wire("group-1"), None)
            .unwrap();
        let private = receiver.recv().await.unwrap();
        let group = receiver.recv().await.unwrap();
        let mut queue = VecDeque::from([(10, private_ack), (11, group_ack)]);
        group.persisted.send(()).unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(1),
                next_persisted_sequence(&mut queue)
            )
            .await
            .is_err()
        );
        assert_eq!(reload(directory.path()).await.last_seq, None);
        private.persisted.send(()).unwrap();
        for expected in [10, 11] {
            let sequence = next_persisted_sequence(&mut queue).await.unwrap();
            assert_eq!(sequence, expected);
            store.set_last_seq(Some(sequence)).await.unwrap();
            queue.pop_front();
            assert_eq!(reload(directory.path()).await.last_seq, Some(expected));
        }
    }

    #[test]
    fn unified_gateway_queue_full_or_closed_is_a_terminal_fault() {
        let (sender, receiver) = mpsc::channel(1);
        let events = GatewayEvents::Chat(sender);
        let _private_ack = events
            .dispatch("C2C_MESSAGE_CREATE", private_wire("private-1"), None)
            .unwrap();
        let error = events
            .dispatch("GROUP_MESSAGE_CREATE", group_wire("group-1"), None)
            .unwrap_err();
        assert!(error.downcast_ref::<DurableConsumerUnavailable>().is_some());
        drop(receiver);
        let error = events
            .dispatch("C2C_MESSAGE_CREATE", private_wire("private-2"), None)
            .unwrap_err();
        assert!(error.downcast_ref::<DurableConsumerUnavailable>().is_some());
    }

    #[tokio::test]
    async fn legacy_private_gateway_remains_unbounded_without_application_ack() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let events = GatewayEvents::C2C(sender);
        assert!(
            events
                .dispatch("C2C_MESSAGE_CREATE", private_wire("private-1"), None)
                .unwrap()
                .is_none()
        );
        assert_eq!(receiver.recv().await.unwrap().id, "private-1");
        assert!(
            events
                .dispatch("GROUP_MESSAGE_CREATE", group_wire("group-1"), None)
                .unwrap()
                .is_none()
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn private_author_bot_flag_is_typed_and_missing_metadata_is_compatible() {
        let mut value = private_wire("private-1");
        value["author"]["bot"] = true.into();
        assert!(
            serde_json::from_value::<C2CMessageEvent>(value.clone())
                .unwrap()
                .author
                .bot
        );
        value["author"].as_object_mut().unwrap().remove("bot");
        value.as_object_mut().unwrap().remove("timestamp");
        let event = serde_json::from_value::<C2CMessageEvent>(value.clone()).unwrap();
        assert!(!event.author.bot);
        assert!(event.timestamp.is_none());
        value["author"]["bot"] = "true".into();
        assert!(serde_json::from_value::<C2CMessageEvent>(value).is_err());
    }

    async fn reload(data_dir: &Path) -> GatewaySessionState {
        GatewaySessionStore::load_or_init(data_dir)
            .await
            .expect("reload store")
            .snapshot()
            .await
    }

    #[tokio::test]
    async fn load_or_init_creates_an_empty_session_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GatewaySessionStore::load_or_init(dir.path())
            .await
            .expect("init store");

        assert!(
            store.path.exists(),
            "session file should be created eagerly"
        );
        let state = store.snapshot().await;
        assert_eq!(state.session_id, None);
        assert_eq!(state.last_seq, None);
    }

    #[tokio::test]
    async fn session_id_and_seq_survive_a_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GatewaySessionStore::load_or_init(dir.path())
            .await
            .expect("init store");
        store
            .set_session_id(Some("sess-1".to_string()))
            .await
            .expect("set session id");
        store.set_last_seq(Some(42)).await.expect("set last seq");

        let state = reload(dir.path()).await;
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));
        assert_eq!(state.last_seq, Some(42));
    }

    #[tokio::test]
    async fn clear_drops_both_fields_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GatewaySessionStore::load_or_init(dir.path())
            .await
            .expect("init store");
        store
            .set_session_id(Some("sess-1".to_string()))
            .await
            .expect("set session id");
        store.set_last_seq(Some(42)).await.expect("set last seq");
        store.clear().await.expect("clear");

        let state = reload(dir.path()).await;
        assert_eq!(state.session_id, None);
        assert_eq!(state.last_seq, None);
    }

    /// A *missing* file is a normal cold start (see the test above), but an
    /// unparsable one is reported rather than silently reset, so `spawn_gateway`
    /// refuses to start instead of quietly discarding a resumable session.
    #[tokio::test]
    async fn unparsable_session_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = DataLayout::new(dir.path());
        tokio::fs::create_dir_all(layout.qq_dir())
            .await
            .expect("mkdir");
        tokio::fs::write(layout.gateway_session_file(), "{ not json")
            .await
            .expect("write garbage");

        assert!(GatewaySessionStore::load_or_init(dir.path()).await.is_err());
    }
}
