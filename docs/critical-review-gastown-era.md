# Critical Review: GasTown-Era Features

## Scope

This review covers all features added during the GasTown agent framework era, spanning **87 commits** tagged with `gt-*` or `to-*` prefixes, from the earliest foundational work (to-754.1, approximately February 2026) through the most recent security hardening (to-76j.3, April 2026). Overlay documentation from CLAUDE.md files was stripped in commit cf0838a, necessitating this external review.

**Branch context**: This analysis is based on `origin/feat/ads-over-mqtt-foundation` (commit f6fe80a), which includes all GasTown-era features plus the foundational ADS-over-MQTT transport work.

## Feature Inventory

| Feature | Primary Commit(s) | Code Location | Unit Tests | Integration Tests | e2e Recipe | Known Gaps |
|---------|-------------------|---------------|------------|-------------------|-----------|-----------|
| **ADS logs pipeline** (v1/v2/v6 binary protocol) | Multiple: 51f2342, a48834f | `tc-otel-ads/src/parser.rs` (log v1/v2/v6 arms), `tc-otel-core/src/models.rs` (LogEntry) | ✓ Parser unit tests in ads_parser bench | ✓ 18+ tests in `metric_sampling.rs`, `log_trace_correlation.rs` | Untested | No PLC-side integration: tests use synthetic binary buffers, no tc31-xar-base validation |
| **PLC CPU/memory metrics** | 740333e, d4e3947 | `tc-otel-service/src/system_metrics.rs` (242 LOC) | Minimal — no dedicated unit tests | ✓ 22 tests in `plc_system_metrics.rs` | Untested | Metrics collected from cycle_tracker only; no actual CPU/memory sampling from PLC or OS; relies on task cycle data as proxy |
| **ADS connection health metrics** | dc974ec, affc521 | `tc-otel-ads/src/health_metrics.rs` | None visible | ✓ 15 tests in `ads_connection_health.rs` covering uptime, error counts, latency | Untested | No actual network connectivity testing; mocked in tests; latency calculation untested against real network delays |
| **Custom metric definitions** | 41342b5, d60728f | `tc-otel-core/src/metric_mapper.rs` (config-driven mapping) | None visible | ✓ 27 tests in `custom_metric_mapping.rs` | Untested | Config schema validation minimal; no validation for cyclic/invalid symbol paths; symbol resolution assumed to succeed |
| **Task cycle time metrics** | 8b1b9a6, 9877a05 | `tc-otel-service/src/cycle_time.rs` (CycleTimeTracker), system_metrics.rs | None visible | Implicit in plc_system_metrics.rs | Untested | Jitter calculation (stddev) not validated against known distributions; per-task attribution fragile if task IDs change |
| **OTLP traces export** | 547ebcd, ee5aa2d | `tc-otel-export/src/exporter.rs` (build_otel_traces_payload), `tc-otel-core/src/models.rs` (TraceRecord) | ✓ 20+ payload structure tests | ✓ Implicit in span tests | Untested | No backend integration (Jaeger/Tempo/Zipkin); OTLP/HTTP endpoint routing untested; span status code mapping incomplete |
| **Log-trace correlation** | a48834f, 86ee188 | `tc-otel-ads/src/parser.rs` (type 0x06 traced log), `tc-otel-core/src/models.rs` (trace_id/span_id fields), `tc-otel-export/src/exporter.rs` (correlation injection) | Partial in parser | ✓ 35+ tests in `log_trace_correlation.rs` | Untested | Backward compat verified (v2 logs still parse); trace context injection path tested but no e2e span-log ordering validation |
| **Prometheus/OTEL Collector/Datadog exporters** | 6cda780, 81d92ac | `tc-otel-service/src/dispatcher.rs` (330+ LOC export dispatcher) | None visible | ✓ 12 tests in `metric_export.rs` covering all three exporters | Untested | All tests mock HTTP, no real endpoint connectivity; Prometheus scrape config untested; Datadog API key/endpoint untested |
| **Runtime test skeleton (docker-compose)** | 0a6aa84 | `tests/runtime/docker-compose.yml`, `scripts/run-runtime-tests.sh`, `tests/runtime/otel-collector-config.yml` | N/A (infrastructure) | N/A (skeleton only) | Untested | Infrastructure defined but no actual test cases in `rt-2` (referenced as TODO); swappable profiles work syntactically but untested against real images |
| **ADS symbol discovery & REST API** | 94fd61c | `tc-otel-service/src/symbol_discovery.rs`, `/symbols` and `/symbols/{name}` HTTP endpoints | None visible | ✓ 18 tests in `symbol_discovery.rs` | Untested | Tests use fake PLC symbol registry; no real ADS symbol upload tested; REST endpoint routing untested against HTTP clients |
| **Config hot-reload** | ba90852, fc74970 | `tc-otel-core/src/config_watcher.rs` (fsnotify integration) | None visible | Implicit in integration tests | Untested | File change detection not validated against OS timing edge cases; no test for reload during active connections |
| **Web UI for status/diagnostics** | 26597b2, f6f0ee9, 8dd5fd5 | `tc-otel-service/src/web/` (assumed; files not examined) | None visible | None visible (no UI tests in suite) | Untested | No integration tests for UI; no e2e testing with actual browser or HTTP client; UI code not reviewed |
| **Motion sequence tracing** | 74aaa4a, 46e1825 | `tc-otel-ads/src/parser.rs` (span type MOTION), `tc-otel-export/src/exporter.rs` (span serialization) | None visible | ✓ 21 tests in `span_motion_tracing.rs` | Untested | Tests use synthetic span data; no real AMS frame fixtures with motion metadata; no validation of axis ID mapping or coordinate accuracy |
| **State machine transition tracing** | 7b0c47c, bd845f4 | `tc-otel-ads/src/protocol.rs` (STATE_MACHINE span kind), parser.rs (state transition parsing) | None visible | ✓ 19 tests in `span_state_machine_tracing.rs` | Untested | No real PLC state machine input tested; transition latency not validated; no corpus of multi-state sequences |
| **Recipe execution tracing** | 47a30d3, 2ef1765 | Same span infrastructure as state machine | None visible | ✓ 18 tests in `span_recipe_tracing.rs` | Untested | Synthetic test data only; no real recipe execution workflow validated; no sequential recipe chains tested |
| **Distributed tracing (multi-PLC)** | e7a799a | Span correlation across net_ids in exporter | None visible | ✓ 24 tests in `span_distributed_tracing.rs` | Untested | Tests assume static net_id assignment; no failover or dynamic PLC list scenarios; no real multi-machine setup |
| **TLS/security config validation** | 6eac74b | `tc-otel-core/src/config.rs` (TLS cert path validation) | Partial | ✓ 16 tests in `security_receiver_tls.rs` | Untested | Cert file existence verified in tests; actual TLS handshake not tested; CA validation path untested |
| **Security message limits** | e437e4b | `tc-otel-ads/src/parser.rs` (MAX_* constants, boundary checks) | Implicit in parser | ✓ 22 tests in `security_message_limits.rs` | Untested | Boundary tests exist; no fuzzing with adversarial payloads; no DoS scenario validation |
| **Connection limits & management** | be987ad | `tc-otel-ads/src/connection_manager.rs` (ConnectionPermit, LRU eviction) | None visible | ✓ 14 tests in `security_connection_limits.rs` | Untested | Mock time-based eviction; no actual network connection exhaustion tested; no kernel-level socket limit interactions |
| **gRPC receiver for OTLP logs** | 1e9efcb | `tc-otel-export/src/grpc.rs` (tonic-based LogsServiceServer), OTLP proto types | ✓ 33 unit tests in grpc.rs | Implicit (no dedicated integration test visible) | Untested | Tests verify request parsing; no actual gRPC channel tested; no external OTLP client tested against the server |
| **Helm chart for K8s deployment** | 9e3a4cd | `charts/tc-otel/` (assumed; not examined) | N/A (config) | None visible | Untested | Chart syntax not validated; no K8s cluster e2e test; no volume mount or env var injection tested |
| **ADS-over-MQTT transport (foundation)** | f6fe80a | `tc-otel-ads/src/transport.rs` (trait), `tc-otel-ads/src/transport/mqtt.rs` (MqttAmsTransport), `tc-otel-ads/src/transport/tcp.rs` (refactored TcpAmsTransport) | ✓ 1 unit test in mqtt.rs (fixture parsing); all existing tcp tests pass | None yet | Untested | MQTT broker connectivity untested (no real MQTT broker); topic routing verified only with fixtures; no topic prefix edge cases; backward compat confirmed (tcp tests pass) but no mixed-transport scenario |

