# Custom Metrics — Active ADS Client

This document describes the **`tc-otel-client`** crate and its integration via
the `client-bridge` feature in `tc-otel-service`. It complements
[`push-metrics-wire-format.md`](push-metrics-wire-format.md), which covers the
PLC-initiated push path.

## When to use this vs. push metrics

| Source | Who initiates | Setup | Determinism | When to pick it |
|---|---|---|---|---|
| `push` | PLC code (function-block API) | Requires PLC rebuild | Deterministic — driven by the task cycle | Production metrics owned by PLC developer |
| `poll` | tc-otel | UI-driven, no PLC rebuild | Client-controlled cadence, subject to jitter | Runtime observability, quick diagnostic metrics |
| `notification` | tc-otel subscribes, PLC pushes on change | UI-driven, no PLC rebuild | On-change (or cyclic) up to PLC cycle rate | Event-driven metrics, sparse signals |

`poll` and `notification` are implemented by this feature. `push` comes from
the separate function-block library and is unrelated to this document.

## Architecture

tc-otel has two distinct AMS/TCP roles:

- **Server / observer** (`tc-otel-ads`): inbound TCP 48898; the PLC's log and
  push-metric pipeline sends frames into tc-otel.
- **Active client** (`tc-otel-client`): outbound TCP 48898 to a target PLC's
  AMS router; tc-otel issues `Read` and `AddDeviceNotification`.

The two stacks are independent. `tc-otel-client` depends on the open-source
[`ads`](https://crates.io/crates/ads) crate (pure Rust, no Beckhoff router
required) and does **not** share code with `tc-otel-ads`.

```text
┌─────────────────────────────────────────────────────────┐
│ PLC (AMS router on port 48898)                           │
└───────▲───────────────────────────────────▲──────────────┘
        │  in: log/push frames              │  out: Read / Notification
        │                                   │
┌───────┴───────────────────┐   ┌───────────┴──────────────┐
│ tc-otel-ads (observer)    │   │ tc-otel-client (active)  │
│  • listener (48898)       │   │  • AdsClient (per target)│
│  • router / parser        │   │  • SymbolTreeCache       │
│  • diagnostics decoder    │   │  • Poller / Notifier     │
└───────────────┬───────────┘   └──────────┬───────────────┘
                │                          │
                └──────┬───────────────────┘
                       ▼
         ┌─────────────────────────────┐
         │ tc-otel-service              │
         │  MetricDispatcher → OTLP     │
         └─────────────────────────────┘
```

The `client-bridge` feature of `tc-otel-service` (enabled by default) wires
the two sides together: it dials each unique PLC target referenced in
`custom_metrics`, uploads the symbol table, and spawns pollers/notifiers
whose output feeds the same `MetricDispatcher` as the push path.

## Configuration

Every non-`push` custom metric must declare `ams_net_id` (and optionally
`ams_port`, default 851). The source discriminator plus per-source sub-struct
select the behavior:

```json
{
  "metrics": {
    "custom_metrics": [
      {
        "symbol": "MAIN.fMotorTemp",
        "metric_name": "plc.motor.temp",
        "description": "Motor temperature",
        "unit": "Cel",
        "kind": "gauge",
        "source": "poll",
        "ams_net_id": "10.0.0.10.1.1",
        "ams_port": 851,
        "poll": { "interval_ms": 500 }
      },
      {
        "symbol": "GVL.bEmergencyStop",
        "metric_name": "plc.estop",
        "kind": "gauge",
        "source": "notification",
        "ams_net_id": "10.0.0.10.1.1",
        "notification": {
          "min_period_ms": 50,
          "max_period_ms": 10000,
          "max_delay_ms": 500,
          "transmission_mode": "on_change"
        }
      }
    ]
  }
}
```

### Supported symbol types

Scalars only:

| IEC type | Wire size |
|---|---|
| `BOOL` | 1 B |
| `SINT`, `USINT`, `BYTE` | 1 B |
| `INT`, `UINT`, `WORD` | 2 B |
| `DINT`, `UDINT`, `DWORD` | 4 B |
| `LINT`, `ULINT`, `LWORD` | 8 B |
| `REAL` | 4 B |
| `LREAL` | 8 B |

All values promote to `f64` for OTLP emission. Strings, structs, arrays are
rejected at the decode boundary (the entry is dropped with a `warn!` log).

## Runtime behavior

### Poller

- On startup, looks up the symbol in the per-target `SymbolTreeCache` (see
  below) or falls back to `ads::symbol::get_location` if the cache is cold.
- Each tick: `spawn_blocking(ads::Device::read)` → decode → `MetricEntry` →
  `metric_tx`.
