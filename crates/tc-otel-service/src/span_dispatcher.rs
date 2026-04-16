//! Distributed trace span dispatcher — aggregates trace wire events into completed spans

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tc_otel_ads::{AmsNetId, AttrValue, TraceWireEvent};
use tc_otel_core::{SpanStatusCode, TraceRecord};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Span attributes from the wire
#[derive(Debug, Clone)]
pub struct SpanEvent {
    pub time: DateTime<Utc>,
    pub name: String,
    pub attrs: Vec<(String, AttrValue)>,
}

/// Unique identifier for a pending span
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanKey {
    #[allow(dead_code)]
    pub ams_net_id: AmsNetId,
    #[allow(dead_code)]
    pub task_index: u8,
    pub local_id: u8,
}

/// A span that is still being built (not yet ended)
#[derive(Debug, Clone)]
pub struct PendingSpan {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    pub name: String,
    pub kind: u8,
    pub start_time: DateTime<Utc>,
    pub attrs: HashMap<String, AttrValue>,
    pub events: Vec<SpanEvent>,
    pub deadline: Instant,
    #[allow(dead_code)]
    pub ams_net_id: AmsNetId,
    #[allow(dead_code)]
    pub task_index: u8,
}

/// Dispatcher that processes trace wire events and produces completed spans
pub struct SpanDispatcher {
    pending: HashMap<SpanKey, PendingSpan>,
    trace_tx: mpsc::Sender<TraceRecord>,
    span_ttl: Duration,
    max_pending: usize,
}

impl SpanDispatcher {
    /// Create a new span dispatcher
    pub fn new(
        trace_tx: mpsc::Sender<TraceRecord>,
        span_ttl: Duration,
        max_pending: usize,
    ) -> Self {
        Self {
            pending: HashMap::new(),
            trace_tx,
            span_ttl,
            max_pending,
        }
    }

