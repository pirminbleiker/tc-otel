//! Transport-agnostic request/response dispatcher for AMS.
//!
//! # Why this exists
//!
//! The legacy observer path is strictly receive-only: `MqttAmsTransport` +
//! `AdsRouter` decode inbound AMS frames from the PLC and route them into
//! the logs/metrics/traces pipeline. That works fine for the PLC → tc-otel
//! direction.
//!
//! For the reverse direction — tc-otel issuing ADS reads and AddNotification
//! subscriptions against a PLC (T-numbered work in the custom-metrics client
//! bridge) — we need **request/response with invoke-id correlation over
//! whichever transport the target is reachable on**. tc-otel must not care
//! whether the remote is on MQTT or direct TCP: it hands an `AmsAddr` and a
//! payload to this dispatcher, and the dispatcher picks the right transport
//! from its live route table.
//!
//! # Design
//!
//! - A single [`AmsDispatcher`] owns one MQTT peer and (future) one TCP
//!   dialer. Both are optional — you can run MQTT-only, TCP-only, or both.
//! - A [`RouteTable`] maps `AmsNetId → TransportKind`, populated from:
//!   - MQTT `AdsOverMqtt/<peer>/info` announcements (peer is reachable via
//!     MQTT).
//!   - Live TCP connections inbound (peer is reachable via TCP).
//!   - Static overrides via [`AmsDispatcher::add_static_route`] (for unusual
//!     deployments where the peer doesn't announce itself).
//! - On [`AmsDispatcher::send_request`], the dispatcher allocates a fresh
//!   invoke-id, publishes the AMS frame to the right transport, and awaits
//!   a response whose invoke-id matches. A `oneshot` channel resolves the
//!   waiter.
//!
//! # TCP outbound
//!
//! For TCP targets the dispatcher **dials outbound lazily**: register a peer
//! with [`AmsDispatcher::add_tcp_peer`] (NetID → `SocketAddr`), and on the
//! first `send_request` for that NetID the dispatcher opens one persistent
//! TCP connection and spawns a reader task. The reader parses AMS frames off
//! the socket and routes responses back to waiters via invoke-id. Subsequent
//! requests to the same peer reuse the connection. If the peer closes the
//! connection, the next send attempts a re-dial automatically.
//!
//! # What's out of scope (for this PR)
//!
//! - Notifications (`AddDeviceNotification` / incoming stamps). The dispatcher
//!   correlates subscribe/unsubscribe itself, but stamp fan-out to many
//!   listeners needs a separate broadcast channel.
//! - TCP *inbound* route learning — i.e. "a peer dialed us, so it must be
//!   reachable via TCP". That depends on wiring the existing
//!   `TcpAmsTransport` listener into the same route table, which lives in the
//!   next commit.
//! - Wiring into [`crate::router::AdsRouter`] or [`tc_otel_service`]. The
//!   dispatcher stays a library primitive in this PR; the callers migrate in
//!   a separate commit.

use crate::ams::{AmsHeader, AmsNetId};
use crate::error::AdsError;
use parking_lot::RwLock;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

const AMS_HEADER_LEN: usize = 32;
/// State-flags bit indicating a reply, per Beckhoff AMS spec. Set on all
/// responses and on all notification "command=8" frames.
const AMS_STATE_RESPONSE: u16 = 0x0001;

/// Kind of transport the route-table entry points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Mqtt,
    Tcp,
}

/// Thread-safe route table. Exposed so the caller can inspect or seed it.
#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    inner: Arc<RwLock<HashMap<AmsNetId, TransportKind>>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Learn (or refresh) a route. Returns `true` if the kind changed or the
    /// entry was new.
    pub fn learn(&self, net_id: AmsNetId, kind: TransportKind) -> bool {
        let mut guard = self.inner.write();
        match guard.insert(net_id, kind) {
            Some(prev) => prev != kind,
            None => true,
        }
    }

    /// Remove a route. Returns the kind that was present, if any.
    pub fn forget(&self, net_id: AmsNetId) -> Option<TransportKind> {
        self.inner.write().remove(&net_id)
    }

    pub fn get(&self, net_id: AmsNetId) -> Option<TransportKind> {
        self.inner.read().get(&net_id).copied()
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    pub fn snapshot(&self) -> Vec<(AmsNetId, TransportKind)> {
        self.inner.read().iter().map(|(k, v)| (*k, *v)).collect()
    }
}