---

## Analysis Summary

### Test Coverage by Category

**Strong coverage (integration test suites exist):**
- Metric sampling pipeline (23 tests)
- Log-trace correlation (35+ tests)
- Span tracing variants (21–24 tests each for motion/state machine/recipe/distributed)
- Metric export (12 tests covering Prometheus/OTEL/Datadog)
- ADS connection health (15 tests)
- Security: message limits (22 tests), connection limits (14 tests), TLS (16 tests)
- ADS symbol discovery (18 tests)
- Custom metric mapping (27 tests)

**Weak or missing unit test coverage:**
- System metrics collection (no unit tests; relies on integration tests)
- Config hot-reload (no dedicated tests)
- Web UI (0 tests)
- MQTT transport (1 fixture test only)
- Connection manager (mocked in integration tests, no unit isolation)

**No e2e coverage:**
- None of the features are validated against actual tc31-xar-base PLC runtime or external telemetry backends (Jaeger, Prometheus, Datadog, MQTT brokers).

### Critical Observations

1. **Parser robustness**: AdsParser handles v1/v2/v6 log formats and spans/metrics with comprehensive size/count limits (MAX_* constants). However, only tested with synthetic binary buffers—no actual PLC output corpus.

2. **Cycle time metrics**: The "CPU/memory" metrics feature collects task cycle times only—not actual OS-level CPU/memory usage. This is a significant feature-behavior gap: the naming suggests hardware monitoring but delivers task-level observability.

