@echo off
rem Wrapper used by the "otelcol" Scheduled Task. Routes tc-otel's
rem OTLP-JSON metrics through Prometheus remote_write into VictoriaMetrics.
cd /d C:\otelcol
otelcol-contrib.exe --config=C:\otelcol\config.yml > C:\otelcol\otelcol-stdout.log 2> C:\otelcol\otelcol-stderr.log
