//! Distributed trace span dispatcher — aggregates trace wire events into completed spans

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
    pub orphan_reason: Option<String>,
    #[allow(dead_code)]
    pub ams_net_id: AmsNetId,
    #[allow(dead_code)]
    pub task_index: u8,
}

/// Dispatcher that processes trace wire events and produces completed spans
pub struct SpanDispatcher {
    pending: HashMap<SpanKey, PendingSpan>,
    // Secondary index: span_id -> primary SpanKey.
    // Points from pregenerated span_id back to the primary key.
    // Populated only when flag_local_ids is set.
    pending_by_span_id: HashMap<[u8; 8], SpanKey>,
    trace_tx: mpsc::Sender<TraceRecord>,
    span_ttl: Duration,
    max_pending: usize,
    orphan_counter: Arc<AtomicU64>,
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
            pending_by_span_id: HashMap::new(),
            trace_tx,
            span_ttl,
            max_pending,
            orphan_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the orphan span counter value for testing/observability
    #[allow(dead_code)]
    pub fn orphan_counter(&self) -> u64 {
        self.orphan_counter.load(Ordering::SeqCst)
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
                pregenerated_trace_id,
                pregenerated_span_id,
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
                    pregenerated_trace_id,
                    pregenerated_span_id,
                );
            }
            TraceWireEvent::Attr {
                span_id,
                task_index,
                flags: _,
                dc_time: _,
                key,
                value,
            } => {
                self.on_attr(net_id, span_id, task_index, key, value);
            }
            TraceWireEvent::Event {
                span_id,
                task_index,
                flags: _,
                dc_time,
                name,
                attrs,
            } => {
                self.on_event_ev(net_id, span_id, task_index, dc_time, name, attrs);
            }
            TraceWireEvent::End {
                span_id,
                task_index,
                flags: _,
                dc_time,
                status,
                message,
            } => {
                self.on_end(net_id, span_id, task_index, dc_time, status, message);
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
        pregenerated_trace_id: Option<[u8; 16]>,
        pregenerated_span_id: Option<[u8; 8]>,
    ) {
        let key = SpanKey {
            ams_net_id: net_id,
            task_index,
            local_id,
        };

        // Resolution order for trace_id + parent_span_id:
        //   1. External W3C traceparent  — upstream propagation wins
        //   2. PLC-local parent lookup    — nested-span stack (inherits trace_id)
        //   3. Fresh mint                 — root span without upstream context
        //
        // For span_id specifically, the PLC-minted ID (when present) is
        // preferred over a fresh UUID so `CurrentTraceParent()` on the
        // producer side can cite the exact bytes that tc-otel stores and
        // downstream consumers can set parent_span_id accordingly.
        let (trace_id, parent_span_id, orphan_reason) = if let Some(tp) = &traceparent {
            match parse_w3c_traceparent(tp) {
                Some((tid, sid)) => (tid, Some(sid), None),
                None => {
                    tracing::warn!("Failed to parse W3C traceparent: {}", tp);
                    (
                        pregenerated_trace_id.unwrap_or_else(|| self.new_trace_id()),
                        None,
                        None,
                    )
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
                Some(p) => (p.trace_id, Some(p.span_id), None),
                None => {
                    let reason = format!("parent_local_id_{}_not_found", parent_local_id);
                    tracing::debug!(
                        parent_local_id = parent_local_id,
                        net_id = %net_id,
                        task_index = task_index,
                        "parent span not found for local_id {}, treating as root",
                        parent_local_id
                    );
                    self.orphan_counter.fetch_add(1, Ordering::SeqCst);
                    (
                        pregenerated_trace_id.unwrap_or_else(|| self.new_trace_id()),
                        None,
                        Some(reason),
                    )
                }
            }
        } else {
            // Root span — use PLC-minted trace_id when offered.
            (
                pregenerated_trace_id.unwrap_or_else(|| self.new_trace_id()),
                None,
                None,
            )
        };

        // Honour PLC-minted span_id when offered. External traceparent
        // identifies the PARENT, so pregenerated local span_id is still
        // the right identifier for THIS span.
        let span_id = pregenerated_span_id.unwrap_or_else(|| self.new_span_id());

        // Check capacity and evict if needed
        if self.pending.len() >= self.max_pending {
            self.evict_oldest();
        }

        // If key already exists, flush stale entry
        if let Some(stale) = self.pending.remove(&key) {
            // Clean up span_id index if present
            if let Some(old_key) = self.pending_by_span_id.remove(&stale.span_id) {
                // Verify consistency: the old span_id should point to the key we're evicting
                debug_assert_eq!(old_key, key);
            }
            self.finalise_timed_out(stale);
        }

        let mut attrs = HashMap::new();
        if let Some(ref reason) = orphan_reason {
            attrs.insert(
                "TCOTEL.orphan_reason".to_string(),
                AttrValue::String(reason.clone()),
            );
        }

        let pending = PendingSpan {
            trace_id,
            span_id,
            parent_span_id,
            name,
            kind,
            start_time: dc_time_to_datetime(dc_time),
            attrs,
            events: Vec::new(),
            deadline: Instant::now() + self.span_ttl,
            orphan_reason,
            ams_net_id: net_id,
            task_index,
        };

        // Phase 6 Stage 3: Always insert into secondary index (span_id is now primary)
        self.pending_by_span_id.insert(span_id, key);

        self.pending.insert(key, pending);
    }

    fn on_attr(
        &mut self,
        net_id: AmsNetId,
        span_id: [u8; 8],
        task_index: u8,
        key: String,
        value: AttrValue,
    ) {
        // Phase 6 Stage 3: Look up span by span_id via pending_by_span_id index
        if let Some(&span_key) = self.pending_by_span_id.get(&span_id) {
            if let Some(pending) = self.pending.get_mut(&span_key) {
                pending.attrs.insert(key, value);
            }
        } else {
            tracing::debug!(
                "SPAN_ATTR for unknown span {}:{}:{:?}",
                net_id,
                task_index,
                span_id
            );
        }
    }

    fn on_event_ev(
        &mut self,
        net_id: AmsNetId,
        span_id: [u8; 8],
        task_index: u8,
        dc_time: i64,
        name: String,
        attrs: Vec<(String, AttrValue)>,
    ) {
        // Phase 6 Stage 3: Look up span by span_id via pending_by_span_id index
        if let Some(&span_key) = self.pending_by_span_id.get(&span_id) {
            if let Some(pending) = self.pending.get_mut(&span_key) {
                pending.events.push(SpanEvent {
                    time: dc_time_to_datetime(dc_time),
                    name,
                    attrs,
                });
            }
        } else {
            tracing::debug!(
                "SPAN_EVENT for unknown span {}:{}:{:?}",
                net_id,
                task_index,
                span_id
            );
        }
    }

    fn on_end(
        &mut self,
        net_id: AmsNetId,
        span_id: [u8; 8],
        task_index: u8,
        dc_time: i64,
        status: u8,
        message: String,
    ) {
        // Phase 6 Stage 3: Look up span by span_id via pending_by_span_id index
        if let Some(&span_key) = self.pending_by_span_id.get(&span_id) {
            if let Some(pending) = self.pending.remove(&span_key) {
                // Clean up span_id index entry
                self.pending_by_span_id.remove(&span_id);

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
            }
        } else {
            tracing::debug!(
                "SPAN_END for unknown span {}:{}:{:?}",
                net_id,
                task_index,
                span_id
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
        // Populate resource attributes so Grafana/Tempo link the span to a
        // service. Without service.name the UI labels it "root span not yet
        // received" and the trace-detail view returns no data.
        let mut resource_attributes = HashMap::with_capacity(3);
        resource_attributes.insert(
            "service.name".to_string(),
            serde_json::json!(format!("plc-{}", pending.ams_net_id)),
        );
        resource_attributes.insert(
            "plc.ams_net_id".to_string(),
            serde_json::json!(pending.ams_net_id.to_string()),
        );
        resource_attributes.insert(
            "plc.task_index".to_string(),
            serde_json::json!(pending.task_index as i64),
        );

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
            resource_attributes,
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
                // Clean up span_id index entry if present
                self.pending_by_span_id.remove(&pending.span_id);
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
                // Clean up span_id index entry if present
                self.pending_by_span_id.remove(&pending.span_id);
                self.finalise_timed_out(pending);
            }
        }
    }

    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Lookup a pending span by its span_id (pregenerated ID from the PLC).
    /// Returns a reference to the PendingSpan, or None if not found or not indexed.
    /// This is the API for Stage 3 PLC events that are keyed by span_id on the wire.
    ///
    /// Currently unused in the main binary (used by integration tests and Stage 3 PLC).
    #[allow(dead_code)]
    pub fn pending_by_span_id(&self, span_id: &[u8; 8]) -> Option<&PendingSpan> {
        self.pending_by_span_id
            .get(span_id)
            .and_then(|key| self.pending.get(key))
    }

    /// Lookup the innermost open span for a task.
    /// Returns (trace_id_hex, span_id_hex) of the most recently begun span, or None.
    pub fn current_context(
        &self,
        ams_net_id: &AmsNetId,
        task_index: u8,
    ) -> Option<(String, String)> {
        // Find all spans for this (ams_net_id, task_index) pair and return the
        // most recently begun one (highest start_time).
        let mut best: Option<(SpanKey, DateTime<Utc>, String, String)> = None;

        for (key, pending) in self.pending.iter() {
            if key.ams_net_id == *ams_net_id && key.task_index == task_index {
                let trace_id_hex = hex::encode(pending.trace_id);
                let span_id_hex = hex::encode(pending.span_id);

                match &mut best {
                    None => {
                        best = Some((*key, pending.start_time, trace_id_hex, span_id_hex));
                    }
                    Some((_, best_time, _, _)) if pending.start_time > *best_time => {
                        best = Some((*key, pending.start_time, trace_id_hex, span_id_hex));
                    }
                    _ => {}
                }
            }
        }

        best.map(|(_, _, trace_hex, span_hex)| (trace_hex, span_hex))
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

    // 946_684_800 s between 1970-01-01 and 2000-01-01 UTC → nanoseconds.
    const DC_EPOCH_OFFSET_NS: i64 = 946_684_800_000_000_000;
    let unix_nanos = dc_nanos + DC_EPOCH_OFFSET_NS;

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
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test_span".to_string(),
            None,
            None,
            None,
        );

        assert_eq!(dispatcher.pending_count(), 1);
    }

    #[test]
    fn test_dispatcher_duplicate_key_flushes_stale() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        // First BEGIN
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "first_span".to_string(),
            None,
            None,
            None,
        );
        assert_eq!(dispatcher.pending_count(), 1);

        // Second BEGIN with same key — should flush first
        dispatcher.on_begin(
            net_id,
            1,
            0,
            100,
            0xFF,
            0,
            "second_span".to_string(),
            None,
            None,
            None,
        );
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

        let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test".to_string(),
            None,
            None,
            Some(span_id),
        );
        dispatcher.on_attr(net_id, span_id, 0, "key1".to_string(), AttrValue::I64(42));

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

        let span_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test".to_string(),
            None,
            None,
            Some(span_id),
        );
        assert_eq!(dispatcher.pending_count(), 1);

        dispatcher.on_end(net_id, span_id, 0, 1000, 1, "success".to_string());
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
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "parent".to_string(),
            None,
            None,
            None,
        );

        let parent_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 1,
        };
        let parent_trace_id = dispatcher.pending.get(&parent_key).unwrap().trace_id;
        let parent_span_id = dispatcher.pending.get(&parent_key).unwrap().span_id;

        // Child span with parent_local_id pointing to parent
        dispatcher.on_begin(
            net_id,
            2,
            0,
            100,
            1,
            0,
            "child".to_string(),
            None,
            None,
            None,
        );

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

        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test".to_string(),
            None,
            None,
            None,
        );
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

    #[test]
    fn test_dispatcher_orphan_span_no_parent_in_pending() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        // BEGIN with parent_local_id=5 but no span with local_id=5 in pending
        let span_id = [10u8, 2, 3, 4, 5, 6, 7, 8];
        dispatcher.on_begin(
            net_id,
            10,
            0,
            0,
            5, // non-existent parent
            0,
            "orphan_span".to_string(),
            None,
            None,
            Some(span_id),
        );

        // Should create a pending span with orphan_reason set
        let orphan_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 10,
        };
        let pending = dispatcher.pending.get(&orphan_key).unwrap();

        // Check that orphan_reason is set
        assert!(pending.orphan_reason.is_some());
        assert_eq!(
            pending.orphan_reason.as_ref().unwrap(),
            "parent_local_id_5_not_found"
        );

        // Check that the orphan_reason attribute is attached
        let orphan_attr = pending.attrs.get("TCOTEL.orphan_reason");
        assert!(orphan_attr.is_some());
        match orphan_attr.unwrap() {
            AttrValue::String(s) => assert_eq!(s, "parent_local_id_5_not_found"),
            _ => panic!("Expected string attribute"),
        }

        // Counter should be incremented
        assert_eq!(dispatcher.orphan_counter(), 1);

        // END the span and verify it's finalized with the attribute intact
        dispatcher.on_end(net_id, span_id, 0, 1000, 1, "success".to_string());
        assert_eq!(dispatcher.pending_count(), 0);

        let record = rx.try_recv();
        assert!(record.is_ok());
        let tr = record.unwrap();
        assert_eq!(tr.name, "orphan_span");
        assert_eq!(tr.parent_span_id, "".to_string()); // no parent
        let orphan_attr_in_record = tr.span_attributes.get("TCOTEL.orphan_reason");
        assert!(orphan_attr_in_record.is_some());
    }

    #[test]
    fn test_dispatcher_orphan_counter_increments() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        assert_eq!(dispatcher.orphan_counter(), 0);

        // First orphan
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            99, // non-existent parent
            0,
            "orphan1".to_string(),
            None,
            None,
            None,
        );
        assert_eq!(dispatcher.orphan_counter(), 1);

        // Second orphan
        dispatcher.on_begin(
            net_id,
            2,
            0,
            100,
            99, // non-existent parent again
            0,
            "orphan2".to_string(),
            None,
            None,
            None,
        );
        assert_eq!(dispatcher.orphan_counter(), 2);

        // Normal parent-child (no increment)
        dispatcher.on_begin(
            net_id,
            3,
            0,
            200,
            0xFF,
            0,
            "root".to_string(),
            None,
            None,
            None,
        );
        assert_eq!(dispatcher.orphan_counter(), 2); // still 2

        dispatcher.on_begin(
            net_id,
            4,
            0,
            300,
            3, // points to existing span 3
            0,
            "child".to_string(),
            None,
            None,
            None,
        );
        assert_eq!(dispatcher.orphan_counter(), 2); // still 2
    }

    #[test]
    fn test_dispatcher_normal_parent_child_no_orphan_attribute() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        // Parent span
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "parent".to_string(),
            None,
            None,
            None,
        );

        let parent_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 1,
        };
        let parent = dispatcher.pending.get(&parent_key).unwrap();
        assert!(parent.orphan_reason.is_none());
        assert!(!parent.attrs.contains_key("TCOTEL.orphan_reason"));

        // Child span with valid parent
        dispatcher.on_begin(
            net_id,
            2,
            0,
            100,
            1, // points to existing parent
            0,
            "child".to_string(),
            None,
            None,
            None,
        );

        let child_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 2,
        };
        let child = dispatcher.pending.get(&child_key).unwrap();
        assert!(child.orphan_reason.is_none());
        assert!(!child.attrs.contains_key("TCOTEL.orphan_reason"));

        // Counter should not have been incremented
        assert_eq!(dispatcher.orphan_counter(), 0);
    }

    #[test]
    fn test_pending_by_span_id_with_pregenerated() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();
        let pregenerated_id = [1u8; 8];

        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test_span".to_string(),
            None,
            None,
            Some(pregenerated_id),
        );

        // Should be retrievable via pending_by_span_id
        let pending = dispatcher.pending_by_span_id(&pregenerated_id);
        assert!(pending.is_some());
        assert_eq!(pending.unwrap().span_id, pregenerated_id);
        assert_eq!(pending.unwrap().name, "test_span");
    }

    #[test]
    fn test_pending_by_span_id_without_pregenerated() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();

        // BEGIN without pregenerated_span_id
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test_span".to_string(),
            None,
            None,
            None,
        );

        // Get the generated span_id from pending
        let span_key = SpanKey {
            ams_net_id: net_id,
            task_index: 0,
            local_id: 1,
        };
        let generated_id = dispatcher.pending.get(&span_key).unwrap().span_id;

        // Phase 6 Stage 3: All span_ids (generated or pregenerated) are indexed
        let pending = dispatcher.pending_by_span_id(&generated_id);
        assert!(pending.is_some());
        assert_eq!(pending.unwrap().span_id, generated_id);
    }

    #[test]
    fn test_pending_by_span_id_end_removes_index() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();
        let pregenerated_id = [2u8; 8];

        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "test_span".to_string(),
            None,
            None,
            Some(pregenerated_id),
        );

        // Verify it's indexed
        assert!(dispatcher.pending_by_span_id(&pregenerated_id).is_some());

        // END the span
        dispatcher.on_end(net_id, pregenerated_id, 0, 1000, 1, "success".to_string());

        // Should be removed from index and primary map
        assert!(dispatcher.pending_by_span_id(&pregenerated_id).is_none());
        assert_eq!(dispatcher.pending_count(), 0);
        let _record = rx.try_recv();
        assert!(_record.is_ok());
    }

    #[test]
    fn test_pending_by_span_id_parallel_spans() {
        let (tx, _rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();
        let id1 = [1u8; 8];
        let id2 = [2u8; 8];

        // Two parallel spans with different pregenerated IDs
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "span1".to_string(),
            None,
            None,
            Some(id1),
        );

        dispatcher.on_begin(
            net_id,
            2,
            0,
            100,
            0xFF,
            0,
            "span2".to_string(),
            None,
            None,
            Some(id2),
        );

        // Both should be retrievable
        let pending1 = dispatcher.pending_by_span_id(&id1);
        let pending2 = dispatcher.pending_by_span_id(&id2);

        assert!(pending1.is_some());
        assert!(pending2.is_some());
        assert_eq!(pending1.unwrap().name, "span1");
        assert_eq!(pending2.unwrap().name, "span2");
    }

    #[test]
    fn test_pending_by_span_id_duplicate_key_cleans_old() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut dispatcher = SpanDispatcher::new(tx, Duration::from_secs(10), 1024);

        let net_id = AmsNetId::from_str_ref("192.168.1.1.1.1").unwrap();
        let pregenerated_id = [3u8; 8];

        // First BEGIN with pregenerated_span_id
        dispatcher.on_begin(
            net_id,
            1,
            0,
            0,
            0xFF,
            0,
            "first_span".to_string(),
            None,
            None,
            Some(pregenerated_id),
        );

        assert!(dispatcher.pending_by_span_id(&pregenerated_id).is_some());

        // Second BEGIN with same key (local_id) but different name — should flush first
        dispatcher.on_begin(
            net_id,
            1,
            0,
            100,
            0xFF,
            0,
            "second_span".to_string(),
            None,
            None,
            Some(pregenerated_id),
        );

        // Old span should be finalized
        let _first_record = rx.try_recv();
        assert!(_first_record.is_ok());

        // Index should point to the new span
        let pending = dispatcher.pending_by_span_id(&pregenerated_id);
        assert!(pending.is_some());
        assert_eq!(pending.unwrap().name, "second_span");
    }
}
