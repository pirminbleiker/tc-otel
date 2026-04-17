# Configuration Examples

This directory contains configuration and setup examples for different deployment scenarios.

## Config Variants

| Scenario | File | When to use |
|----------|------|-------------|
| Local TCP development | `config/tcp.json` | Running tc-otel on your development box alongside TwinCAT in a VM. Simple point-to-point ADS connection. |
| Docker Compose stack | `config/tcp-docker.json` | Using the bundled `docker-compose.observability.yml` with Grafana, Tempo, Loki, Prometheus all containerized. |
| MQTT transport | `config/mqtt.json` | PLCs publish traces/metrics via MQTT broker; tc-otel subscribes. Useful for multi-PLC farms or WAN scenarios. |
| Minimal / starter | `config/minimal.json` | Copy-paste reference. Bare-minimum options with inline comments. Start here if nothing else fits. |

## OTEL Collector

`otel-collector/config.yml` — Example configuration for the OTEL Collector (contrib distribution). Used by docker-compose for local metric/trace aggregation. Edit if you need additional processors (sampling, batching, etc.) or exporters beyond OTLP.

## TwinCAT Routes

`twincat/StaticRoutes.xml` — Example ADS static routes for TwinCAT runtime. Import into your Engineering System if tc-otel runs on a separate network segment. Adjust IP addresses and Net IDs to match your environment.