3. **Metric mapping config**: Custom metric definitions allow mapping PLC symbols to OTEL metrics, but the config schema lacks validation for invalid symbol paths or cyclic references. Symbol resolution is assumed to succeed (no error handling visible in tests).

4. **MQTT transport**: Foundation implemented (trait + MQTT impl) with one fixture test proving parsing works. Backward compat confirmed (TCP tests pass). However:
   - No real MQTT broker connectivity test
   - Topic prefix handling untested
   - Failover or broker outage scenarios absent
   - Config integration (`TransportConfig` enum) present but untested in real deployments

5. **Span types (motion/state machine/recipe)**: Well-structured test coverage with 18–24 tests per type, but all tests use synthetic span data generated in Rust. No validation against actual PLC recordings.

6. **Distributed tracing**: Tests assume static net_id assignment. Multi-PLC failover, dynamic PLC discovery, or net_id reuse scenarios are untested.

7. **Exporters (Prometheus/OTEL/Datadog)**: Dispatcher code (330+ LOC) is real, but all tests mock HTTP. No actual backend connectivity validated.

### Feature-Readiness Scorecard

| Category | Status | Risk |
|----------|--------|------|
| Core protocol parsing (logs/metrics/spans) | **Beta** — Parser complete with security limits, integration tests solid, but no real PLC data | Low-Medium |
| Metrics collection | **Alpha** — Named as "CPU/memory" but delivers cycle time only; untested against real hardware | Medium |
| Trace export (Jaeger/Tempo/Zipkin) | **Beta** — Payload generation tested; backend connectivity untested | Medium |
| MQTT transport | **Beta-** — Foundation implemented, fixture test passes, TCP backward compat confirmed; no real broker test | Medium-High |
| Config hot-reload | **Alpha** — Code exists; not exercised in any test | High |
| Web UI | **Unvalidated** — Code assumed present; no tests at all | High |
| Security (message limits, connection mgmt, TLS) | **Beta** — Boundary tests solid; no adversarial/DoS testing | Low-Medium |
| Multi-PLC distributed tracing | **Alpha** — Single PLC scenarios tested; multi-PLC failover untested | High |

