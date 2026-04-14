# Critical Review: GasTown-Era Features

## Scope

This review covers all features added during the GasTown agent framework era, spanning **87 commits** tagged with `gt-*` or `to-*` prefixes, from the earliest foundational work (to-754.1, approximately February 2026) through the most recent security hardening (to-76j.3, April 2026). Overlay documentation from CLAUDE.md files was stripped in commit cf0838a, necessitating this external review.

**Branch context**: This analysis is based on `origin/feat/ads-over-mqtt-foundation` (commit f6fe80a), which includes all GasTown-era features plus the foundational ADS-over-MQTT transport work.

## Feature Inventory

| Feature | Primary Commit(s) | Code Location | Unit Tests | Integration Tests | e2e Recipe | Known Gaps |
|---------|-------------------|---------------|------------|-------------------|-----------|-----------|
| ADS logs pipeline (v1/v2/v6 binary protocol) | 51f2342, a48834f | tc-otel-ads/src/parser.rs, tc-otel-core/src/models.rs | OK in ads_parser bench | OK 18+ tests in metric_sampling.rs, log_trace_correlation.rs | Untested | No PLC-side validation, tests use synthetic buffers |
| PLC CPU/memory metrics | 740333e, d4e3947 | tc-otel-service/src/system_metrics.rs | None | OK 22 tests in plc_system_metrics.rs | Untested | Collects cycle time only, not actual CPU/memory |
| ADS connection health metrics | dc974ec, affc521 | tc-otel-ads/src/health_metrics.rs | None | OK 15 tests in ads_connection_health.rs | Untested | No real network testing, mocked in tests |
| Custom metric definitions | 41342b5, d60728f | tc-otel-core/src/metric_mapper.rs | None | OK 27 tests in custom_metric_mapping.rs | Untested | No config validation for invalid paths |
| Task cycle time metrics | 8b1b9a6, 9877a05 | tc-otel-service/src/cycle_time.rs | None | Implicit in plc_system_metrics.rs | Untested | Jitter calculation unvalidated |
| OTLP traces export | 547ebcd, ee5aa2d | tc-otel-export/src/exporter.rs | OK 20+ payload tests | Implicit in span tests | Untested | No backend integration tested |
| Log-trace correlation | a48834f, 86ee188 | tc-otel-ads/src/parser.rs type 0x06 | Partial in parser | OK 35+ tests in log_trace_correlation.rs | Untested | No e2e span-log ordering validation |
| Prometheus/OTEL/Datadog exporters | 6cda780, 81d92ac | tc-otel-service/src/dispatcher.rs | None | OK 12 tests in metric_export.rs | Untested | All tests mock HTTP endpoints |
| Runtime test skeleton (docker-compose) | 0a6aa84 | tests/runtime/docker-compose.yml | N/A | N/A skeleton only | Untested | No actual test cases wired |
| ADS symbol discovery & REST API | 94fd61c | tc-otel-service/src/symbol_discovery.rs | None | OK 18 tests in symbol_discovery.rs | Untested | Tests use fake registry, no real ADS upload |
| Config hot-reload | ba90852, fc74970 | tc-otel-core/src/config_watcher.rs | None | Implicit only | Untested | Not exercised in any test |
| Web UI for status/diagnostics | 26597b2, f6f0ee9, 8dd5fd5 | tc-otel-service/src/web/ | None | None | Untested | Scope unknown, no tests |
| Motion sequence tracing | 74aaa4a, 46e1825 | tc-otel-ads/src/parser.rs MOTION type | None | OK 21 tests in span_motion_tracing.rs | Untested | Synthetic data only |
| State machine transition tracing | 7b0c47c, bd845f4 | tc-otel-ads/src/protocol.rs | None | OK 19 tests in span_state_machine_tracing.rs | Untested | No real PLC state machine tested |
| Recipe execution tracing | 47a30d3, 2ef1765 | Span infrastructure | None | OK 18 tests in span_recipe_tracing.rs | Untested | Synthetic data only |
| Distributed tracing (multi-PLC) | e7a799a | Exporter span correlation | None | OK 24 tests in span_distributed_tracing.rs | Untested | Static net_id assumption only |
| TLS/security config validation | 6eac74b | tc-otel-core/src/config.rs | Partial | OK 16 tests in security_receiver_tls.rs | Untested | No actual TLS handshake tested |
| Security message limits | e437e4b | tc-otel-ads/src/parser.rs MAX_* | Implicit in parser | OK 22 tests in security_message_limits.rs | Untested | No fuzzing or DoS testing |
| Connection limits & management | be987ad | tc-otel-ads/src/connection_manager.rs | None | OK 14 tests in security_connection_limits.rs | Untested | Mock time-based only |
| gRPC receiver for OTLP logs | 1e9efcb | tc-otel-export/src/grpc.rs | OK 33 unit tests | Implicit only | Untested | No real gRPC client tested |
| Helm chart for K8s deployment | 9e3a4cd | charts/tc-otel/ | N/A | None | Untested | No K8s e2e tested |
| ADS-over-MQTT transport (foundation) | f6fe80a | tc-otel-ads/src/transport.rs, mqtt.rs | OK 1 fixture test | None yet | Untested | No real MQTT broker tested |

