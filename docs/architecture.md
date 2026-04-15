# log4TC Architecture: Layered Design

This document describes the layered architecture of log4TC, emphasizing the separation of concerns between the ADS protocol (command dispatch, frame codec, response building) and the transport mechanisms (TCP, MQTT).

## Overview

The core insight is that **the ADS protocol logic should be independent of transport**. This separation enables:

- **Transport neutrality**: Add TCP, MQTT, or future transports without changing command dispatch logic
- **Testability**: Mock transports without network dependencies
- **Maintainability**: Protocol handlers evolve independently from transport implementations
- **Comparison with Beckhoff**: The Beckhoff TwinCAT source code uses a similar layering (AmsRouter → AmsConnection → Frame codec), which informed this design

## Layer Diagram

```
┌─────────────────────────────────────────────────────────┐
│ tc-otel-service (async channels, web UI, exporters)   │
│                 (scheduler, telemetry pipeline)        │
├─────────────────────────────────────────────────────────┤
│ AdsRouter (dispatch, handlers)          ← command-aware  │
│  ├ READ_STATE handler                                   │
│  ├ READ_DEVICE_INFO handler                            │
│  ├ READ handler                                         │
│  └ WRITE handler (parser dispatch)                      │
├─────────────────────────────────────────────────────────┤
│ AmsFrame codec (encode/decode)     ← transport-agnostic │
│  ├ AMS header (32 bytes)                                │
│  ├ ADS payload (variable)                               │
│  └ TCP prefix wrapper (6 bytes)                         │
├─────────────────────────────────────────────────────────┤
│ AmsTransport trait (async send/receive)  ← bytes only   │
│  ├ TcpAmsTransport (TCP sockets)                        │
│  └ MqttAmsTransport (rumqttc broker)                    │
└─────────────────────────────────────────────────────────┘
```

## Layer Responsibilities

### Layer 1: Service (tc-otel-service)

**Responsibility**: Application-level orchestration, channel management, export pipeline

- Spawns async tasks for each transport
- Manages mpsc channels for log/metric entries (LogEntry, MetricEntry)
- Runs the OpenTelemetry exporter pipeline (batching, flushing, HTTP/gRPC)
- Web server (optional) for health checks and diagnostics

### Layer 2: AdsRouter

**Responsibility**: ADS protocol command dispatch, response building, parser integration

- Receives AMS frames (32-byte header + ADS payload)
- Dispatches on command ID:
  - `READ_STATE` → returns ADS state (ready/running)
  - `READ_DEVICE_INFO` → returns device name and version
  - `READ` → returns empty data (variable read placeholder)
  - `WRITE` → parses ADS payload and dispatches to handlers:
    - Registration messages → TaskRegistry
    - Log entries → log_tx channel
    - Metric entries → metric_tx channel (if configured)
- Builds response frames with correct AMS header structure
- **Key property**: Command dispatch logic is pure protocol, has no transport knowledge

### Layer 3: AmsFrame Codec

**Responsibility**: Frame serialization/deserialization, transport format wrapping

- Parses 32-byte AMS header using `AmsHeader::parse()`
- Separates header from variable-length ADS payload
- Encodes (header, payload) back to raw bytes
- TCP wrapper: adds 6-byte prefix (reserved + data length in LE u32)
  - Input: AMS header + payload
  - Output: [2 bytes reserved][4 bytes length][header+payload]
- MQTT: uses frame as-is (no transport prefix)
- **Key property**: Transport-agnostic, doesn't know how bytes arrive or leave

### Layer 4: AmsTransport Trait

**Responsibility**: Transport mechanism (TCP, MQTT, future…)

```rust
#[async_trait]
pub trait AmsTransport: Send + Sync + 'static {
    async fn run(self: Arc<Self>) -> Result<()>;
    async fn send(&self, dest: AmsNetId, frame: Vec<u8>) -> Result<()>;
    fn local_net_id(&self) -> AmsNetId;
}
```