---

## Known Gaps & Unverified Assumptions

### Protocol & Data Flow
1. **No real PLC fixture data**: All parser tests use synthetic binary buffers generated in Rust. The actual ADS/AMS wire format from a real tc31-xar-base PLC is never validated against.
2. **Log sequence assumption**: Log-trace correlation tests assume logs arrive with correct trace_id/span_id. No testing of clock skew, out-of-order arrival, or partial trace data.
3. **Span ordering**: Distributed tracing tests don't validate that parent-child spans arrive in correct order or that span timing is plausible.

### Metrics & Collection
4. **CPU/memory naming mismatch**: Feature name "PLC CPU and memory utilization metrics" collects task cycle times, not CPU or memory data. This is a **major gap** between feature intent and implementation.
5. **Cycle time jitter**: Jitter calculation (stddev) is implemented but never validated against synthetic or real workloads with known variance.
6. **Missing OS metrics**: No host (service) CPU, memory, or disk metrics collected, despite naming suggesting system-wide telemetry.

### Configuration & Operations
7. **Config schema gaps**: Custom metric mapping config has no validation for:
   - Invalid or non-existent symbol paths
   - Cyclic symbol references
   - Type mismatches (expecting DINT but symbol is REAL)
8. **Hot-reload untested**: Config change detection code exists but is not exercised in any test scenario. Reload during active connections is unknown.
9. **Web UI unknown scope**: No documentation or tests visible for the web UI feature. Assumes it exists and is functional.

### Transport & Networking
10. **MQTT broker absent**: MQTT transport has 1 unit test (fixture parsing). No real MQTT broker endpoint tested. Topic prefix, wildcard subscriptions, and QoS behavior are untested.
11. **Export endpoint validation**: Prometheus scrape config, OTEL Collector routing, and Datadog endpoint auth are all mocked in tests. No real-world backend connectivity confirmed.
12. **TLS cert validation**: Tests verify cert file existence; actual TLS handshake and certificate chain validation untested.

### Test Infrastructure
13. **Runtime test skeleton incomplete**: docker-compose.yml and OTEL Collector config defined but no actual test cases wired into `rt-2`. Profile-specific images (softbeckhoff vs tc-runtime) never tested with real containers.
14. **No gRPC client testing**: gRPC receiver has 33 unit tests for proto parsing but no integration test with a real gRPC client sending OTLP logs.

### Multi-Instance Scenarios
15. **Static net_id assumption**: Distributed tracing assumes net_ids are known and fixed. Dynamic PLC discovery, failover, or net_id reuse untested.
16. **Single service instance**: All tests assume one tc-otel service instance. Multi-instance coordination, state sharing, or failover is untested.

---

## Top 5 Risk Areas (Severity Order)

### 1. **Metrics Feature Name-Behavior Mismatch** [High Risk]
- **Issue**: Feature named "PLC CPU and memory utilization metrics" but collects only task cycle times.
- **Impact**: User expectations unmet; actual telemetry is task-level performance, not system-level resource usage.
- **Recommendation**: Either rename feature to reflect actual behavior ("PLC task cycle time metrics") or implement actual CPU/memory sampling from PLC runtime or host OS.

### 2. **MQTT Transport Unvalidated Against Real Brokers** [High Risk]
- **Issue**: MQTT foundation implemented but never tested with actual MQTT broker. Topic routing, QoS, and failover untested.
- **Impact**: Feature may fail silently or incorrectly route frames in production; users of MQTT feature have no guarantee of correctness.
- **Recommendation**: Add integration test using docker-compose + MQTT broker (mosquitto or emqx) with real frame send/receive validation.