## Test Coverage Summary

**Strong coverage (18+ integration tests each):**
- Metric sampling (23 tests)
- Log-trace correlation (35+ tests)
- Span tracing variants (21-24 tests each)
- Security testing (22 message limit + 14 connection + 16 TLS)

**Weak coverage:**
- System metrics (no unit tests)
- Config hot-reload (no tests)
- Web UI (no tests)
- MQTT transport (1 fixture test only)

**No e2e coverage:**
- None validated against real tc31-xar-base PLC or external backends

## Top 5 Risk Areas

### 1. Metrics Feature Name-Behavior Mismatch [HIGH]
Feature named "PLC CPU and memory utilization metrics" but collects only task cycle times, not actual CPU/memory data. **Major gap between feature intent and implementation.**

### 2. MQTT Transport Unvalidated Against Real Brokers [HIGH]
Foundation implemented but never tested with actual MQTT broker. Topic routing, QoS, failover all untested.

### 3. Config Hot-Reload Not Exercised [HIGH]
Code exists but zero test coverage. Unknown failure modes during reload.

### 4. Web UI Feature Unvalidated [HIGH]
No tests, no documentation. Scope unknown. Assumed functional but never confirmed.

### 5. No Real PLC Integration Testing [MEDIUM-HIGH]
All tests use synthetic binary buffers. No validation against actual tc31-xar-base or real ADS/AMS frames.

## Recommendations

**IMMEDIATE (block production):**
1. Clarify CPU/memory metrics naming or implement actual hardware monitoring
2. Add MQTT broker integration test (docker-compose + mosquitto)
3. Validate config hot-reload with actual file changes
4. Document and test Web UI feature scope

**SHORT-TERM (1-2 weeks):**
5. Establish real PLC baseline with tc31-xar-base fixtures
6. Add config schema validation for custom metric mappings
7. Validate exporters against real Prometheus/Jaeger/Datadog backends

**MEDIUM-TERM (ongoing):**
8. Wire runtime test cases using docker-compose skeleton
9. Test multi-instance failover and net_id reuse scenarios
10. Document feature readiness status in release notes

## Conclusion

GasTown-era features represent substantial progress: core protocol parsing is robust, metric/span/log infrastructure well-structured, integration test coverage solid for synthetic scenarios.

**CRITICAL GAPS:**
- Feature-intent mismatch (CPU/memory vs cycle time)
- No real PLC or backend validation
- MQTT transport unproven
- Config hot-reload untested
- Web UI scope unknown

**RECOMMENDATION:** Beta-suitable for controlled environments only. Not production-ready without addressing the five high-risk areas. Prioritize real PLC integration testing and MQTT broker validation.
