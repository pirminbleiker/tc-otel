# Push-Based Diagnostics Setup

## Why Push?

Push-based diagnostics collect per-cycle execution data on the PLC and ship it
to tc-otel in batches. Compared to 5 Hz polling this buys:

- **No missed events.** Every cycle is sampled with its own `cycle_count` and
  `dc_time`; cycle-exceed and RT-violation cannot overwrite each other even
  when they happen on the same cycle.
- **RT violations visible.** The polled diagnostics interface does not expose
  `RTViolation`. The push samples do.
- **1:1 log correlation.** Each sample carries the same timestamp that log
  entries use — a flagged cycle can be matched to the log line written on
  that cycle.
- **Runtime-tunable.** The aggregation window is a plain PLC variable
  (`PRG_TaskLog.aTaskDiagConfig[n].window_ms`) that tc-otel can rewrite via
  ADS symbol access — no PLC rebuild needed to change the sampling rate or
  disable a task.

## Architecture

```
┌──────────────────────────────────────────────┐
│ PLC task cycle                               │
│   PRG_TaskLog.Call()                         │
│     → aTaskLogger[nIdx].Call()               │
│     → aTaskRtcTime[nIdx].Call()              │
│     → aTaskDiag[nIdx].Call()                 │
│         • store sample in 1024-slot ring     │
│         • update min/max/avg + exceed/rtv    │
│         • on window expiry: flush batch      │
└──────────────────┬───────────────────────────┘
                   │ ADSWRITE (non-blocking)
                   │ IG=0x4D42_4301 (MBC\1)
                   │ IO=0 (batch)
                   ↓
┌──────────────────────────────────────────────┐
│ tc-otel router (port 16150)                  │
│   decode_batch → DiagEvent::TaskDiagBatch    │
└──────────────────┬───────────────────────────┘
                   ↓
┌──────────────────────────────────────────────┐
│ diagnostics_bridge                           │
│   • aggregate gauges / counters              │
│   • per-flagged-sample edge metrics with     │
│     cycle_count + dc_time attributes         │
└──────────────────┬───────────────────────────┘
                   ↓
          Prometheus / VictoriaMetrics
                   ↓
              Grafana dashboard
```

## Using Push Diagnostics on the PLC

Push diagnostics ride along with the Log4TC library — there is no separate
FB to instantiate. Every task that already calls `PRG_TaskLog.Call()` gets a
per-task sampler for free.

**Minimum setup:**

```structured-text
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.Init('127.0.0.1.1.1'); // tc-otel AMS net id
END_IF

PRG_TaskLog.Call();
```

`Init` wires the push-diagnostic collector to the tc-otel endpoint on the
default port (16150) with default config (`window_ms=100`, `enabled=TRUE`,
`divider=1`).

**Custom window / port:**

```structured-text
IF _TaskInfo[GETCURTASKINDEXEX()].FirstCycle THEN
    PRG_TaskLog.InitDiag(
        sAmsNetId  := '127.0.0.1.1.1',
        nPort      := 16150,
        nWindowMs  := 250,   // flush every 250 ms
        bEnabled   := TRUE,
        nDivider   := 1      // sample every cycle
    );
END_IF
```

On a sub-100 µs task that would fill the 1024-slot ring in <100 ms, set
`nDivider` to e.g. `10` so the sampler keeps every tenth cycle.

## Runtime Reconfiguration from tc-otel

tc-otel (or any ADS client) can update the config slot for task *n* by
writing the `ST_PushDiagConfig` struct at
`PRG_TaskLog.aTaskDiagConfig[n]`. Fields:

| Offset | Field       | Type  | Meaning                                |
| ------ | ----------- | ----- | -------------------------------------- |
| 0x00   | `window_ms` | UDINT | flush interval; 0 disables             |
| 0x04   | `enabled`   | BOOL  | TRUE = collect and push                |
| 0x05   | `divider`   | USINT | 0/1 = every cycle, N = keep every N-th |
| 0x06   | reserved    | UINT  | always 0                               |

Changes take effect on the next task cycle — the collector reads the slot
every call.

## Wire Format

