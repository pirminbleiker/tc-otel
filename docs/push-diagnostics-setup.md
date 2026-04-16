# Push-Based Diagnostics Setup

## Why Push?

Push-based diagnostics offer significant advantages over polling:

- **No missed edges**: Polling at 5 Hz (every 200 ms) misses transient cycle-exceed spikes and RT violations that occur and resolve within the sampling interval. Push captures every edge as it happens.
- **RT violations visible**: The ADS polling interface does not expose real-time violation flags; only push diagnostics can surface RT violations.
- **Rich metadata**: Task names, priorities, and configured cycle times are baked into PLC configuration and sent with every push event, avoiding asynchronous symbol lookups.
- **Efficient**: One snapshot per second per PLC is far more efficient than 5 Hz polling across all tasks.

## Architecture

Push diagnostics flow through the system as follows:

```
┌──────────────────────────────────────┐
│ PLC (with FB_PushDiag)               │
│  • Monitor all tasks                 │
│  • Detect cycle/RT violations        │
│  • Marshal binary snapshots+edges    │
└──────────────────┬───────────────────┘
                   │
                   │ ADSWRITE
                   │ IG=0x4D42_4301 (MBC\1)
                   │ IO=0 (snapshot), 1 (cycle edge), 2 (RT edge)
                   ↓
┌──────────────────────────────────────┐
│ tc-otel Router (ADS receiver)        │
│  • Port 16150 (default)              │
│  • Listens for push writes           │
└──────────────────┬───────────────────┘
                   │
                   ↓
┌──────────────────────────────────────┐
│ diagnostics_push Decoder             │
│  • Parse binary format (v1)          │
│  • Extract task data + edges         │
└──────────────────┬───────────────────┘
                   │
                   ↓
┌──────────────────────────────────────┐
│ Metrics Aggregator (OTel Bridge)     │
│  • Emit OTel metrics                 │
│  • Attach labels (task_name, etc)    │
└──────────────────┬───────────────────┘
                   │
                   ↓
┌──────────────────────────────────────┐
│ Prometheus                           │
│  (time-series database)              │
└──────────────────────────────────────┘
        ↓
    Grafana Dashboard
```

## Installing the PLC Function Block

The push-diagnostics function block is provided in the mbc_log4tc library.

**Steps:**

1. Ensure `library/mbc_log4tc_diag` is available in your project (see [FB_PushDiag README](../plc/mbc_log4tc_diag/README.md)).
2. Import `FB_PushDiag.TcPOU` and the three data type definitions into your TwinCAT 3 PLC project.
3. Add an instance of `FB_PushDiag` to your PLC's main program or a background task.
4. Configure:
   - `sTcOtelNetId` — AMS Net ID of the tc-otel service.
   - `nTcOtelPort` — ADS port (default 16150).
   - `nSnapshotIntervalMs` — Snapshot frequency (e.g., 1000 ms for 1 Hz).
   - `bEnabled` — Set to `TRUE` to activate push diagnostics.

**Example:**

```structured-text
VAR
    fbPushDiag : FB_PushDiag;
    stTcOtelNetId : T_AmsNetId := (
        ipaddr := [192, 168, 1, 100],
        netid := 1,
        port := 1
    );
END_VAR

// In your main cycle:
fbPushDiag(
    sTcOtelNetId := stTcOtelNetId,
    nTcOtelPort := 16150,
    nSnapshotIntervalMs := 1000,
    bEnabled := TRUE
);
```

For full details on the FB, see [FB_PushDiag README](../plc/mbc_log4tc_diag/README.md).

## Wire Format

Push diagnostics use a binary wire format over ADS Write with Index Group `0x4D42_4301` (ASCII "MBC\1").

Three event types are supported:

- **IO=0** — Task snapshot (periodic batched state)
- **IO=1** — Cycle-exceed edge (when execution time exceeds configured cycle time)
- **IO=2** — RT-violation edge (real-time violation detected)

For complete wire-format details including byte layouts, field descriptions, and size calculations, see [Push Diagnostics Wire Format](push-diagnostics-wire-format.md).

## Coexistence with Polling

tc-otel automatically manages the transition from polling to push diagnostics. **Polling is not disabled by default**, but the poller includes auto-detect logic:

- When a push event (snapshot or edge) is received from a target, an internal timer starts.
- If no push events are received within the **10-second window**, polling resumes automatically for that target.
- If push events arrive again within the window, polling is suspended and the timer resets.

This allows seamless fallback: if the PLC's `FB_PushDiag` is disabled or crashes, the poller silently takes over without reconfiguration.

**To fully disable polling**, explicitly set `poll_interval_ms: 0` or remove the target from the diagnostics config once you confirm push data is flowing steadily.

## Dashboard Panels

The tc-otel Grafana dashboard includes the following new panels for push-diagnostic metrics:

### New Panels (Push Diagnostics)

1. **Actual Last-Exec-Time per Task (µs)**
   - Metric: `tc_task_last_exec_time_us`
   - Type: timeseries
   - Shows measured execution time per task; useful to detect transient overruns.

2. **Per-Task Cycle-Exceed Counter (lifetime)**
   - Metric: `tc_task_cycle_exceed_counter_total`
   - Type: timeseries / stat
   - Monotonic counter; increments each time a task exceeds its cycle time.

3. **Cycle-Exceed Events Rate (events/min)**
   - PromQL: `rate(tc_task_cycle_exceed_edge_total[1m]) * 60`
   - Type: timeseries
   - High-frequency edge events; rate-normalized to events per minute.

4. **RT Violations (lifetime)** and **RT Violations Rate (events/min)**
   - Metrics: `tc_rt_rt_violation_counter_total` (stat) and `tc_rt_rt_violation_edge_total` (timeseries with rate)
   - Exclusive to push diagnostics; not available from polling.

5. **Configured vs Actual Cycle Time (µs)**
   - Metrics: `tc_task_cycle_time_configured_us` and `tc_task_last_exec_time_us` overlaid
   - Type: timeseries (two series per panel)
   - Overlay to visually compare expected vs. observed execution time.

### Annotations

The dashboard includes two annotation types:

- **Cycle exceeds** (red): Marks when cycle-exceed edges occur.
- **RT violations** (orange): Marks when RT-violation edges occur.

Both are sourced from the push-diagnostic edge counters and provide instant visual cues on the dashboard timeline.

## Troubleshooting

### No push data appearing in Prometheus

**Check:**

1. **tc-otel logs for decode errors:**
   ```bash
   docker logs tc-otel | grep -i "push\|decode\|0x4D42"
   ```
   Look for error messages indicating malformed push packets or unhandled versions.

2. **MQTT broker traffic (if using MQTT transport):**
   ```bash
   mosquitto_sub -h <broker_ip> -t '$SYS/#' | grep -i traffic
   # or inspect raw publish events with:
   mosquitto_sub -h <broker_ip> -t '#' -v
   ```
   Verify that writes with IG `0x4D42_4301` are arriving.

3. **PLC FB status:**
   - Confirm `FB_PushDiag.bEnabled = TRUE`.
   - Check that the ADS route to tc-otel is configured and active.
   - Monitor the snapshot timer to verify it's incrementing.

### Empty or null `task_name` in metrics

**Cause:** The PLC function block is not writing the task name field, or is writing null bytes that are not being converted to empty strings.

**Fix:**

1. Dump a raw frame using `mosquitto_sub` (or tcpdump if using TCP):
   ```bash
   mosquitto_sub -h <broker_ip> -t '#' --output-format=p --show-hidden | hexdump -C | grep "4D42"
   ```
2. Inspect the task name bytes (offset +0x34, 20 bytes) in the snapshot payload.
3. Ensure the PLC's `ST_PushDiagTaskRecord` is properly null-padded and that the task name is correctly copied from `TaskInfo.sTaskName`.

### Dashboard panels show no data

**Check:**

1. Verify push events are arriving: inspect tc-otel logs and confirm decode success.
2. Confirm Prometheus is scraping the metrics endpoint and ingesting the data:
   ```
   # In Prometheus UI: http://localhost:9090
   # Query: up{job="tc-otel"}
   ```
3. Verify the correct datasource (VictoriaMetrics or Prometheus) is configured in Grafana.
4. Check metric names match the Prometheus naming convention (e.g., `tc_task_last_exec_time_us`, not `tc.task.last_exec_time_us`).