/// Errors specific to the dispatcher layer.
#[derive(Debug, thiserror::Error)]
pub enum DispatcherError {
    #[error("no route for AMS Net ID {0}")]
    NoRoute(AmsNetId),
    #[error("transport '{0:?}' not attached")]
    TransportNotAttached(TransportKind),
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("response channel closed — dispatcher shut down")]
    Closed,
    #[error("ads protocol error: {0}")]
    Ads(#[from] AdsError),
    #[error("mqtt error: {0}")]
    Mqtt(String),
}

impl From<rumqttc::ClientError> for DispatcherError {
    fn from(e: rumqttc::ClientError) -> Self {
        DispatcherError::Mqtt(e.to_string())
    }
}

/// MQTT peer attached to an [`AmsDispatcher`]. Owns its own client-id so it
/// can coexist with the observer-side `MqttAmsTransport`.
pub struct MqttPeer {
    client: AsyncClient,
    #[allow(dead_code)]
    event_task: JoinHandle<()>,
    topic_prefix: String,
}

impl MqttPeer {
    fn outbound_topic(&self, target: AmsNetId) -> String {
        format!("{}/{}/ams", self.topic_prefix, target)
    }
}

/// One outbound TCP connection to a PLC. Reader task runs in background and
/// routes inbound frames to pending waiters or the sink (shared with the
/// MQTT code path).
struct TcpPeer {
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    #[allow(dead_code)]
    reader_task: JoinHandle<()>,
    #[allow(dead_code)]
    addr: SocketAddr,
}

/// Mutable outbound-TCP state: current live connection + known addresses.
#[derive(Default)]
struct TcpState {
    peers: HashMap<AmsNetId, Arc<TcpPeer>>,
    addrs: HashMap<AmsNetId, SocketAddr>,
}

/// Trait for a sink that receives parsed AMS frames (header + payload).
///
/// The dispatcher uses this to hand inbound frames it received on a
/// "request" topic (i.e. frames the PLC directed *at us* — not our own
/// responses) to the observer side of the stack. Tests and integration
/// callers can substitute a channel-based sink.
pub trait InboundSink: Send + Sync + 'static {
    fn deliver(&self, header: AmsHeader, payload: Vec<u8>);
}

/// No-op sink. Used when a dispatcher is running outbound-only (no observer
/// counterpart wired).
pub struct NullSink;
impl InboundSink for NullSink {
    fn deliver(&self, _header: AmsHeader, _payload: Vec<u8>) {}
}

/// A [`oneshot::Sender`] registered under an invoke-id for request/response
/// correlation.
type PendingMap = Arc<Mutex<HashMap<u32, oneshot::Sender<ResponseFrame>>>>;

/// Decoded response frame passed to the waiter.
#[derive(Debug, Clone)]
pub struct ResponseFrame {
    pub header: AmsHeader,
    pub payload: Vec<u8>,
}

/// Transport-agnostic request/response dispatcher.
pub struct AmsDispatcher {
    source_net_id: AmsNetId,
    source_port: u16,
    routes: RouteTable,
    pending: PendingMap,
    invoke_counter: Arc<AtomicU32>,
    mqtt: Option<Arc<MqttPeer>>,
    tcp: Arc<Mutex<TcpState>>,
    /// Shared sink for frames received that are *not* replies to our
    /// own requests (i.e. PLC-initiated ADS writes, notifications, etc.).
    inbound_sink: Arc<dyn InboundSink>,
}

impl AmsDispatcher {
    /// Create a dispatcher with no transports attached. Call
    /// [`AmsDispatcher::attach_mqtt`] (and later `attach_tcp`) to plug them in.
    pub fn new(source_net_id: AmsNetId, source_port: u16) -> Self {
        Self::with_sink(source_net_id, source_port, Arc::new(NullSink))
    }

