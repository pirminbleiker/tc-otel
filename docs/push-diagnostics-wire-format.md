# Push Diagnostics Wire Format

## Overview

Push diagnostics let the PLC emit per-task execution data to tc-otel on every
cycle. Each task runs its own sampler (`FB_TcOtelTaskDiag` — one instance per
task, driven from `PRG_TaskLog.Call()`). Samples are collected into a
1024-entry ring buffer and flushed as a single **batch frame** whenever the
per-task aggregation window expires.

Why per-task oversampling:

- **No cycle loss.** Every cycle is captured with `cycle_count` + `dc_time`,
  so cycle-exceed and RT-violation events that happen on the same cycle as
  an unrelated sample cannot overwrite each other.
- **1:1 log correlation.** Each sample carries the same timestamp the
  logger writes into log entries — tc-otel can match a flagged cycle to
  the exact log line produced on that cycle.
- **Pre-aggregated.** Min/max/avg exec time and exceed/rtv counts for the
  window are computed on the PLC and shipped in the batch header.
- **Runtime-tunable window.** tc-otel writes back to
  `PRG_TaskLog.aTaskDiagConfig[n]` via standard ADS symbol access to
  adjust the window, enable flag, and divider on the fly.

## Transport

All push-diagnostic events are sent as ADS Write commands to:

- **Index group**: `IG_PUSH_DIAG = 0x4D42_4301` (ASCII "MBC\1" in
  little-endian; chosen for debug-friendliness and collision safety)
- **Target port**: The dedicated tc-otel listener port (default 16150)
- **Index offset**: `IO_PUSH_BATCH = 0` — per-task diagnostic batch

A sibling index group `IG_PUSH_CONFIG = 0x4D42_4302` is reserved for
outbound writes from tc-otel back to the PLC; the PLC does not need a
custom handler for these — tc-otel writes per-task config slots by
symbol name (`PRG_TaskLog.aTaskDiagConfig[n]`).

## Wire Format

All offsets and sizes are in bytes. All integers are **little-endian**. The
on-wire layout is declared in the PLC DUTs with `{attribute 'pack_mode' := '1'}`
so the structs serialize exactly as laid out.

### Batch (IO_PUSH_BATCH)

Total size: **80 + (24 × sample_count)** bytes. Maximum sample_count is
**1024** (ring depth).

#### Batch header — 80 bytes

```
+0x00  u8    version               = 2
+0x01  u8    event_type            = 10  (batch)
+0x02  u16   reserved0             = 0
+0x04  u32   task_obj_id           (PLC internal task object ID)
+0x08  u16   task_port             (AMS port of this task, e.g. 350, 351)
+0x0A  u16   window_ms             (aggregation window used for this batch)
+0x0C  u16   sample_count          (≤ 1024)
+0x0E  u16   reserved1             = 0
+0x10  u32   cycle_count_start     (cycle at the first sample)
+0x14  u32   cycle_count_end       (cycle at the last sample)
+0x18  i64   dc_time_start         (FILETIME ticks, first sample)
+0x20  i64   dc_time_end           (FILETIME ticks, last sample)
+0x28  u32   exec_time_min_us      (min observed exec time)
+0x2C  u32   exec_time_max_us      (max observed exec time)
+0x30  u32   exec_time_avg_us      (mean exec time)
+0x34  u32   cycle_exceed_count    (cycles with CycleTimeExceeded in window)
+0x38  u32   rt_violation_count    (cycles with RTViolation in window)
+0x3C  char  task_name[20]         (null-padded UTF-8)
```

#### Sample record — 24 bytes each

```
+0x00  u32   cycle_count           (task cycle counter at this sample)
+0x04  u32   exec_time_us          (cycle exec time, microseconds)
+0x08  i64   dc_time               (FILETIME — 100 ns ticks since 1601; same
                                     format log entries use for plc_timestamp
                                     so a flagged cycle is byte-identical to
                                     its log line's timestamp)
+0x10  u8    flags                 (see below)
+0x11  u8[7] reserved              (pad to 24-byte record)
```

