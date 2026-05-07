# Stop the tc-otel and VictoriaLogs processes (the tasks remain registered).
Get-Process -Name tc-otel,victoria-logs -ErrorAction SilentlyContinue |
    Stop-Process -Force
"Stopped."