    pub fn with_sink(
        source_net_id: AmsNetId,
        source_port: u16,
        inbound_sink: Arc<dyn InboundSink>,
    ) -> Self {
        Self {
            source_net_id,
            source_port,
            routes: RouteTable::new(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            invoke_counter: Arc::new(AtomicU32::new(1)),
            mqtt: None,
            tcp: Arc::new(Mutex::new(TcpState::default())),
            inbound_sink,
        }
    }

    pub fn routes(&self) -> RouteTable {
        self.routes.clone()
    }

    pub fn source_net_id(&self) -> AmsNetId {
        self.source_net_id
    }

    pub fn source_port(&self) -> u16 {
        self.source_port
    }

    /// Configuration block for the MQTT transport attached to this dispatcher.
    /// The client-id must be distinct from any other MQTT peer on the same
    /// broker (including the observer-side `MqttAmsTransport`) — otherwise
    /// the broker will disconnect one of them.
    pub async fn attach_mqtt(
        &mut self,
        broker_host: &str,
        broker_port: u16,
        client_id: &str,
        topic_prefix: &str,
    ) -> std::result::Result<(), DispatcherError> {
        if self.mqtt.is_some() {
            // Idempotent — replacing the MQTT peer would lose pending requests;
            // force the caller to construct a new dispatcher instead.
            return Err(DispatcherError::Mqtt(
                "MQTT transport already attached".into(),
            ));
        }

        let mut opts = MqttOptions::new(client_id, broker_host, broker_port);
        opts.set_keep_alive(Duration::from_secs(60));
        opts.set_max_packet_size(16 * 1024 * 1024, 16 * 1024 * 1024);

        let (client, mut event_loop) = AsyncClient::new(opts, 64);

        let topic_prefix = topic_prefix.to_string();
        let topic_prefix_for_task = topic_prefix.clone();
        let res_topic = format!("{}/{}/ams/res", topic_prefix, self.source_net_id);
        let own_ams_topic = format!("{}/{}/ams", topic_prefix, self.source_net_id);
        let info_glob = format!("{}/+/info", topic_prefix);

        let routes = self.routes.clone();
        let pending = self.pending.clone();
        let sink = self.inbound_sink.clone();
        let client_for_task = client.clone();

        let event_task = tokio::spawn(async move {
            loop {
                match event_loop.poll().await {
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        if let Err(e) = client_for_task.subscribe(&res_topic, QoS::AtMostOnce).await
                        {
                            tracing::warn!("dispatcher mqtt: res subscribe failed: {e}");
                        }
                        if let Err(e) = client_for_task
                            .subscribe(&own_ams_topic, QoS::AtMostOnce)
                            .await
                        {
                            tracing::warn!("dispatcher mqtt: own ams subscribe failed: {e}");
                        }
                        if let Err(e) = client_for_task.subscribe(&info_glob, QoS::AtMostOnce).await
                        {
                            tracing::warn!("dispatcher mqtt: info subscribe failed: {e}");
                        }
                    }
                    Ok(Event::Incoming(Incoming::Publish(publish))) => {
                        handle_incoming_mqtt(
                            &publish.topic,
                            publish.payload.to_vec(),
                            &topic_prefix_for_task,
                            &routes,
                            &pending,
                            sink.clone(),
                        )
                        .await;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("dispatcher mqtt: event loop error (will retry): {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        self.mqtt = Some(Arc::new(MqttPeer {
            client,
            event_task,
            topic_prefix,
        }));

        Ok(())
    }

    /// Seed the route table with an explicit entry. Used for targets that
    /// don't announce themselves via `/info` (e.g. TCP-only peers discovered
    /// via a config file).
    pub fn add_static_route(&self, net_id: AmsNetId, kind: TransportKind) {
        self.routes.learn(net_id, kind);
    }

    /// Register an outbound TCP peer: associate an AMS Net ID with a dial
    /// target (`<host>:48898` typically). The TCP connection is dialed
    /// lazily on the first `send_request` that routes to this NetID.
    ///
    /// Calling this also learns the route as `TransportKind::Tcp`, so the
    /// next `send_request(net_id, ...)` will use TCP even if no `/info`
    /// announcement for that peer has been seen.
    pub async fn add_tcp_peer(&self, net_id: AmsNetId, addr: SocketAddr) {
        self.routes.learn(net_id, TransportKind::Tcp);
        self.tcp.lock().await.addrs.insert(net_id, addr);
    }

    /// Issue a request and wait for the matching response.
    ///
    /// Allocates a fresh invoke-id, builds the AMS header for `cmd`, publishes
    /// the frame via the transport that currently routes to `target.netid`,
    /// and awaits a response. Returns the response payload (stripped of the
    /// AMS header) on success.
    pub async fn send_request(
        &self,
        target_net_id: AmsNetId,
        target_port: u16,
        cmd: u16,
        payload: &[u8],
        timeout: Duration,
    ) -> std::result::Result<Vec<u8>, DispatcherError> {
        let transport = self
            .routes
            .get(target_net_id)
            .ok_or(DispatcherError::NoRoute(target_net_id))?;

        let invoke_id = self.invoke_counter.fetch_add(1, Ordering::Relaxed);
        let frame = build_ams_frame(
            target_net_id,
            target_port,
            self.source_net_id,
            self.source_port,
            cmd,
            /* state_flags */ 0x0004, // ADS command (request) flag
            invoke_id,
            payload,
        );

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(invoke_id, tx);

        // Publish on the right transport.
        let publish_result = match transport {
            TransportKind::Mqtt => self.publish_mqtt(target_net_id, frame).await,
            TransportKind::Tcp => self.publish_tcp(target_net_id, frame).await,
        };

        if let Err(e) = publish_result {
            self.pending.lock().await.remove(&invoke_id);
            return Err(e);
        }

        // Await the response or timeout.
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => {
                if resp.header.error_code != 0 {
                    Err(DispatcherError::Ads(AdsError::BufferError(format!(
                        "ADS error 0x{:x} from {}",
                        resp.header.error_code, resp.header.source_net_id
                    ))))
                } else {
                    Ok(resp.payload)
                }
            }
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&invoke_id);
                Err(DispatcherError::Closed)
            }
            Err(_) => {
                self.pending.lock().await.remove(&invoke_id);
                Err(DispatcherError::Timeout(timeout))
            }
        }
    }