#### Sample flag bits

- Bit 0: `SAMPLE_FLAG_CYCLE_EXCEED = 1 << 0` — cycle-exceed this cycle
- Bit 1: `SAMPLE_FLAG_RT_VIOLATION = 1 << 1` — rt-violation this cycle
- Bit 2: `SAMPLE_FLAG_FIRST_CYCLE = 1 << 2` — first cycle after task start
- Bit 3: `SAMPLE_FLAG_OVERFLOW = 1 << 3` — ring overflowed before this sample

## Size and Bandwidth Estimation

Frames are per-task and emitted once per window. Window is configurable
per task via `aTaskDiagConfig[n].window_ms`.

| Task cycle | Window  | Samples/batch | Batch size | Per-sec rate |
| ---------- | ------- | ------------- | ---------- | ------------ |
| 1 ms       | 100 ms  | 100           | 2 480 B    | 10 batches   |
| 1 ms       | 1000 ms | 1000          | 24 080 B   | 1 batch      |
| 100 µs     | 100 ms  | 1024 (cap)    | 24 656 B   | 10 batches   |

Worst case per task (1024 samples): **24 656 bytes**, well below the ADS
write limit (~8 MB). The ring stores at most 1024 samples; once full,
the oldest slot is overwritten and the next stored sample carries
`SAMPLE_FLAG_OVERFLOW`.

## Versioning

The `version` field at `+0x00` identifies the wire format. Current value
is **2** (batch format). Version **1** was the earlier snapshot+edge
format and is no longer emitted or decoded. Decoders must:

1. **Check version on entry.** If `version != 2`, drop the whole frame.
2. **Check event_type.** Must equal `10` for batch frames.
3. **Treat reserved fields as opaque.** Senders set them to 0; receivers
   must not interpret them.

## Rust DiagEvent Variant

```rust
DiagEvent::TaskDiagBatch {
    task_port: u16,
    task_name: String,
    task_obj_id: u32,
    window_ms: u16,
    cycle_count_start: u32,
    cycle_count_end: u32,
    dc_time_start: i64,
    dc_time_end: i64,
    exec_time_min_us: u32,
    exec_time_max_us: u32,
    exec_time_avg_us: u32,
    cycle_exceed_count: u32,
    rt_violation_count: u32,
    samples: Vec<DiagSample>,
}

struct DiagSample {
    cycle_count: u32,
    exec_time_us: u32,
    dc_time: i64,
    flags: u8,
}
```

## Metric Emission

Each batch fans out to the following metrics. Per-task metrics carry
`task_port` and `task_name` attributes plus `ams_net_id`.

Window aggregates:

- `tc.task.exec_time_min_us` (gauge)
- `tc.task.exec_time_max_us` (gauge)
- `tc.task.exec_time_avg_us` (gauge)
- `tc.task.window_ms` (gauge)
- `tc.task.sample_count` (gauge)
- `tc.task.cycle_count` (sum, monotonic) — end-of-window value
- `tc.task.cycle_exceed_window` (sum, non-monotonic) — exceeds in this window
- `tc.task.rt_violation_window` (sum, non-monotonic) — rt-violations in this window
- `tc.task.sample_buffer_overflow` (gauge) — 1 if any sample carried overflow flag

Per-flagged-sample edge events (one metric per flagged sample, carrying
`cycle_count`, `dc_time_ft`, `exec_time_us` attributes for log correlation):

- `tc.task.cycle_exceed_edge` (sum, non-monotonic, value = 1)
- `tc.task.rt_violation_edge` (sum, non-monotonic, value = 1)

Prometheus `rate()` over the edge metrics gives a clean per-task
exceed/violation rate; the attributes let a user jump from a spike to the
exact log line (`plc.cycle_count` / `plc.timestamp`) produced on that cycle.
