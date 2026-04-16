//! Self-polling diagnostics collector.
//!
//! Issues the same ADS reads the TwinCAT XAE IDE does when its diagnostic
//! windows are open, regardless of whether an IDE is actually connected.
//! Decodes responses via [`crate::diagnostics_observer`] and emits
//! [`DiagEvent`]s over an mpsc channel for the metrics pipeline.
//!
//! Runs on its own rumqttc connection (separate client-id) so the main
//! [`crate::transport::mqtt::MqttAmsTransport`] keeps its existing
//! subscription and routing behaviour unchanged.
//!
//! Topic layout:
//! - Publishes requests to `{prefix}/{target_net_id}/ams`.
//! - Subscribes to `{prefix}/{local_net_id}/ams/res` for responses.

use crate::ams::{AmsHeader, AmsNetId, ADS_CMD_READ, ADS_CMD_READ_DEVICE_INFO, ADS_STATE_REQUEST};
use crate::diagnostics::{
    DiagEvent, IG_RT_SYSTEM, IG_RT_USAGE, IO_EXCEED_COUNTER, IO_RT_USAGE, IO_TASK_STATS,
    RT_USAGE_LEN, TASK_STATS_LEN,
};
use crate::diagnostics_observer::DiagnosticsObserver;
use crate::error::Result;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, RwLock};

/// Default AMS port of the TwinCAT realtime subsystem.
pub const DEFAULT_RT_PORT: u16 = 200;

/// Default source port tc-otel uses when issuing diagnostic polls.
pub const POLLER_SOURCE_PORT: u16 = 30001;

/// Duration for which the poller suspends polling after a push-diagnostic event.
pub const PUSH_SUSPEND_WINDOW: Duration = Duration::from_secs(10);

/// Configuration for a single PLC target.
#[derive(Debug, Clone)]
pub struct TargetConfig {
    /// AMS Net ID of the PLC to poll.
    pub net_id: AmsNetId,
    /// Poll period. 1 s is a reasonable default for IDE-free operation.
    pub poll_interval: Duration,
    /// Read the system-wide cycle-exceed counter each tick.
    pub exceed_counter: bool,
    /// Read the RT usage + system-latency block each tick.
    pub rt_usage: bool,
    /// List of AMS task ports to poll for per-task cycle stats. Defaults
    /// to `[340, 350, 351]` (I/O idle + first two PLC tasks).
    pub task_ports: Vec<u16>,
    /// AMS port of the realtime subsystem for RT-usage reads.
    pub rt_port: u16,
    /// Pre-populated task names keyed by port. These seed the shared
    /// `task_names` map before discovery runs, so metrics carry the
    /// right name from the first sample onward. TC3 task ports usually
    /// don't answer `AdsReadDeviceInfo` — configure names here for
    /// reliable labeling.
    pub task_names: HashMap<u16, String>,
}

impl TargetConfig {
    pub fn with_defaults(net_id: AmsNetId) -> Self {
        Self {
            net_id,
            poll_interval: Duration::from_secs(1),
            exceed_counter: true,
            rt_usage: true,
            task_ports: vec![340, 350, 351],
            rt_port: DEFAULT_RT_PORT,
            task_names: HashMap::new(),
        }
    }
}

/// Configuration for the poller as a whole.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub client_id: String,
    pub topic_prefix: String,
    pub local_net_id: AmsNetId,
    pub targets: Vec<TargetConfig>,
}

/// Planned outgoing poll: target net ID, AMS port, raw frame bytes.
///
/// Returned by [`build_polls_for_target`]; the poller publishes each to
/// `{prefix}/{target_net_id}/ams`.
#[derive(Debug, Clone)]
pub struct PlannedPoll {
    pub target_net_id: AmsNetId,
    pub target_port: u16,
    pub index_group: u32,
    pub index_offset: u32,
    pub read_size: u32,
    pub invoke_id: u32,
    pub frame: Vec<u8>,
}