    async fn publish_mqtt(
        &self,
        target: AmsNetId,
        frame: Vec<u8>,
    ) -> std::result::Result<(), DispatcherError> {
        let mqtt = self
            .mqtt
            .as_ref()
            .ok_or(DispatcherError::TransportNotAttached(TransportKind::Mqtt))?;
        let topic = mqtt.outbound_topic(target);
        mqtt.client
            .publish(topic, QoS::AtMostOnce, false, frame)
            .await
            .map_err(DispatcherError::from)
    }

    async fn publish_tcp(
        &self,
        target: AmsNetId,
        frame: Vec<u8>,
    ) -> std::result::Result<(), DispatcherError> {
        let peer = self.get_or_dial_tcp_peer(target).await?;
        let mut writer = peer.writer.lock().await;
        if let Err(e) = writer.write_all(&frame).await {
            // Connection broken — drop it so the next send redials.
            drop(writer);
            self.tcp.lock().await.peers.remove(&target);
            return Err(DispatcherError::Mqtt(format!("tcp write: {e}")));
        }
        Ok(())
    }

    async fn get_or_dial_tcp_peer(
        &self,
        target: AmsNetId,
    ) -> std::result::Result<Arc<TcpPeer>, DispatcherError> {
        // Fast path — connection already established.
        {
            let guard = self.tcp.lock().await;
            if let Some(peer) = guard.peers.get(&target) {
                return Ok(peer.clone());
            }
        }

        // Resolve the dial address (must be pre-registered).
        let addr = {
            let guard = self.tcp.lock().await;
            guard
                .addrs
                .get(&target)
                .copied()
                .ok_or(DispatcherError::NoRoute(target))?
        };

        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| DispatcherError::Mqtt(format!("tcp connect to {addr} failed: {e}")))?;
        // Disable Nagle — ADS traffic is request/response, we want small
        // frames out immediately.
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();

        let pending = self.pending.clone();
        let sink = self.inbound_sink.clone();
        let tcp_state = self.tcp.clone();
        let reader_task = tokio::spawn(run_tcp_reader(read_half, target, pending, sink, tcp_state));

        let peer = Arc::new(TcpPeer {
            writer: Arc::new(Mutex::new(write_half)),
            reader_task,
            addr,
        });
        self.tcp.lock().await.peers.insert(target, peer.clone());
        Ok(peer)
    }
}

