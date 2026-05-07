# Start the tc-otel and VictoriaLogs Scheduled Tasks.
& schtasks /Run /TN "VictoriaLogs"
& schtasks /Run /TN "tc-otel"
Start-Sleep -Seconds 3
Get-Process -Name tc-otel,victoria-logs -ErrorAction SilentlyContinue |
    Format-Table Id, ProcessName -AutoSize