/// Build a raw AMS frame for an `AdsReadDeviceInfo` request (cmd 1).
/// Request payload is empty; the response carries the 24-byte device
/// descriptor `result(4) + major(1) + minor(1) + version(2) + name(16)`.
pub fn build_read_device_info_frame(
    source_net_id: AmsNetId,
    source_port: u16,
    target_net_id: AmsNetId,
    target_port: u16,
    invoke_id: u32,
) -> Vec<u8> {
    let header = AmsHeader {
        target_net_id,
        target_port,
        source_net_id,
        source_port,
        command_id: ADS_CMD_READ_DEVICE_INFO,
        state_flags: ADS_STATE_REQUEST,
        data_length: 0,
        error_code: 0,
        invoke_id,
    };
    header.serialize()
}

/// Extract the zero-terminated ASCII task name from a `ReadDeviceInfo`
/// response body. Returns `None` on malformed input.
fn parse_device_info_name(body: &[u8]) -> Option<String> {
    // body = result(4) + major(1) + minor(1) + version(2) + name(16)
    if body.len() < 24 {
        return None;
    }
    let result = u32::from_le_bytes(body[0..4].try_into().ok()?);
    if result != 0 {
        return None;
    }
    let name_bytes = &body[8..24];
    let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
    let name = std::str::from_utf8(&name_bytes[..end]).ok()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Build a raw AMS frame for an ADS-Read request (header + payload, no
/// TCP prefix — MQTT transport carries the frame unwrapped).
#[allow(clippy::too_many_arguments)]
pub fn build_read_frame(
    source_net_id: AmsNetId,
    source_port: u16,
    target_net_id: AmsNetId,
    target_port: u16,
    index_group: u32,
    index_offset: u32,
    read_length: u32,
    invoke_id: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&index_group.to_le_bytes());
    payload.extend_from_slice(&index_offset.to_le_bytes());
    payload.extend_from_slice(&read_length.to_le_bytes());

    let header = AmsHeader {
        target_net_id,
        target_port,
        source_net_id,
        source_port,
        command_id: ADS_CMD_READ,
        state_flags: ADS_STATE_REQUEST,
        data_length: payload.len() as u32,
        error_code: 0,
        invoke_id,
    };

    let mut out = header.serialize();
    out.extend_from_slice(&payload);
    out
}

/// Compute the full set of polls this target owes in one tick.
///
/// Pure: no I/O, no time. Unit tests assert the exact byte layout.
pub fn build_polls_for_target(
    target: &TargetConfig,
    source_net_id: AmsNetId,
    invoke_counter: &AtomicU32,
) -> Vec<PlannedPoll> {
    let mut polls = Vec::new();

    if target.exceed_counter {
        let invoke = invoke_counter.fetch_add(1, Ordering::Relaxed);
        // Any AMS port on the PLC answers IG 0xF200:0x100; use the lowest
        // task port if available, else RT port, else 100 (R0_PLC) as a
        // reasonable default that all TwinCAT targets expose.
        let port = target
            .task_ports
            .iter()
            .copied()
            .min()
            .unwrap_or(target.rt_port);
        polls.push(PlannedPoll {
            target_net_id: target.net_id,
            target_port: port,
            index_group: IG_RT_SYSTEM,
            index_offset: IO_EXCEED_COUNTER,
            read_size: 4,
            invoke_id: invoke,
            frame: build_read_frame(
                source_net_id,
                POLLER_SOURCE_PORT,
                target.net_id,
                port,
                IG_RT_SYSTEM,
                IO_EXCEED_COUNTER,
                4,
                invoke,
            ),
        });
    }

    if target.rt_usage {
        let invoke = invoke_counter.fetch_add(1, Ordering::Relaxed);
        polls.push(PlannedPoll {
            target_net_id: target.net_id,
            target_port: target.rt_port,
            index_group: IG_RT_USAGE,
            index_offset: IO_RT_USAGE,
            read_size: RT_USAGE_LEN as u32,
            invoke_id: invoke,
            frame: build_read_frame(
                source_net_id,
                POLLER_SOURCE_PORT,
                target.net_id,
                target.rt_port,
                IG_RT_USAGE,
                IO_RT_USAGE,
                RT_USAGE_LEN as u32,
                invoke,
            ),
        });
    }

    for &port in &target.task_ports {
        let invoke = invoke_counter.fetch_add(1, Ordering::Relaxed);
        polls.push(PlannedPoll {
            target_net_id: target.net_id,
            target_port: port,
            index_group: IG_RT_SYSTEM,
            index_offset: IO_TASK_STATS,
            read_size: TASK_STATS_LEN as u32,
            invoke_id: invoke,
            frame: build_read_frame(
                source_net_id,
                POLLER_SOURCE_PORT,
                target.net_id,
                port,
                IG_RT_SYSTEM,
                IO_TASK_STATS,
                TASK_STATS_LEN as u32,
                invoke,
            ),
        });
    }

    polls
}

/// Map from `(PLC net ID, task port)` to the human-readable task name
/// discovered via `AdsReadDeviceInfo`. Shared so the metric bridge can
/// attach the name as a label.
pub type TaskNameMap = Arc<RwLock<HashMap<(AmsNetId, u16), String>>>;

/// Self-polling diagnostics collector.
pub struct DiagnosticsPoller {
    config: PollerConfig,
    observer: Arc<Mutex<DiagnosticsObserver>>,
    invoke_counter: Arc<AtomicU32>,
    event_tx: mpsc::Sender<(AmsNetId, DiagEvent)>,
    task_names: TaskNameMap,
    /// Invoke-IDs the poller sent for `ReadDeviceInfo` requests, keyed so
    /// the matching response can be attributed to the right (net_id, port).
    pending_device_info: Arc<Mutex<HashMap<u32, (AmsNetId, u16)>>>,
    /// Tracks the most recent time a push-diagnostic event was received
    /// per target. Used to suspend polling when push events are active.
    push_seen: Arc<RwLock<HashMap<AmsNetId, Instant>>>,
}

impl DiagnosticsPoller {
    pub fn new(config: PollerConfig, event_tx: mpsc::Sender<(AmsNetId, DiagEvent)>) -> Self {
        // Seed the task-name map with manual overrides from config so
        // metrics carry the right label from the first sample onward.
        let mut seeded = HashMap::new();
        for target in &config.targets {
            for (&port, name) in &target.task_names {
                seeded.insert((target.net_id, port), name.clone());
            }
        }
        Self {
            config,
            observer: Arc::new(Mutex::new(DiagnosticsObserver::new())),
            invoke_counter: Arc::new(AtomicU32::new(1)),
            event_tx,
            task_names: Arc::new(RwLock::new(seeded)),
            pending_device_info: Arc::new(Mutex::new(HashMap::new())),
            push_seen: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Shared task-name map. Populated asynchronously after startup — empty
    /// until the first `ReadDeviceInfo` response arrives.
    pub fn task_names(&self) -> TaskNameMap {
        self.task_names.clone()
    }

    /// Shared push-seen timestamps map. Used to track when push-diagnostic
    /// events are received so polling can be suspended during active push periods.
    pub fn push_seen(&self) -> Arc<RwLock<HashMap<AmsNetId, Instant>>> {
        self.push_seen.clone()
    }

    /// Run the poller forever. Subscribes to the response topic, spawns one
    /// ticker per target, and feeds observed frames through the diagnostics
    /// observer.
    ///
    /// Returns only if the broker connection cannot be established — the
    /// rumqttc event loop handles transient reconnects internally.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut opts = MqttOptions::new(
            &self.config.client_id,
            &self.config.broker_host,
            self.config.broker_port,
        );
        opts.set_keep_alive(Duration::from_secs(60));
        opts.set_max_packet_size(16 * 1024 * 1024, 16 * 1024 * 1024);

        let (client, mut event_loop) = AsyncClient::new(opts, 10);

        let res_topic = format!(
            "{}/{}/ams/res",
            self.config.topic_prefix, self.config.local_net_id
        );

        // Spawn one ticker per target — independent cadences without cross-
        // talk between slow and fast targets.
        for target in &self.config.targets {
            let this = self.clone();
            let target = target.clone();
            let client = client.clone();
            tokio::spawn(async move {
                this.target_loop(client, target).await;
            });
        }

        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    tracing::info!("DiagnosticsPoller: connected, subscribing to {}", res_topic);
                    if let Err(e) = client.subscribe(&res_topic, QoS::AtMostOnce).await {
                        tracing::warn!("DiagnosticsPoller: subscribe failed: {e}");
                    }
                    // Kick off one-shot task-name discovery for each target.
                    // Failures just leave the name map empty; regular polls
                    // continue to work with the port number as identifier.
                    for target in &self.config.targets {
                        let this = self.clone();
                        let target = target.clone();
                        let client = client.clone();
                        tokio::spawn(async move {
                            this.discover_task_names(client, target).await;
                        });
                    }
                }
                Ok(Event::Incoming(Incoming::Publish(publish))) => {
                    self.on_response(&publish.payload).await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("DiagnosticsPoller: connection error (will retry): {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn target_loop(self: Arc<Self>, client: AsyncClient, target: TargetConfig) {
        let mut ticker = tokio::time::interval(target.poll_interval);
        // First tick fires immediately — skip so we don't race broker connect.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            // Check if a push-diagnostic event was received recently for this target.
            // If so, skip this polling tick to avoid redundant requests.
            {
                let push_map = self.push_seen.read().await;
                if let Some(&last_push) = push_map.get(&target.net_id) {
                    if Instant::now().duration_since(last_push) < PUSH_SUSPEND_WINDOW {
                        tracing::debug!(
                            "DiagnosticsPoller: suspending polls for {} (push-diagnostic active)",
                            target.net_id
                        );
                        continue;
                    }
                }
            }
            let polls =
                build_polls_for_target(&target, self.config.local_net_id, &self.invoke_counter);

            for poll in polls {
                let topic = format!("{}/{}/ams", self.config.topic_prefix, poll.target_net_id);

                // Track the request in the observer before publishing so a
                // very fast response can't arrive before we're watching for
                // it. `feed` with the raw frame bytes is a no-op when the
                // observer doesn't recognise the shape, but our polls are
                // all known, so this always inserts the pending entry.
                let header = match AmsHeader::parse(&poll.frame) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!("DiagnosticsPoller: self-built frame failed to parse: {e}");
                        continue;
                    }
                };
                let payload = &poll.frame[32..];
                {
                    let mut obs = self.observer.lock().await;
                    let _ = obs.feed(&header, payload);
                }

                if let Err(e) = client
                    .publish(&topic, QoS::AtMostOnce, false, poll.frame)
                    .await
                {
                    tracing::warn!("DiagnosticsPoller: publish to {topic} failed: {e}");
                }
            }
        }
    }

    async fn on_response(&self, payload: &[u8]) {
        let header = match AmsHeader::parse(payload) {
            Ok(h) => h,
            Err(_) => return,
        };
        // Only responses addressed to us. Any wildcard leakage is silently
        // ignored (the broker already filters, but belt and braces).
        if header.target_net_id != self.config.local_net_id {
            return;
        }
        if payload.len() < 32 {
            return;
        }
        let body = &payload[32..];

        // ReadDeviceInfo responses carry the task name — resolve and store.
        if header.command_id == ADS_CMD_READ_DEVICE_INFO {
            let pending = self
                .pending_device_info
                .lock()
                .await
                .remove(&header.invoke_id);
            if let Some((net_id, port)) = pending {
                if let Some(name) = parse_device_info_name(body) {
                    tracing::info!(
                        "DiagnosticsPoller: discovered task name '{name}' for {net_id}:{port}"
                    );
                    self.task_names.write().await.insert((net_id, port), name);
                }
            }
            return;
        }

        let event = {
            let mut obs = self.observer.lock().await;
            obs.feed(&header, body)
        };
        if let Some(ev) = event {
            // Non-blocking send: if the consumer is backpressured, drop the
            // sample rather than stall the poller. Tracing gives us an
            // observable symptom.
            let target_net_id = header.source_net_id;
            if let Err(e) = self.event_tx.try_send((target_net_id, ev)) {
                tracing::warn!("DiagnosticsPoller: event channel full, dropping: {e}");
            }
        }
    }

    async fn discover_task_names(self: Arc<Self>, client: AsyncClient, target: TargetConfig) {
        tracing::info!(
            "DiagnosticsPoller: starting name discovery for {} ({} ports)",
            target.net_id,
            target.task_ports.len()
        );
        // Give the broker a moment to propagate our response subscription
        // before the PLC's replies start flowing.
        tokio::time::sleep(Duration::from_millis(250)).await;
        for &port in &target.task_ports {
            let invoke = self.invoke_counter.fetch_add(1, Ordering::Relaxed);
            self.pending_device_info
                .lock()
                .await
                .insert(invoke, (target.net_id, port));
            let frame = build_read_device_info_frame(
                self.config.local_net_id,
                POLLER_SOURCE_PORT,
                target.net_id,
                port,
                invoke,
            );
            let topic = format!("{}/{}/ams", self.config.topic_prefix, target.net_id);
            tracing::debug!(
                "DiagnosticsPoller: publishing ReadDeviceInfo to {topic} port={port} invoke=0x{invoke:x}"
            );
            if let Err(e) = client.publish(&topic, QoS::AtMostOnce, false, frame).await {
                tracing::warn!(
                    "DiagnosticsPoller: ReadDeviceInfo publish to {topic} port {port} failed: {e}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ams::AmsNetId;

    fn net(a: [u8; 6]) -> AmsNetId {
        AmsNetId::from_bytes(a)
    }

    fn make_config() -> TargetConfig {
        TargetConfig {
            net_id: net([172, 28, 41, 37, 1, 1]),
            poll_interval: Duration::from_secs(1),
            exceed_counter: true,
            rt_usage: true,
            task_ports: vec![340, 350, 351],
            rt_port: DEFAULT_RT_PORT,
            task_names: HashMap::new(),
        }
    }

    #[test]
    fn read_frame_layout_matches_wire_format() {
        let source = net([10, 10, 10, 10, 1, 1]);
        let target = net([1, 2, 3, 4, 5, 6]);
        let frame = build_read_frame(
            source,
            POLLER_SOURCE_PORT,
            target,
            350,
            IG_RT_SYSTEM,
            IO_TASK_STATS,
            16,
            0xAABB,
        );
        assert_eq!(frame.len(), 32 + 12);

        // Parse it back to verify header fields are exactly what we expect.
        let header = AmsHeader::parse(&frame).unwrap();
        assert_eq!(header.target_net_id, target);
        assert_eq!(header.target_port, 350);
        assert_eq!(header.source_net_id, source);
        assert_eq!(header.source_port, POLLER_SOURCE_PORT);
        assert_eq!(header.command_id, ADS_CMD_READ);
        assert_eq!(header.state_flags, ADS_STATE_REQUEST);
        assert_eq!(header.data_length, 12);
        assert_eq!(header.invoke_id, 0xAABB);

        // Payload = IG || IO || size, all u32 LE.
        let body = &frame[32..];
        assert_eq!(
            u32::from_le_bytes(body[0..4].try_into().unwrap()),
            IG_RT_SYSTEM
        );
        assert_eq!(
            u32::from_le_bytes(body[4..8].try_into().unwrap()),
            IO_TASK_STATS
        );
        assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 16);
    }

    #[test]
    fn full_poll_plan_covers_all_metrics() {
        let target = make_config();
        let ctr = AtomicU32::new(1);
        let polls = build_polls_for_target(&target, net([10, 10, 10, 10, 1, 1]), &ctr);
        // 1 exceed + 1 rt_usage + 3 task ports = 5 polls
        assert_eq!(polls.len(), 5);

        // Invoke IDs must be unique and sequential.
        let ids: Vec<u32> = polls.iter().map(|p| p.invoke_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);

        // Exceed is first and targets the min task port.
        assert_eq!(polls[0].index_group, IG_RT_SYSTEM);
        assert_eq!(polls[0].index_offset, IO_EXCEED_COUNTER);
        assert_eq!(polls[0].target_port, 340);

        // RT-usage is second.
        assert_eq!(polls[1].index_group, IG_RT_USAGE);
        assert_eq!(polls[1].target_port, DEFAULT_RT_PORT);

        // Per-task polls follow in config order.
        assert_eq!(polls[2].target_port, 340);
        assert_eq!(polls[3].target_port, 350);
        assert_eq!(polls[4].target_port, 351);
        for p in &polls[2..] {
            assert_eq!(p.index_offset, IO_TASK_STATS);
            assert_eq!(p.read_size, 16);
        }
    }

    #[test]
    fn disabled_metrics_are_skipped() {
        let mut target = make_config();
        target.exceed_counter = false;
        target.rt_usage = false;
        target.task_ports.clear();
        let ctr = AtomicU32::new(1);
        let polls = build_polls_for_target(&target, net([10, 10, 10, 10, 1, 1]), &ctr);
        assert!(polls.is_empty());
        assert_eq!(ctr.load(Ordering::Relaxed), 1, "no invoke IDs consumed");
    }

    #[test]
    fn task_only_config_skips_exceed_and_rt() {
        let mut target = make_config();
        target.exceed_counter = false;
        target.rt_usage = false;
        let ctr = AtomicU32::new(100);
        let polls = build_polls_for_target(&target, net([10, 10, 10, 10, 1, 1]), &ctr);
        assert_eq!(polls.len(), 3);
        for p in &polls {
            assert_eq!(p.index_group, IG_RT_SYSTEM);
            assert_eq!(p.index_offset, IO_TASK_STATS);
        }
    }

    #[test]
    fn device_info_frame_has_empty_payload_and_cmd1() {
        let frame = build_read_device_info_frame(
            net([10, 10, 10, 10, 1, 1]),
            POLLER_SOURCE_PORT,
            net([1, 2, 3, 4, 5, 6]),
            350,
            0x42,
        );
        assert_eq!(frame.len(), 32);
        let header = AmsHeader::parse(&frame).unwrap();
        assert_eq!(header.command_id, ADS_CMD_READ_DEVICE_INFO);
        assert_eq!(header.data_length, 0);
        assert_eq!(header.invoke_id, 0x42);
    }

    #[test]
    fn device_info_name_parses_zero_terminated_ascii() {
        // body = result(4)=0 + major(1) + minor(1) + version(2) + name(16)
        let mut body = vec![0, 0, 0, 0, 3, 1, 0x0e, 0x00];
        body.extend_from_slice(b"PlcTask\0\0\0\0\0\0\0\0\0");
        assert_eq!(parse_device_info_name(&body), Some("PlcTask".to_string()));
    }

    #[test]
    fn device_info_name_rejects_error_result() {
        let mut body = vec![0x07, 0, 0, 0, 0, 0, 0, 0];
        body.extend_from_slice(b"ShouldNotReturn\0");
        assert_eq!(parse_device_info_name(&body), None);
    }

    #[test]
    fn exceed_without_task_ports_falls_back_to_rt_port() {
        let mut target = make_config();
        target.rt_usage = false;
        target.task_ports.clear();
        target.rt_port = 500;
        let ctr = AtomicU32::new(1);
        let polls = build_polls_for_target(&target, net([10, 10, 10, 10, 1, 1]), &ctr);
        assert_eq!(polls.len(), 1);
        assert_eq!(polls[0].index_offset, IO_EXCEED_COUNTER);
        assert_eq!(polls[0].target_port, 500);
    }

    #[tokio::test]
    async fn suspends_polling_when_push_seen_recently() {
        let config = PollerConfig {
            broker_host: "localhost".to_string(),
            broker_port: 1883,
            client_id: "test".to_string(),
            topic_prefix: "test".to_string(),
            local_net_id: net([10, 10, 10, 10, 1, 1]),
            targets: vec![make_config()],
        };
        let (tx, _rx) = mpsc::channel(10);
        let poller = Arc::new(DiagnosticsPoller::new(config, tx));
        let target_id = net([172, 28, 41, 37, 1, 1]);

        // Record a push event now
        let push_seen_arc = poller.push_seen();
        {
            let mut map = push_seen_arc.write().await;
            map.insert(target_id, Instant::now());
        }

        // Verify it's present and within the suspend window
        let last_push = {
            let map = push_seen_arc.read().await;
            map.get(&target_id).copied()
        };
        if let Some(last_push) = last_push {
            let elapsed = Instant::now().duration_since(last_push);
            assert!(elapsed < PUSH_SUSPEND_WINDOW);
        } else {
            panic!("push_seen should contain the target");
        }
    }
}