    /// Process an incoming trace wire event
    pub fn on_event(&mut self, net_id: AmsNetId, ev: TraceWireEvent) {
        match ev {
            TraceWireEvent::Begin {
                local_id,
                task_index,
                flags: _,
                dc_time,
                parent_local_id,
                kind,
                name,
                traceparent,
            } => {
                self.on_begin(
                    net_id,
                    local_id,
                    task_index,
                    dc_time,
                    parent_local_id,
                    kind,
                    name,
                    traceparent,
                );
            }
            TraceWireEvent::Attr {
                local_id,
                task_index,
                flags: _,
                dc_time: _,
                key,
                value,
            } => {
                self.on_attr(net_id, local_id, task_index, key, value);
            }
            TraceWireEvent::Event {
                local_id,
                task_index,
                flags: _,
                dc_time,
                name,
                attrs,
            } => {
                self.on_event_ev(net_id, local_id, task_index, dc_time, name, attrs);
            }
            TraceWireEvent::End {
                local_id,
                task_index,
                flags: _,
                dc_time,
                status,
                message,
            } => {
                self.on_end(net_id, local_id, task_index, dc_time, status, message);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_begin(
        &mut self,
        net_id: AmsNetId,
        local_id: u8,
        task_index: u8,
        dc_time: i64,
        parent_local_id: u8,
        kind: u8,
        name: String,
        traceparent: Option<String>,
    ) {
        let key = SpanKey {
            ams_net_id: net_id,
            task_index,
            local_id,
        };

        // Determine trace_id and parent_span_id
        let (trace_id, parent_span_id) = if let Some(tp) = &traceparent {
            match parse_w3c_traceparent(tp) {
                Some((tid, sid)) => (tid, Some(sid)),
                None => {
                    tracing::warn!("Failed to parse W3C traceparent: {}", tp);
                    (self.new_trace_id(), None)
                }
            }
        } else if parent_local_id != 0xFF {
            // Try to find parent by local_id
            let parent_key = SpanKey {
                ams_net_id: net_id,
                task_index,
                local_id: parent_local_id,
            };
            match self.pending.get(&parent_key) {
                Some(p) => (p.trace_id, Some(p.span_id)),
                None => {
                    tracing::warn!(
                        "parent span not found for local_id {}, treating as root",
                        parent_local_id
                    );
                    (self.new_trace_id(), None)
                }
            }
        } else {
            // Root span
            (self.new_trace_id(), None)
        };

        let span_id = self.new_span_id();

        // Check capacity and evict if needed
        if self.pending.len() >= self.max_pending {
            self.evict_oldest();
        }

        // If key already exists, flush stale entry
        if let Some(stale) = self.pending.remove(&key) {
            self.finalise_timed_out(stale);
        }

        let pending = PendingSpan {
            trace_id,
            span_id,
            parent_span_id,
            name,
            kind,
            start_time: dc_time_to_datetime(dc_time),
            attrs: HashMap::new(),
            events: Vec::new(),
            deadline: Instant::now() + self.span_ttl,
            ams_net_id: net_id,
            task_index,
        };

        self.pending.insert(key, pending);
    }

    fn on_attr(
        &mut self,
        net_id: AmsNetId,
        local_id: u8,
        task_index: u8,
        key: String,
        value: AttrValue,
    ) {
        let span_key = SpanKey {
            ams_net_id: net_id,
            task_index,
            local_id,
        };

        if let Some(pending) = self.pending.get_mut(&span_key) {
            pending.attrs.insert(key, value);
        } else {
            tracing::debug!(
                "SPAN_ATTR for unknown span {}:{}:{}",
                net_id,
                task_index,
                local_id
            );
        }
    }

    fn on_event_ev(
        &mut self,
        net_id: AmsNetId,
        local_id: u8,
        task_index: u8,
        dc_time: i64,
        name: String,
        attrs: Vec<(String, AttrValue)>,
    ) {
        let span_key = SpanKey {
            ams_net_id: net_id,
            task_index,
            local_id,
        };

        if let Some(pending) = self.pending.get_mut(&span_key) {
            pending.events.push(SpanEvent {
                time: dc_time_to_datetime(dc_time),
                name,
                attrs,
            });
        } else {
            tracing::debug!(
                "SPAN_EVENT for unknown span {}:{}:{}",
                net_id,
                task_index,
                local_id
            );
        }
    }

    fn on_end(
        &mut self,
        net_id: AmsNetId,
        local_id: u8,
        task_index: u8,
        dc_time: i64,
        status: u8,
        message: String,
    ) {
        let span_key = SpanKey {
            ams_net_id: net_id,
            task_index,
            local_id,
        };

        if let Some(pending) = self.pending.remove(&span_key) {
            let end_time = dc_time_to_datetime(dc_time);
            let status_code = match status {
                0 => SpanStatusCode::Unset,
                1 => SpanStatusCode::Ok,
                2 => SpanStatusCode::Error,
                _ => {
                    tracing::warn!("Unknown span status code: {}", status);
                    SpanStatusCode::Unset
                }
            };

            self.finalise(pending, end_time, status_code, message);
        } else {
            tracing::debug!(
                "SPAN_END for unknown span {}:{}:{}",
                net_id,
                task_index,
                local_id
            );
        }
    }

    fn finalise(
        &self,
        pending: PendingSpan,
        end_time: DateTime<Utc>,
        status_code: SpanStatusCode,
        status_message: String,
    ) {
        let trace_record = TraceRecord {
            trace_id: hex::encode(pending.trace_id),
            span_id: hex::encode(pending.span_id),
            parent_span_id: pending.parent_span_id.map(hex::encode).unwrap_or_default(),
            name: pending.name,
            kind: pending.kind as i32,
            start_time: pending.start_time,
            end_time,
            status_code: status_code as i32,
            status_message,
            resource_attributes: HashMap::new(),
            scope_attributes: HashMap::new(),
            span_attributes: pending
                .attrs
                .into_iter()
                .map(|(k, v)| {
                    let json_val = match v {
                        AttrValue::I64(n) => serde_json::json!(n),
                        AttrValue::F64(f) => serde_json::json!(f),
                        AttrValue::Bool(b) => serde_json::json!(b),
                        AttrValue::String(s) => serde_json::json!(s),
                    };
                    (k, json_val)
                })
                .collect(),
            events: pending
                .events
                .into_iter()
                .map(|ev| tc_otel_core::TraceEventRecord {
                    timestamp: ev.time,
                    name: ev.name,
                    attributes: ev
                        .attrs
                        .into_iter()
                        .map(|(k, v)| {
                            let json_val = match v {
                                AttrValue::I64(n) => serde_json::json!(n),
                                AttrValue::F64(f) => serde_json::json!(f),
                                AttrValue::Bool(b) => serde_json::json!(b),
                                AttrValue::String(s) => serde_json::json!(s),
                            };
                            (k, json_val)
                        })
                        .collect(),
                })
                .collect(),
        };

        let _ = self.trace_tx.try_send(trace_record);
    }

    fn finalise_timed_out(&self, pending: PendingSpan) {
        let elapsed = Instant::now() - (pending.deadline - self.span_ttl);
        let elapsed_secs = elapsed.as_secs();
        let msg = format!(
            "span did not end within {} seconds",
            self.span_ttl.as_secs()
        );
        tracing::warn!("Span timed out after {:.1}s: {}", elapsed_secs, msg);
        self.finalise(pending, Utc::now(), SpanStatusCode::Error, msg);
    }

    fn evict_oldest(&mut self) {
        let oldest_key = self
            .pending
            .iter()
            .min_by_key(|(_, p)| p.deadline)
            .map(|(k, _)| *k);

        if let Some(key) = oldest_key {
            if let Some(pending) = self.pending.remove(&key) {
                self.finalise_timed_out(pending);
            }
        }
    }

    pub fn sweep_timed_out(&mut self) {
        let now = Instant::now();
        let expired: Vec<SpanKey> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| *k)
            .collect();

        for key in expired {
            if let Some(pending) = self.pending.remove(&key) {
                self.finalise_timed_out(pending);
            }
        }
    }

    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn new_trace_id(&self) -> [u8; 16] {
        *Uuid::new_v4().as_bytes()
    }

    fn new_span_id(&self) -> [u8; 8] {
        let uuid = Uuid::new_v4();
        let uuid_bytes = uuid.as_bytes();
        let mut id = [0u8; 8];
        id.copy_from_slice(&uuid_bytes[0..8]);
        id
    }
}

/// Parse W3C traceparent format: 00-<32hex>-<16hex>-<2hex>
pub fn parse_w3c_traceparent(s: &str) -> Option<([u8; 16], [u8; 8])> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 4 {
        return None;
    }

    // Version must be "00"
    if parts[0] != "00" {
        return None;
    }

    // Trace ID: 32 hex chars = 16 bytes
    let trace_hex = parts[1];
    if trace_hex.len() != 32 {
        return None;
    }
    let trace_id = hex::decode(trace_hex).ok()?;
    if trace_id.len() != 16 {
        return None;
    }
    let mut tid = [0u8; 16];
    tid.copy_from_slice(&trace_id);

    // Span ID: 16 hex chars = 8 bytes
    let span_hex = parts[2];
    if span_hex.len() != 16 {
        return None;
    }
    let span_id = hex::decode(span_hex).ok()?;
    if span_id.len() != 8 {
        return None;
    }
    let mut sid = [0u8; 8];
    sid.copy_from_slice(&span_id);

    Some((tid, sid))
}

/// Convert DC time (nanoseconds since 2000-01-01 UTC) to UTC DateTime
pub fn dc_time_to_datetime(dc_nanos: i64) -> DateTime<Utc> {
    if dc_nanos <= 0 {
        // Fallback to current time if DC clock is missing or zero
        return Utc::now();
    }

    const DC_EPOCH_OFFSET: i64 = 125_911_584_000_000_000; // Nanos between 1970-01-01 and 2000-01-01
    let unix_nanos = dc_nanos + DC_EPOCH_OFFSET;

    let secs = unix_nanos / 1_000_000_000;
    let nanos = (unix_nanos % 1_000_000_000) as u32;

    DateTime::<Utc>::from_timestamp(secs, nanos).unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_w3c_traceparent_valid() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let result = parse_w3c_traceparent(tp);
        assert!(result.is_some());
        let (tid, sid) = result.unwrap();
        assert_eq!(hex::encode(tid), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(hex::encode(sid), "00f067aa0ba902b7");
    }

    #[test]
    fn test_parse_w3c_traceparent_invalid_version() {
        let tp = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(parse_w3c_traceparent(tp).is_none());
    }

    #[test]
    fn test_parse_w3c_traceparent_wrong_length() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e47-00f067aa0ba902b7-01";
        assert!(parse_w3c_traceparent(tp).is_none());
    }

