# Grafana data source — VictoriaLogs on the target

`install.ps1` runs VictoriaLogs as a Scheduled Task on the target,
exposed at TCP/9428 (firewall opened automatically). Grafana running on
any other machine that can reach the target's IP can use it as a
log data source.

## 1. Install the VictoriaLogs Grafana plugin

In Grafana → **Administration → Plugins**, search for
**"VictoriaLogs"** (publisher: VictoriaMetrics) and click *Install*.
Grafana ≥ 10 picks it up without restart.

The plugin's docs:
<https://docs.victoriametrics.com/victorialogs/victorialogs-datasource/>.

> Alternative: VictoriaLogs also speaks a Loki-compatible API at
> `/select/logsql/query` — you can use Grafana's built-in **Loki**
> datasource against `http://<target>:9428` if you'd rather not install
> the dedicated plugin. Native plugin is recommended for `LogsQL` syntax.

## 2. Add the data source

**Connections → Data sources → Add data source → VictoriaLogs**

| Field | Value |
|---|---|
| Name | `tc-otel (CX1800)` (or your target name) |
| URL  | `http://<target-ip>:9428` |
| Access | Server (default) |

Leave auth empty unless you front VL with a reverse proxy.
Click **Save & test** — should return *"Data source is working"*.

## 3. Built-in fields tc-otel emits

Each log entry from tc-otel arrives in VictoriaLogs with at least:

| Field | Example | Meaning |
|---|---|---|
| `_msg` | `Try F_Log with args 5 and TOD#16:37:10.016` | log message text |
| `_time` | `2026-05-06T16:37:10.016Z` | when the PLC produced it |
| `host.name` | `plc-172.28.41.37.1.1` | source NetId |
| `severity_text` | `INFO`, `WARN`, ... | log level from `E_LogLevel` |
| `service.name` | `tc-otel` | the producer |
| Plus any context properties added via `WithContext(...)` in the PLC |

## 4. Example queries (LogsQL)

Recent logs from a specific PLC task:
```
host.name:"plc-172.28.41.37.1.1" AND task.name:"PlcTask"
```

Errors only, last hour:
```
severity_text:"ERROR" | sort by (_time) desc
```

Free-text search through messages:
```
_msg:"connection lost"
```

The full LogsQL reference:
<https://docs.victoriametrics.com/victorialogs/logsql/>.

## 5. Direct VictoriaLogs Web UI (no Grafana)

`http://<target-ip>:9428/select/vmui/` — built-in log viewer, useful for
quick smoke tests when Grafana isn't reachable.

## 6. Storage / retention

- Data path: `C:\victoria-logs\data` on the target.
- Default retention: **7 days** (VL default `-retentionPeriod=7d`).
- To change retention or limit disk use, edit
  `C:\victoria-logs\run.bat` and add `-retentionPeriod=30d` or
  `-storage.maxDiskSpaceUsageBytes=10GB`, then restart the task:
  ```powershell
  schtasks /End /TN "VictoriaLogs"
  schtasks /Run /TN "VictoriaLogs"
  ```