- **Lifecycle**: `run()` starts listening/subscribing indefinitely
- **Frame I/O**: Receives raw bytes, passes to AdsRouter; receives response bytes, forwards via transport
- **Destination routing**: `send()` handles AMS Net ID routing (TCP: lookup connection, MQTT: publish to topic)
- **No protocol logic**: Byte-level only, no command parsing or response building

## Module Layout

| Concern | Module | Notes |
|---------|--------|-------|
| AMS header struct + parse | `ams.rs` | Serialization, parsing, ADS command constants |
| Full frame codec + 6-byte TCP prefix | `frame.rs` | AmsFrame type with encode/decode methods |
| Command dispatch + response builders | `router.rs` | AdsRouter::dispatch() and handlers |
| Log4TC binary parser (v1/v2/registration/metric) | `parser.rs` | AdsParser for ADS_CMD_WRITE payloads |
| TCP listener + per-connection loop | `transport/tcp.rs` | TcpAmsTransport, socket handling |
| MQTT eventloop + pub/sub | `transport/mqtt.rs` | MqttAmsTransport, rumqttc integration |
| Task metadata registry | `registry.rs` | TaskRegistry lookup for v2 log entries |
| Channels (LogEntry, MetricEntry) | `tc_otel_core` + `tc-otel-service` | Cross-crate boundary |
| ADS client request builders | `ads_client.rs` | Helper functions for READ, WRITE requests |
| ADS symbol upload (ADS native) | `symbol.rs` | Symbol table parsing (ADSIGRP_SYM_UPLOAD) |
| Device health metrics | `health_metrics.rs`, `mqtt_health_metrics.rs` | Connection state, message counts |

## Adding a New Transport

To add a new transport (e.g., HTTP/WebSocket, serial, cloud connector):

1. **Create `transport/mynew.rs`** with a struct implementing `AmsTransport`
   ```rust
   pub struct MyNewAmsTransport {
       local_net_id: AmsNetId,
       router: Arc<AdsRouter>,
       // ... transport-specific config
   }
   ```

2. **Implement the trait**:
   - `run()`: Start listening/connecting indefinitely
   - `send()`: Route frames by AMS Net ID
   - `local_net_id()`: Return your AMS Net ID

3. **Byte-level I/O only**: Receive raw bytes → pass to `router.dispatch()` → send response bytes
   - No command parsing, no response building
   - `router.dispatch()` handles all protocol logic

4. **Export from `transport.rs`**:
   ```rust
   pub mod mynew;
   pub use mynew::MyNewAmsTransport;
   ```

5. **Integrate in `tc-otel-service`**: Add config section, spawn task in main loop

## Adding a New ADS Command

To add a new ADS command (e.g., custom telemetry, device control):

1. **Add command constant in `ams.rs`**:
   ```rust
   pub const ADS_CMD_MYCOMMAND: u16 = 0x0042; // example
   ```

2. **Add handler in `router.rs::dispatch()`**:
   ```rust
   ADS_CMD_MYCOMMAND => {
       let payload = &frame[32..];
       // Parse payload, build response
       let response_data = vec![0, 0, 0, 0]; // Result code
       Some(Self::build_response(&header, response_data))
   }
   ```

3. **Add unit test** in `router.rs`:
   ```rust
   #[tokio::test]
   async fn test_mycommand() {
       // Build frame, call dispatch(), verify response
   }
   ```

4. **If command carries log/metric data**: Extend `AdsParser` in `parser.rs`

## Reference

For protocol details and comparisons:
- **Beckhoff TwinCAT Source**: https://github.com/Beckhoff/ADS
  - `AmsRouter`: Command dispatch pattern
  - `AmsConnection`: Transport handling pattern
  - `Frame`: Codec design inspiration
- **ADS Protocol Spec**: Beckhoff TwinCAT 3 documentation (internal reference)

## Future Work

- **Planned refactors** (mentioned in sibling branches):
  - `refactor/ams-frame-codec`: Consolidate frame handling
  - `refactor/ads-router`: Extend command set for metrics, spans, custom handlers
- **New transports**: WebSocket, cloud-native (Azure Event Hubs, AWS Kinesis), serial links
- **Extensible handlers**: Plugin system for custom command processing