Push diagnostics use a binary wire format over ADS Write with index group
`0x4D42_4301` (ASCII "MBC\1") and index offset `0` (batch). Wire version is
`2`; batches are 80-byte header + N × 24-byte samples, with N ≤ 1024.

For complete byte-level layout, field descriptions, and bandwidth math, see
[Push Diagnostics Wire Format](push-diagnostics-wire-format.md).

## Coexistence with Polling

tc-otel still supports 5 Hz polling as a fallback. The poller includes
auto-detect logic:

- When a push event arrives from a target, an internal timer starts.
- If no push events are received within 10 seconds, polling resumes
  automatically for that target.
- If push events arrive again, polling suspends and the timer resets.

Seamless fallback: if the PLC library is updated, paused, or crashes, the
poller silently takes over without reconfiguration.

To fully disable polling, set `poll_interval_ms: 0` on the target or remove
it from the diagnostics config once push data is flowing steadily.

## Dashboard Panels

Push-diagnostic metrics emitted by `diagnostics_bridge`:

**Per-window aggregates** (one data point per task per window):

- `tc_task_exec_time_min_us`, `tc_task_exec_time_max_us`,
  `tc_task_exec_time_avg_us` (gauges)
- `tc_task_window_ms` (gauge) — the effective window length
- `tc_task_sample_count` (gauge) — samples in the last batch
- `tc_task_cycle_count_total` (monotonic counter) — `rate()` gives cycles/sec
- `tc_task_cycle_exceed_window_total` and `tc_task_rt_violation_window_total`
  (non-monotonic counters) — window deltas; use `sum_over_time()` for
  cumulative totals
- `tc_task_sample_buffer_overflow` (gauge) — 1 if any sample carried the
  overflow flag (data gap occurred)

**Per-flagged-sample edge events** (one data point per exceed / rtv cycle):

- `tc_task_cycle_exceed_edge_total` — each emission carries `cycle_count`,
  `dc_time_ft`, `exec_time_us` attributes. `dc_time_ft` is FILETIME ticks
  (100 ns since 1601), byte-identical to log entry `plc_timestamp`.
- `tc_task_rt_violation_edge_total` — same attribute set

The `cycle_count` + `dc_time_ns` attributes make it possible to jump from a
spike on the dashboard directly to the log line produced on that exact
cycle.

## Troubleshooting

### No push data appearing in Prometheus

1. Check tc-otel logs for decode errors:
   ```bash
   docker logs tc-otel | grep -i "push\|decode\|0x4D42"
   ```
2. For MQTT transport, tail broker traffic:
   ```bash
   mosquitto_sub -h <broker_ip> -t '#' -v
   ```
   Verify writes with `IG=0x4D42_4301` are arriving.
3. Inspect PLC state:
   - `PRG_TaskLog.aTaskDiagConfig[n].enabled` is TRUE.
   - `PRG_TaskLog.aTaskDiagConfig[n].window_ms` > 0.
   - ADS route to tc-otel is configured and active.

### Empty or null `task_name` in metrics

The collector pulls `task_name` from `TwinCAT_SystemInfoVarList._TaskInfo[n].TaskName`
and null-pads to 20 bytes. If the metric label is empty:

1. Dump a raw frame (MQTT):
   ```bash
   mosquitto_sub -h <broker_ip> -t '#' --output-format=p --show-hidden | hexdump -C | grep 4D42
   ```
2. Inspect bytes at header offset `+0x3C` (20 bytes).
3. Confirm the PLC task has a non-empty name configured in the system manager.

### `tc_task_sample_buffer_overflow = 1`

The ring filled up before the window flushed — either the window is too
large for the task's cycle rate, or ADSWRITE is backed up. Shorten
`window_ms`, raise `divider`, or check the network path to tc-otel.

### Dashboard panels show no data

1. Verify push events arrive: check tc-otel logs for successful decodes.
2. In Prometheus UI, query `up{job="tc-otel"}` and one of the new metric
   names (Prometheus converts dots to underscores:
   `tc_task_exec_time_avg_us`, not `tc.task.exec_time_avg_us`).
3. Confirm Grafana points at the right datasource.
