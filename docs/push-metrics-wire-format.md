# Push Metrics Wire Format

## Overview

Push metrics let the PLC emit user-defined counters, gauges, and histograms to
tc-otel on every cycle (or on demand). Unlike the existing per-task diagnostics
which are fixed (exec-time, cycle-exceed, RT-violation), push metrics allow PLC
developers to stream arbitrary performance data — motor temperatures, pump flow
rates, battery levels, algorithm error counts — without ADS read overhead or
runtime configuration complexity.

Metrics are collected into batches and flushed via ADS Write to the tc-otel
listener port. Each batch announces metric metadata (name, unit, kind, bounds)
once on first appearance, then subsequent batches reference the same metric by
stable ID and ship only samples.

## Transport

All push-metric events are sent as ADS Write commands to:

- **Index group**: `IG_PUSH_DIAG = 0x4D42_4301` (shared with push diagnostics;
  ASCII "MBC\1" in little-endian, chosen for debug-friendliness)
- **Target port**: The dedicated tc-otel listener port (default 16150)
- **Index offset**: `IO_PUSH_METRIC_BATCH = 1` (distinct from `IO_PUSH_BATCH = 0`
  which carries per-task diagnostics)

This design reuses the same index group as per-task diagnostics but on a
different index offset, allowing both batch types to flow through the same ADS
listen socket without interference.

## Wire Format

All offsets and sizes are in bytes. All integers are **little-endian**. Floating-
point values are 32-bit IEEE 754 (`f32`). The on-wire layout is declared in the
PLC with `{attribute 'pack_mode' := '1'}` so structs serialize exactly as laid
out — no padding.

### Batch header — 32 bytes

```
+0x00  u8    version        = 1
+0x01  u8    event_type     = 20  (metric batch discriminator)
+0x02  u16   reserved       = 0
+0x04  u16   descriptor_count   (number of descriptors in this frame, can be 0 after first)
+0x06  u16   sample_count       (number of samples in this frame)
+0x08  u16   window_ms          (aggregation window or sample interval)
+0x0A  u16   reserved       = 0
+0x0C  u32   cycle_count        (PLC base cycle counter or task cycle counter)
+0x10  i64   dc_time_start      (FILETIME ticks, first sample)
+0x18  i64   dc_time_end        (FILETIME ticks, last sample or announcement)
```

Total: **32 bytes**.

### Descriptor block — variable length

Descriptors announce metric metadata. They are sent:
- On the first appearance of a metric in a session,
- When metadata changes (renamed, unit changed, bounds changed),
- On PLC online-change (restart),
- Can be re-sent opportunistically to refresh the dispatcher's cache.

Each descriptor is followed by its string and attribute payloads.

**Descriptor record header — 16 bytes:**

```
+0x00  u16   metric_id          (stable ID within session; client assigns)
+0x02  u8    kind               (0=Gauge, 1=Sum/Counter, 2=Histogram)
+0x03  u8    flags              (bit 0: is_monotonic; only for Sum)
+0x04  u8    name_len           (UTF-8 name length in bytes)
+0x05  u8    unit_len           (UTF-8 unit length in bytes)
+0x06  u8    description_len    (UTF-8 description length in bytes)
+0x07  u8    attr_count         (number of attributes; max 8)
+0x08  u8    histogram_bucket_count  (0 for non-Histogram; number of bucket boundaries)
+0x09  u8    reserved           = 0
+0x0A  u16   reserved           = 0
+0x0C  <name bytes>             (name_len bytes of UTF-8)
       <unit bytes>             (unit_len bytes of UTF-8)
       <description bytes>      (description_len bytes of UTF-8)
       <attributes>             (attr_count × ST_TcOtelAttr)
       <histogram_bounds>       (histogram_bucket_count × f32, if kind==2)
```

**Attribute record — variable length, repeated `attr_count` times:**

```
+0x00  u8    key_len            (UTF-8 key length in bytes)
+0x01  u8    value_len          (UTF-8 value length in bytes)
+0x02  <key bytes>              (key_len bytes of UTF-8)
       <value bytes>            (value_len bytes of UTF-8)
```

**Validation rules:**
- Reject frames with `version != 1` or `event_type != 20`.
- Reject if `attr_count > 8`.
- Reject if `histogram_bucket_count != 0` when `kind != 2`.
- String lengths and attribute data must be valid UTF-8; reject frames with
  invalid UTF-8 sequences.
- Reject frames that would read past the declared total length.

### Sample record — 16 bytes, repeated `sample_count` times

```
+0x00  u16   metric_id          (matches a descriptor ID)
+0x02  u8    flags              (bit 0: histogram_observe, bit 1: counter_delta_i64)
+0x03  u8    reserved           = 0
+0x04  i64   dc_time            (FILETIME ticks; may differ per sample in a batch)
+0x0C  f32   value              (32-bit IEEE 754 floating-point)
```

Total: **16 bytes per sample**.

**Sample semantics:**

- **Gauge**: `value` is the instantaneous reading at `dc_time`.

- **Sum/Counter**: `value` is the delta added this sample (for monotonic counters:
  always >= 0; for non-monotonic sums: can be any value). The decoder accumulates
  deltas across samples or emits non-monotonic Sum depending on
  `is_monotonic` in the descriptor.

- **Histogram**: `flags` bit 0 = `histogram_observe` indicates the `value` is an
  observation to be accumulated into the histogram's bucket counts. Each observation
  is matched against the bounds declared in the descriptor to determine bucket
  placement. Decoder accumulates observations across samples and emits a
  histogram-shaped `MetricEntry` with populated `histogram_bounds`,
  `histogram_counts`, `histogram_count`, and `histogram_sum`.

