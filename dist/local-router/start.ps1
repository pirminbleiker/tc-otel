# Start Victoria backends + tc-otel.
foreach ($t in @("VictoriaLogs","VictoriaMetrics","VictoriaTraces")) {
    if ((schtasks /Query /TN $t 2>&1) -notmatch "ERROR") {
        & schtasks /Run /TN $t | Out-Null
    }
}
Start-Sleep -Seconds 3
& schtasks /Run /TN "tc-otel" | Out-Null
Start-Sleep -Seconds 2
Get-Process -Name tc-otel,victoria-logs,victoria-metrics,victoria-traces -ErrorAction SilentlyContinue |
    Format-Table Id, ProcessName -AutoSize
