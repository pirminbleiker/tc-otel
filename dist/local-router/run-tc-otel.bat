@echo off
rem Wrapper used by the "tc-otel" Scheduled Task. Sets RUST_LOG so the
rem service's tracing output covers all tc-otel crates (this can be removed
rem once the binary respects logging.log_level globally).
set RUST_LOG=info
cd /d C:\tc-otel
tc-otel.exe -c C:\tc-otel\config.json > C:\tc-otel\tcotel-stdout.log 2> C:\tc-otel\tcotel-stderr.log