    #[test]
    fn test_span_key_hash_and_eq() {
        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();
        let k1 = SpanKey {
            ams_net_id: net_id,
            task_index: 1,
            local_id: 5,
        };
        let k2 = SpanKey {
            ams_net_id: net_id,
            task_index: 1,
            local_id: 5,
        };
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_dc_time_conversion() {
        let dc_nanos = 0_i64; // DC epoch
        let dt = dc_time_to_datetime(dc_nanos);
        // Should fall back to now, since DC clock is 0
        assert!(dt <= Utc::now());
    }

    #[test]
    fn test_dispatcher_begin_creates_pending_span() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();
        dispatcher.on_begin(net_id, 1, 0, 0, 0xFF, 0, "test_span".to_string(), None);

        assert_eq!(dispatcher.pending_count(), 1);
    }

    #[test]
    fn test_dispatcher_duplicate_key_flushes_stale() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        // First BEGIN
        dispatcher.on_begin(net_id, 1, 0, 0, 0xFF, 0, "first_span".to_string(), None);
        assert_eq!(dispatcher.pending_count(), 1);

        // Second BEGIN with same key — should flush first
        dispatcher.on_begin(net_id, 1, 0, 100, 0xFF, 0, "second_span".to_string(), None);
        assert_eq!(dispatcher.pending_count(), 1);

