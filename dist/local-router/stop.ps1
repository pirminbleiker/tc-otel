# Stop tc-otel + Victoria backends. Tasks remain registered.
Get-Process -Name tc-otel,victoria-logs,victoria-metrics,victoria-traces -ErrorAction SilentlyContinue |
    Stop-Process -Force
"Stopped."