### 3. **Config Hot-Reload Not Exercised in Tests** [High Risk]
- **Issue**: Code exists but no test scenario validates config change detection or in-flight connection handling during reload.
- **Impact**: Hot-reload may fail silently (stale config) or crash during reload. Unknown failure modes.
- **Recommendation**: Add integration test that modifies config file and validates new settings take effect without service restart.

### 4. **Web UI Feature Unvalidated** [High Risk]
- **Issue**: No tests, no documentation, scope unknown. Assumed to be functional but never confirmed.
- **Impact**: UI may be non-functional, incomplete, or break silently on config changes.
- **Recommendation**: Document UI feature scope, add integration tests (HTTP client or headless browser), validate against actual deployments.

### 5. **No Real PLC Integration Testing** [Medium-High Risk]
- **Issue**: All parser tests use synthetic binary buffers. No validation against tc31-xar-base real PLC output or actual ADS/AMS frames.
- **Impact**: Undiscovered wire-format bugs, timing issues, or protocol edge cases only emerge in field deployment.
- **Recommendation**: Establish baseline test suite with real tc31-xar-base and validate parser against live PLC logs, metrics, and spans.

---

## Recommendations

### Immediate (Block Production Use)
1. **Clarify/fix CPU-memory metrics naming** (commit message mismatch):
   - Option A: Rename feature to "Task Cycle Time Metrics" and document cycle_time as performance indicator.
   - Option B: Implement actual PLC CPU/memory sampling (may require PLC-side library extension).
   - Document trade-offs clearly.

2. **Add MQTT broker integration test**:
   - Use docker-compose + mosquitto service.
   - Validate frame send/receive with real topic subscriptions.
   - Test QoS and wildcard routing.

3. **Validate config hot-reload**:
   - Add test that modifies config.json and confirms new settings propagated.
   - Test behavior during active connections.

### Short-term (1–2 weeks)
4. **Document and test Web UI feature**:
   - List all endpoints and feature scope.
   - Add integration tests using HTTP client.
   - Validate status page and PLC tag browser against mock data.

5. **Establish real PLC baseline**:
   - Record live ADS frames from tc31-xar-base with Log4TC library.
   - Add regression test corpus (binary fixtures) with known expected parses.
   - Validate parser against real frames monthly.

6. **Enhance config validation**:
   - Add schema validation for custom metric mappings (symbol existence, type checks).
   - Reject configs with invalid symbol paths at startup.

### Medium-term (Ongoing)
7. **Add e2e test harness**:
   - Use existing docker-compose skeleton (0a6aa84).
   - Wire runtime test cases that validate logs/metrics/spans end-to-end.
   - Support both softbeckhoff and tc-runtime profiles.

8. **Validate all exporters against real backends**:
   - Deploy test Prometheus, Jaeger, and Datadog instances.
   - Validate metric scrape, span ingest, and log correlation.

9. **Multi-instance coordination testing**:
   - Test multiple tc-otel services with shared config or etcd.
   - Validate failover and net_id reuse scenarios.

### Documentation
10. **Release notes for GasTown-era features**:
   - List tested features (green), partially tested (yellow), untested (red).
   - Include known limitations and recommendations.
   - Communicate risk areas clearly to users.

---

## Conclusion

The GasTown-era feature set represents **substantial progress** in OTEL integration for TwinCAT: core protocol parsing is robust, metric/span/log infrastructure is well-structured, and integration test coverage is solid for synthetic scenarios. However, **critical gaps remain**:

- **Feature-intent mismatch** (CPU/memory metrics vs. cycle time)
- **No real PLC or external backend validation**
- **MQTT transport unproven in production**
- **Config hot-reload untested**
- **Web UI feature scope unknown**

**Recommendation**: These features are suitable for **beta testing in controlled environments** but **not production-ready** without addressing the five high-risk areas above. Prioritize real PLC integration testing and MQTT broker validation to build confidence in field reliability.