        // Should have received the timed-out first span
        let first_record = rx.try_recv();
        assert!(first_record.is_ok());
    }

    #[test]
    fn test_dispatcher_attr_adds_to_pending() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        dispatcher.on_begin(net_id, 1, 0, 0, 0xFF, 0, "test".to_string(), None);
        dispatcher.on_attr(net_id, 1, 0, "key1".to_string(), AttrValue::I64(42));

        let span_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 1,
        };
        let pending = dispatcher.pending.get(&span_key).unwrap();
        assert_eq!(pending.attrs.len(), 1);
        assert_eq!(pending.attrs.get("key1"), Some(&AttrValue::I64(42)));
    }

    #[test]
    fn test_dispatcher_end_finalizes_span() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        dispatcher.on_begin(net_id, 1, 0, 0, 0xFF, 0, "test".to_string(), None);
        assert_eq!(dispatcher.pending_count(), 1);

        dispatcher.on_end(net_id, 1, 0, 1000, 1, "success".to_string());
        assert_eq!(dispatcher.pending_count(), 0);

        let record = rx.try_recv();
        assert!(record.is_ok());
        let tr = record.unwrap();
        assert_eq!(tr.name, "test");
        assert_eq!(tr.status_message, "success");
    }

    #[test]
    fn test_dispatcher_parent_lookup() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        // Parent span
        dispatcher.on_begin(net_id, 1, 0, 0, 0xFF, 0, "parent".to_string(), None);

        let parent_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 1,
        };
        let parent_trace_id = dispatcher.pending.get(&parent_key).unwrap().trace_id;
        let parent_span_id = dispatcher.pending.get(&parent_key).unwrap().span_id;

        // Child span with parent_local_id pointing to parent
        dispatcher.on_begin(net_id, 2, 0, 100, 1, 0, "child".to_string(), None);

        let child_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 2,
        };
        let child = dispatcher.pending.get(&child_key).unwrap();
        assert_eq!(child.trace_id, parent_trace_id);
        assert_eq!(child.parent_span_id, Some(parent_span_id));
    }

    #[test]
    fn test_dispatcher_sweep_timed_out() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_millis(100), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        dispatcher.on_begin(net_id, 1, 0, 0, 0xFF, 0, "test".to_string(), None);
        assert_eq!(dispatcher.pending_count(), 1);

        // Manually set deadline to past
        let span_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 1,
        };
        if let Some(p) = dispatcher.pending.get_mut(&span_key) {
            p.deadline = Instant::now() - Duration::from_millis(50);
        }

        dispatcher.sweep_timed_out();
        assert_eq!(dispatcher.pending_count(), 0);

        let record = rx.try_recv();
        assert!(record.is_ok());
    }
}