/// Background task: parse AMS frames off a TCP socket and dispatch.
async fn run_tcp_reader(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    target: AmsNetId,
    pending: PendingMap,
    sink: Arc<dyn InboundSink>,
    tcp_state: Arc<Mutex<TcpState>>,
) {
    // The AMS/TCP wire format is: 6-byte TCP header (2 reserved + 4-byte
    // length) + AMS header (32 bytes) + payload. Read header first, then
    // the rest by declared length.
    let mut prefix = [0u8; 6];
    loop {
        match reader.read_exact(&mut prefix).await {
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(%target, "tcp reader: connection closed ({e})");
                break;
            }
        }
        let total_len = u32::from_le_bytes([prefix[2], prefix[3], prefix[4], prefix[5]]) as usize;
        if total_len == 0 || total_len > 16 * 1024 * 1024 {
            tracing::warn!(%target, total_len, "tcp reader: bogus frame length, closing");
            break;
        }
        let mut body = vec![0u8; total_len];
        if let Err(e) = reader.read_exact(&mut body).await {
            tracing::debug!(%target, "tcp reader: body read failed ({e})");
            break;
        }
        if body.len() < AMS_HEADER_LEN {
            tracing::warn!(%target, bytes = body.len(), "tcp reader: short AMS body");
            continue;
        }
        let header = match AmsHeader::parse(&body[..AMS_HEADER_LEN]) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(%target, "tcp reader: bad AMS header ({e})");
                continue;
            }
        };
        let payload = body[AMS_HEADER_LEN..].to_vec();

        let is_response = header.state_flags & AMS_STATE_RESPONSE != 0;
        if is_response {
            if let Some(waiter) = pending.lock().await.remove(&header.invoke_id) {
                let _ = waiter.send(ResponseFrame { header, payload });
                continue;
            }
            tracing::debug!(%target, invoke_id = header.invoke_id, "tcp reader: unmatched response");
            continue;
        }
        sink.deliver(header, payload);
    }

    // Reader exited — peer connection dead. Remove from live-peer map so the
    // next send_request redials.
    tcp_state.lock().await.peers.remove(&target);
}

/// Build a full AMS/TCP frame: 6-byte TCP prefix + 32-byte AMS header + payload.
#[allow(clippy::too_many_arguments)]
pub fn build_ams_frame(
    target_net_id: AmsNetId,
    target_port: u16,
    source_net_id: AmsNetId,
    source_port: u16,
    cmd: u16,
    state_flags: u16,
    invoke_id: u32,
    payload: &[u8],
) -> Vec<u8> {
    let header = AmsHeader {
        target_net_id,
        target_port,
        source_net_id,
        source_port,
        command_id: cmd,
        state_flags,
        data_length: payload.len() as u32,
        error_code: 0,
        invoke_id,
    };
    let hdr_bytes = header.serialize();
    let total_len = hdr_bytes.len() + payload.len();
    let mut out = Vec::with_capacity(6 + total_len);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(total_len as u32).to_le_bytes());
    out.extend_from_slice(&hdr_bytes);
    out.extend_from_slice(payload);
    out
}

/// Decode `/info` XML into `(peer_name, online)` if well-formed enough for
/// routing. Only the `online` attribute matters for the route table — any
/// other attributes (os, unidirectional, …) are preserved verbatim in the
/// topic but ignored here.
fn parse_info_xml(payload: &[u8]) -> Option<bool> {
    let s = std::str::from_utf8(payload).ok()?;
    // Very tolerant parsing: look for `>true<` or `>false<` as the element body.
    // The XML fragments TwinCAT emits are always the same shape — e.g.
    //   <info><online name="x" ...>true</online></info>
    // so a byte-level search is good enough for route learning.
    let lower = s.to_ascii_lowercase();
    if lower.contains(">true</online>") {
        Some(true)
    } else if lower.contains(">false</online>") {
        Some(false)
    } else {
        None
    }
}

/// Extract the middle NetID segment from a topic like
/// `AdsOverMqtt/10.1.2.3.1.1/info`. Returns `None` on malformed topics.
fn topic_net_id(topic: &str, prefix: &str) -> Option<AmsNetId> {
    let rest = topic.strip_prefix(prefix)?.strip_prefix('/')?;
    let (net_id, _) = rest.split_once('/')?;
    AmsNetId::from_str_ref(net_id).ok()
}