## Size and Bandwidth Estimation

Batches can be per-task, per-application, or per-producer depending on the PLC
producer's design. The examples below assume one batch per cycle or per
window.

| Scenario | Header | 1 Descriptor | 50 Samples | Total |
| -------- | ------ | ------------ | ---------- | ----- |
| First announcement (full descriptor) | 32 | ~80 | 800 | ~912 B |
| Samples-only follow-up batch | 32 | 0 | 800 | 832 B |
| 50 samples / batch × 10 batches / sec | — | — | — | ~8.3 kB/s |

Typical window: 100 ms on a 1 ms task → 100 samples/batch. Typical descriptor:
name + unit + description ≈ 40 bytes; attribute table ≈ 20 bytes → ~70 bytes.

Well under the ADS write payload limit of 64 KiB.

## Aggregate batch: optional per-sample timestamps

The compact FB_Metrics aggregate frame (`event_type = 21`, wire version 2 —
see `ST_PushMetricAggHeader`) by default carries per-sample timestamps only
implicitly via `dc_time_start`/`dc_time_end`. The receiver interpolates
linearly across the window. When the PLC owns irregular sampling (change-
detect bursts, sporadic `Observe` calls, heterogeneous `SetSampleIntervalMs`
changes), a per-sample cycle offset can be opt-in emitted.

**Header flag**: `flags.bit2 = METRIC_FLAG_HAS_SAMPLE_TS = 0x04`.

**Body layout when set**: each sample slot is preceded by a `u16`
little-endian `cycle_offset`, expressed as cycles since `cycle_count_start`.
Slot stride becomes `sample_size + 2`. `sample_size` itself remains the
value size (unchanged semantics — decoders can still dispatch on it).

**Reconstructing DC time**:
```
cycle_time_ns = (dc_time_end - dc_time_start) / (cycle_count_end - cycle_count_start)
ts_sample_i   = dc_time_start + cycle_offset[i] * cycle_time_ns
```

**Why `u16`**: at the minimum TwinCAT tick of 250 µs, u16 covers 16.38 s —
well beyond any realistic metric push window. The PLC-side `_CheckFlush`
caps the window at `0xFFFF` cycles when the flag is active, so offsets never
overflow; the only observable effect is an earlier autoflush for extreme
configs.

**Cost**: `+2 B` per sample when active. Noticeable on Bool bodies (1 B →
3 B, +200 %), negligible on NumericAggregated (48 B → 50 B, +4 %). Zero when
the flag is not set.

**Enabling from PLC**: `FB_Metrics.SetRecordSampleTimes(TRUE)`. Opt-in only —
default behavior is unchanged.

## Versioning and Compatibility

- **Wire version = 1**: Stable metric batch format. Future breaking changes will
  bump to v2.

- **Event type = 20**: Metric batch discriminator. Decoders can dispatch on this
  byte to handle metric batches separately from per-task diagnostics (event_type
  = 10).

- **kind field**: MetricKind values (Gauge=0, Sum=1, Histogram=2) map directly to
  OpenTelemetry kinds. New kinds will bump event_type, not reuse existing values.

## Rust Decoder Integration

The decoder in `crates/tc-otel-ads/src/diagnostics_push.rs` provides a new
function `decode_metric_batch()` that mirrors the existing `decode_batch()`
for task diagnostics.

Output variant:

```rust
pub enum DiagEvent {
    // ... existing variants ...
    MetricBatch {
        window_ms: u16,
        cycle_count: u32,
        dc_time_start: i64,
        dc_time_end: i64,
        descriptors: Vec<MetricDescriptor>,
        samples: Vec<MetricSample>,
    }
}

pub struct MetricDescriptor {
    pub metric_id: u16,
    pub kind: MetricKind,
    pub is_monotonic: bool,
    pub name: String,
    pub unit: String,
    pub description: String,
    pub attributes: Vec<(String, String)>,
    pub histogram_bounds: Option<Vec<f32>>,
}

pub struct MetricSample {
    pub metric_id: u16,
    pub flags: u8,
    pub dc_time: i64,
    pub value: f32,
}
```

## Relationship to Per-Task Diagnostics

Both push-metric batches and per-task diagnostic batches share the same index
group (`IG_PUSH_DIAG = 0x4D42_4301`) but use different index offsets:

- `IO_PUSH_BATCH = 0`: Per-task diagnostics (event_type = 10), 80-byte header,
  24-byte samples, pre-aggregated exec-time/exceed stats.

- `IO_PUSH_METRIC_BATCH = 1`: User-defined metrics (event_type = 20), 32-byte
  header, 16-byte samples, announce descriptors once then ship samples.

A single ADS listener socket can multiplex both by decoding the event_type byte
after parsing the 2-byte header.

## Metric Emission to OTLP

The bridge (`crates/tc-otel-service/src/diagnostics_bridge.rs`) converts
`DiagEvent::MetricBatch` to `MetricEntry` values using the accumulated
descriptor table per `(ams_net_id, task_port)`:

- **Gauge** → OTLP Gauge data point.

- **Sum with is_monotonic=true** → OTLP monotonic Sum (Aggregation Temporality =
  Cumulative). Decoder accumulates deltas across batches.

- **Sum with is_monotonic=false** → OTLP non-monotonic Sum (Aggregation Temporality
  = Delta). Each batch's deltas are emitted as-is.

- **Histogram** → OTLP ExponentialHistogram or Histogram (depending on the
  exporter). Bounds are linearly spaced (classic histogram); buckets are
  populated from accumulated observations.

The `custom_metrics` configuration (via `MetricMapper` in the service) can
override name, unit, and attributes on a per-metric-name basis (same pattern as
in Plan 2).
