# Push Diagnostics Wire Format

## Overview

Push diagnostics allow PLC runtimes to proactively emit task snapshots and edge events (cycle-exceed, RT-violation) via ADS Write instead of relying on 5 Hz polling. This approach captures transient conditions that would be lost or invisible to the polled diagnostic subsystem:

- **Cycle-exceed edges**: High-frequency sampling (every cycle) detects spikes that disappear within the polling interval (~200 ms at 5 Hz).
- **RT-violation edges**: RT violations are not exposed by the polling interface at all; only push diagnostics can surface them.
- **Task names**: Names are baked into PLC configuration and sent with every push, avoiding async GVL lookups.

## Transport

All push-diagnostic events are sent as ADS Write commands to:

- **Index group**: `IG_PUSH_DIAG = 0x4D42_4301` (ASCII "MBC\1" in little-endian; chosen for debug-friendliness and collision safety)
- **Target port**: The dedicated tc-otel listener port (default 16150)
- **Index offset**: Determines event type:
  - `IO_PUSH_SNAPSHOT = 0` — task snapshot
  - `IO_PUSH_CYCLE_EXCEED_EDGE = 1` — cycle-exceed edge
  - `IO_PUSH_RT_VIOLATION_EDGE = 2` — RT-violation edge

## Wire Format

All offsets and sizes are in bytes. All integers are **little-endian**.

### Task Snapshot (IO_PUSH_SNAPSHOT)

Sent at every PLC cycle (or on demand). Contains configuration and current state for all tasks in a single message. Total size is **16 + (72 × num_tasks)** bytes.

```
+0x00  u8    version                = 1
+0x01  u8    event_type             = 0  (snapshot)
+0x02  u16   num_tasks              (number of per-task records)
+0x04  u64   plc_timestamp_ns       (DC clock nanoseconds; 0 if not available)
+0x0C  u32   reserved = 0           (padding / future use)
+0x10  per-task record × num_tasks, 72 B each:
  +0x00  u32   task_obj_id          (PLC internal task object ID)
  +0x04  u32   ads_port             (AMS port of this task, e.g. 350, 351)
  +0x08  u32   priority             (configured task priority)
  +0x0C  u32   cycle_time_us        (configured cycle time, microseconds)
  +0x10  u32   last_exec_time_us    (measured exec time, microseconds)
  +0x14  u32   reserved = 0         (alignment / future use)
  +0x18  u64   cycle_count          (monotonic cycle counter since boot)
  +0x20  u64   cycle_exceed_count   (monotonic counter, increments on exceed)
  +0x28  u64   rt_violation_count   (monotonic counter, increments on RT violation)
  +0x30  u32   flags                (see Flags section below)
  +0x34  char  task_name[20]        (null-padded UTF-8, e.g. "PlcTask\0\0\0\0...")
```

**Flags field (+0x30):**
- Bit 0: `PUSH_FLAG_CYCLE_EXCEED_NOW = 1 << 0` — cycle exceeded on this cycle
- Bit 1: `PUSH_FLAG_RT_VIOLATION_NOW = 1 << 1` — RT violation on this cycle
- Bit 2: `PUSH_FLAG_FIRST_CYCLE = 1 << 2` — first cycle after PLC start

### Cycle-Exceed Edge (IO_PUSH_CYCLE_EXCEED_EDGE)

Sent when a task's execution time exceeds its configured cycle time. Total size is **44 bytes**.

```
+0x00  u8    version       = 1
+0x01  u8    event_type    = 1  (cycle-exceed edge)
+0x02  u16   reserved = 0
+0x04  u32   ads_port      (AMS port of the task)
+0x08  u64   cycle_count   (cycle counter at the triggering cycle)
+0x10  u32   last_exec_time_us
+0x14  u32   reserved = 0
+0x18  char  task_name[20] (null-padded UTF-8)
```

### RT-Violation Edge (IO_PUSH_RT_VIOLATION_EDGE)

Sent when a task violates real-time guarantees (e.g., missed deadline, preemption by higher-priority work). Total size is **44 bytes**.

```
+0x00  u8    version       = 1
+0x01  u8    event_type    = 2  (RT-violation edge)
+0x02  u16   reserved = 0
+0x04  u32   ads_port      (AMS port of the task)
+0x08  u64   cycle_count   (cycle counter at the triggering cycle)
+0x10  u32   last_exec_time_us
+0x14  u32   reserved = 0
+0x18  char  task_name[20] (null-padded UTF-8)
```

## Size and Bandwidth Estimation

For a typical PLC with **3 tasks** pushing snapshots at **1 Hz** (one per second):

- Snapshot: 16 + (72 × 3) = **232 bytes per message**
- Interval: 1000 ms
- Bandwidth: 232 B / 1000 ms ≈ **0.23 KB/s per PLC**

Edge events are asynchronous and typically low-frequency; bandwidth depends on task behavior.

## Versioning

The `version` field (byte +0x00) is set to **1** for the current format. Decoders must:

1. **Check version on entry**: If `version != 1`, drop the entire message and log a warning.
2. **Ignore unknown versions**: Do not attempt to parse messages with unrecognized version numbers.
3. **Forward compatibility**: Reserved fields must be ignored (set to 0 by senders, read but not interpreted by receivers).

This ensures safe evolution when new fields are added in future versions.

## Rust DiagEvent Variants

The decoder produces these variant types (in addition to existing polling-based variants):

```rust
DiagEvent::TaskSnapshot {
    task_port: u16,
    task_name: String,
    priority: u32,
    cycle_time_configured_us: u32,
    last_exec_time_us: u32,
    cycle_count: u64,
    cycle_exceed_count: u64,
    rt_violation_count: u64,
    flags: u32,
    plc_timestamp_ns: u64,
}

DiagEvent::CycleExceedEdge {
    task_port: u16,
    task_name: String,
    cycle_count: u64,
    last_exec_time_us: u32,
}

DiagEvent::RtViolationEdge {
    task_port: u16,
    task_name: String,
    cycle_count: u64,
    last_exec_time_us: u32,
}
```

## Metric Emission

Decoders emit the following metrics from push-diagnostic events (metric names and types are listed for reference; emission happens in the metrics pipeline):

- `tc.task.last_exec_time_us` (gauge) — last measured execution time
- `tc.task.cycle_time_configured_us` (gauge) — configured cycle time
- `tc.task.priority` (gauge) — task priority value
- `tc.task.cycle_count` (sum, monotonic) — cumulative cycle counter
- `tc.task.cycle_exceed_counter` (sum, monotonic, per-task) — cumulative exceed count
- `tc.rt.rt_violation_counter` (sum, monotonic) — cumulative RT violation count
- `tc.task.cycle_exceed_edge_total` (sum, monotonic) — edge occurrence counter
- `tc.rt.rt_violation_edge_total` (sum, monotonic) — edge occurrence counter

Metrics carry tags: `task_name`, `task_port`, `plc_net_id`, etc. (set by the metrics aggregator).