async fn handle_incoming_mqtt(
    topic: &str,
    payload: Vec<u8>,
    topic_prefix: &str,
    routes: &RouteTable,
    pending: &PendingMap,
    sink: Arc<dyn InboundSink>,
) {
    // 1. /info publications update the route table.
    if topic.ends_with("/info") {
        if let (Some(net_id), Some(online)) =
            (topic_net_id(topic, topic_prefix), parse_info_xml(&payload))
        {
            if online {
                routes.learn(net_id, TransportKind::Mqtt);
            } else {
                routes.forget(net_id);
            }
        }
        return;
    }

    // 2. AMS frames on `/ams/res` or on our own `/ams` topic. Parse and
    // dispatch by invoke-id if a waiter is registered, otherwise hand to the
    // inbound sink.
    if !topic.ends_with("/ams") && !topic.ends_with("/ams/res") {
        return;
    }
    if payload.len() < 6 + AMS_HEADER_LEN {
        tracing::debug!(
            "dispatcher mqtt: ignoring short frame on {} ({} bytes)",
            topic,
            payload.len()
        );
        return;
    }
    let ams_bytes = &payload[6..];
    let header = match AmsHeader::parse(ams_bytes) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!("dispatcher mqtt: bad AMS header on {}: {e}", topic);
            return;
        }
    };
    let body_start = AMS_HEADER_LEN;
    let body = ams_bytes[body_start..].to_vec();

    let is_response = header.state_flags & AMS_STATE_RESPONSE != 0;
    if is_response {
        if let Some(waiter) = pending.lock().await.remove(&header.invoke_id) {
            let _ = waiter.send(ResponseFrame {
                header,
                payload: body,
            });
            return;
        }
        // Unmatched response — drop with a debug log so we can audit later.
        tracing::debug!(
            "dispatcher mqtt: unmatched response invoke_id={} on {}",
            header.invoke_id,
            topic
        );
        return;
    }

    // Not a response — hand it to the observer sink.
    sink.deliver(header, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_learn_returns_true_for_new() {
        let rt = RouteTable::new();
        assert!(rt.learn(AmsNetId([1, 2, 3, 4, 5, 6]), TransportKind::Mqtt));
        assert!(!rt.learn(AmsNetId([1, 2, 3, 4, 5, 6]), TransportKind::Mqtt));
        assert!(rt.learn(AmsNetId([1, 2, 3, 4, 5, 6]), TransportKind::Tcp));
    }

    #[test]
    fn route_table_forget_returns_previous() {
        let rt = RouteTable::new();
        rt.learn(AmsNetId([1, 2, 3, 4, 5, 6]), TransportKind::Mqtt);
        assert_eq!(
            rt.forget(AmsNetId([1, 2, 3, 4, 5, 6])),
            Some(TransportKind::Mqtt)
        );
        assert_eq!(rt.forget(AmsNetId([1, 2, 3, 4, 5, 6])), None);
    }

    #[test]
    fn topic_net_id_parses_standard_shape() {
        let id = topic_net_id("AdsOverMqtt/10.1.2.3.1.1/info", "AdsOverMqtt");
        assert_eq!(id, Some(AmsNetId([10, 1, 2, 3, 1, 1])));
    }

    #[test]
    fn topic_net_id_rejects_malformed() {
        assert_eq!(topic_net_id("AdsOverMqtt/info", "AdsOverMqtt"), None);
        assert_eq!(
            topic_net_id("NopePrefix/10.1.2.3.1.1/info", "AdsOverMqtt"),
            None
        );
        assert_eq!(
            topic_net_id("AdsOverMqtt/not-a-netid/info", "AdsOverMqtt"),
            None
        );
    }

    #[test]
    fn parse_info_xml_online_true() {
        let xml = br#"<info><online name="x" osPlatform="0">true</online></info>"#;
        assert_eq!(parse_info_xml(xml), Some(true));
    }

    #[test]
    fn parse_info_xml_online_false() {
        let xml = br#"<info><online name="x">false</online></info>"#;
        assert_eq!(parse_info_xml(xml), Some(false));
    }

    #[test]
    fn parse_info_xml_rejects_junk() {
        assert_eq!(parse_info_xml(b"not xml"), None);
        assert_eq!(parse_info_xml(b"<info/>"), None);
    }

    #[test]
    fn build_ams_frame_shape() {
        let frame = build_ams_frame(
            AmsNetId([1, 2, 3, 4, 5, 6]),
            851,
            AmsNetId([10, 10, 10, 10, 1, 1]),
            16150,
            crate::ams::ADS_CMD_READ,
            0x0004,
            42,
            &[0xAA, 0xBB, 0xCC],
        );
        // 6-byte TCP prefix + 32-byte AMS header + 3-byte payload = 41 bytes
        assert_eq!(frame.len(), 6 + 32 + 3);
        assert_eq!(&frame[0..2], &[0, 0]);
        let total_len = u32::from_le_bytes([frame[2], frame[3], frame[4], frame[5]]);
        assert_eq!(total_len, 32 + 3);

        let header = AmsHeader::parse(&frame[6..]).unwrap();
        assert_eq!(header.target_net_id, AmsNetId([1, 2, 3, 4, 5, 6]));
        assert_eq!(header.target_port, 851);
        assert_eq!(header.source_net_id, AmsNetId([10, 10, 10, 10, 1, 1]));
        assert_eq!(header.source_port, 16150);
        assert_eq!(header.command_id, crate::ams::ADS_CMD_READ);
        assert_eq!(header.state_flags, 0x0004);
        assert_eq!(header.invoke_id, 42);
        assert_eq!(header.data_length, 3);
    }

    #[tokio::test]
    async fn send_request_without_mqtt_attached_errors_cleanly() {
        let disp = AmsDispatcher::new(AmsNetId([10, 10, 10, 10, 1, 1]), 16150);
        disp.add_static_route(AmsNetId([1, 2, 3, 4, 5, 6]), TransportKind::Mqtt);
        let err = disp
            .send_request(
                AmsNetId([1, 2, 3, 4, 5, 6]),
                851,
                crate::ams::ADS_CMD_READ,
                &[],
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DispatcherError::TransportNotAttached(TransportKind::Mqtt)
        ));
    }

    #[tokio::test]
    async fn send_request_to_tcp_route_without_addr_errors_cleanly() {
        // `add_static_route(netid, Tcp)` without a registered dial address
        // should fail fast with NoRoute rather than hang in dial.
        let disp = AmsDispatcher::new(AmsNetId([10, 10, 10, 10, 1, 1]), 16150);
        disp.add_static_route(AmsNetId([1, 2, 3, 4, 5, 6]), TransportKind::Tcp);
        let err = disp
            .send_request(
                AmsNetId([1, 2, 3, 4, 5, 6]),
                851,
                crate::ams::ADS_CMD_READ,
                &[],
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DispatcherError::NoRoute(_)));
    }

    #[tokio::test]
    async fn add_tcp_peer_learns_route_and_stores_addr() {
        let disp = AmsDispatcher::new(AmsNetId([10, 10, 10, 10, 1, 1]), 16150);
        let netid = AmsNetId([1, 2, 3, 4, 5, 6]);
        disp.add_tcp_peer(netid, "127.0.0.1:59999".parse().unwrap())
            .await;
        assert_eq!(disp.routes().get(netid), Some(TransportKind::Tcp));
        let addr = disp.tcp.lock().await.addrs.get(&netid).copied();
        assert_eq!(addr, Some("127.0.0.1:59999".parse().unwrap()));
    }

    #[tokio::test]
    async fn send_request_tcp_dial_failure_surfaces_error_not_hang() {
        // Pick a high unprivileged port almost certainly unused.
        let disp = AmsDispatcher::new(AmsNetId([10, 10, 10, 10, 1, 1]), 16150);
        let netid = AmsNetId([1, 2, 3, 4, 5, 6]);
        disp.add_tcp_peer(netid, "127.0.0.1:61237".parse().unwrap())
            .await;
        let err = disp
            .send_request(
                netid,
                851,
                crate::ams::ADS_CMD_READ,
                &[],
                Duration::from_secs(2),
            )
            .await
            .unwrap_err();
        // Connection error surfaces as Mqtt(String) — the general I/O
        // variant we reuse for both transports. What matters is that it's
        // an error rather than the `send_request` blocking forever.
        assert!(matches!(err, DispatcherError::Mqtt(_)));
    }

    #[tokio::test]
    async fn send_request_with_no_route_errors() {
        let disp = AmsDispatcher::new(AmsNetId([10, 10, 10, 10, 1, 1]), 16150);
        let err = disp
            .send_request(
                AmsNetId([1, 2, 3, 4, 5, 6]),
                851,
                crate::ams::ADS_CMD_READ,
                &[],
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DispatcherError::NoRoute(_)));
    }

    #[tokio::test]
    async fn handle_incoming_info_learns_then_forgets() {
        let routes = RouteTable::new();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let sink: Arc<dyn InboundSink> = Arc::new(NullSink);

        handle_incoming_mqtt(
            "AdsOverMqtt/10.1.2.3.1.1/info",
            br#"<info><online name="x">true</online></info>"#.to_vec(),
            "AdsOverMqtt",
            &routes,
            &pending,
            sink.clone(),
        )
        .await;
        assert_eq!(
            routes.get(AmsNetId([10, 1, 2, 3, 1, 1])),
            Some(TransportKind::Mqtt)
        );

        handle_incoming_mqtt(
            "AdsOverMqtt/10.1.2.3.1.1/info",
            br#"<info><online name="x">false</online></info>"#.to_vec(),
            "AdsOverMqtt",
            &routes,
            &pending,
            sink,
        )
        .await;
        assert_eq!(routes.get(AmsNetId([10, 1, 2, 3, 1, 1])), None);
    }

    #[tokio::test]
    async fn handle_incoming_response_resolves_waiter() {
        let routes = RouteTable::new();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let sink: Arc<dyn InboundSink> = Arc::new(NullSink);

        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(99, tx);

        // Build a response frame addressed to us with invoke_id=99 and the
        // response state-flag set.
        let mut header_bytes = AmsHeader {
            target_net_id: AmsNetId([10, 10, 10, 10, 1, 1]),
            target_port: 16150,
            source_net_id: AmsNetId([1, 2, 3, 4, 5, 6]),
            source_port: 851,
            command_id: crate::ams::ADS_CMD_READ,
            state_flags: AMS_STATE_RESPONSE,
            data_length: 2,
            error_code: 0,
            invoke_id: 99,
        }
        .serialize();
        header_bytes.push(0x11);
        header_bytes.push(0x22);

        let mut frame = Vec::new();
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&((header_bytes.len()) as u32).to_le_bytes());
        frame.extend_from_slice(&header_bytes);

        handle_incoming_mqtt(
            "AdsOverMqtt/10.10.10.10.1.1/ams/res",
            frame,
            "AdsOverMqtt",
            &routes,
            &pending,
            sink,
        )
        .await;

        let resp = tokio::time::timeout(Duration::from_millis(500), rx)
            .await
            .expect("timed out")
            .expect("waiter dropped");
        assert_eq!(resp.header.invoke_id, 99);
        assert_eq!(resp.payload, vec![0x11, 0x22]);
    }

    #[tokio::test]
    async fn handle_incoming_non_response_goes_to_sink() {
        use std::sync::atomic::AtomicUsize;

        struct CountSink(Arc<AtomicUsize>);
        impl InboundSink for CountSink {
            fn deliver(&self, _h: AmsHeader, _p: Vec<u8>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn InboundSink> = Arc::new(CountSink(counter.clone()));
        let routes = RouteTable::new();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // Request frame (state_flags = 0x0004 = ADS command, not response).
        let header = AmsHeader {
            target_net_id: AmsNetId([10, 10, 10, 10, 1, 1]),
            target_port: 16150,
            source_net_id: AmsNetId([1, 2, 3, 4, 5, 6]),
            source_port: 851,
            command_id: crate::ams::ADS_CMD_WRITE,
            state_flags: 0x0004,
            data_length: 0,
            error_code: 0,
            invoke_id: 1,
        }
        .serialize();
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&(header.len() as u32).to_le_bytes());
        frame.extend_from_slice(&header);

        handle_incoming_mqtt(
            "AdsOverMqtt/10.10.10.10.1.1/ams",
            frame,
            "AdsOverMqtt",
            &routes,
            &pending,
            sink,
        )
        .await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