- **Backpressure**: `try_send`; drop + `warn!` if the channel is full.
- **Error recovery**: exponential backoff (250 ms → 30 s cap) on read failure.
- **Shutdown**: bridge aborts the task on config change or service stop.

### Notifier

- One `AdsClient` owns one `notif::Receiver`. A single `Notifier` drives all
  subscriptions for that client.
- `subscribe()` issues `AddDeviceNotification` (ADS cmd 6) → u32 handle →
  stored in a `HashMap<handle, Subscription>`.
- `spawn_dispatcher()` runs a blocking loop reading from the backend's
  `crossbeam_channel::Receiver<Notification>` and routes each sample to the
  corresponding metric entry.
- `reconcile()` compares a desired `Vec<(Def, Meta)>` against the current
  subscriptions and issues only the minimum set of add/remove operations.

## Symbol cache

The PLC symbol table can have 10k+ entries on a real project. The bridge
uploads it once per target on connect (`ads::symbol::get_symbol_info`,
≈ few hundred kB over AMS) and stores it in a `SymbolTreeCache`.

- **Key**: 6-byte AMS Net ID.
- **Population**: on bridge startup for each unique target.
- **Invalidation**: explicit — via the UI refresh button (`POST
  /api/client/symbols/refresh?target=<net_id>`) or when the bridge drops a
  target during reconcile.
- **Not auto-TTL**: symbols don't drift once a PLC is running. A PLC online
  change would invalidate the cache, but that path is currently manual (press
  Refresh in the UI after applying an online change).

## Web UI

The `Symbols` tab in the dashboard exposes:

1. **Target list**: all PLC targets referenced by `custom_metrics` entries,
   showing per-target cache status (`✓` / `—`), symbol count, and last-fetched
   timestamp.
2. **Symbol table**: filterable by prefix; each row has a *copy-to-clipboard*
   button so users can paste symbol names into the Config tab's
   `custom_metrics` editor.
3. **Refresh button**: POSTs `/api/client/symbols/refresh` — cache invalidates
   and repopulates on the next reconcile.

## Feature flag

`client-bridge` is a default feature of `tc-otel-service`. To opt out (e.g.,
embedded builds without an outbound client):

```bash
cargo build --no-default-features -p tc-otel-service
```

When the feature is disabled:

- `custom_metrics` entries with `source: "poll"` or `"notification"` are
  silently ignored.
- `/api/client/*` endpoints respond with HTTP 503.
- The `Symbols` UI tab shows a "client-bridge is not enabled" banner.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| 503 at `/api/client/*` | Feature off, or `metric_tx`/`config_rx` were not set up (metrics export disabled?) | Enable metrics export; ensure default features are on |
| Target shows `cached: —`, no symbols | Bridge couldn't dial the PLC | Check `router_addr` — the first four bytes of the net ID must be a routable IP |
| Poller emits `warn!` "unsupported scalar" | Symbol type is not in the table above | Promote on the PLC side (wrap in `LREAL`) or switch to push-source |
| Notification samples missing after online change | Cache is stale; handles are invalid | Hit the Refresh button or restart the service |
| Channel-full warnings in logs | `service.channel_capacity` too small for configured metric rate | Lower `poll.interval_ms`, or raise channel capacity in config |

## Wire-level details

For developers extending the stack:

- Poller uses `Device::read_exact(ig, io, &mut buf)` — a standard ADS Read
  (cmd 2). The symbol location (ig/io) comes from the cached `SymbolTree`.
- Notifier uses `Device::add_notification(ig, io, &attrs)` (ADS cmd 6),
  mapping `TransmissionMode::{OnChange, Cyclic}` → `ServerOnChange /
  ServerCycle` on the wire.
- The `ads` crate handles AMS framing, invoke-id bookkeeping, and the
  reader thread that splits replies from notifications. We do not touch the
  wire format directly.

## Testing

- Unit + integration tests in `tc-otel-client/tests/` cover the parser
  (`browse_parser.rs`), cache (`cache_behavior.rs`), poll loop
  (`poll_loop.rs`) and notifier (`notify_loop.rs`) — all using in-process
  fakes.
- The `live-plc`-gated `custom_metrics_client_e2e.rs` runs against a real
  PLC:

  ```bash
  export TC_OTEL_TEST_AMS_ROUTER="10.0.0.10:48898"
  export TC_OTEL_TEST_AMS_NET_ID="10.0.0.10.1.1"
  export TC_OTEL_TEST_AMS_PORT="851"
  export TC_OTEL_TEST_SYMBOL="MAIN.fTestValue"
  cargo test --features live-plc --test custom_metrics_client_e2e -- --ignored
  ```
