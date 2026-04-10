# tc-otel-benches - Performance Benchmarking Suite

High-performance benchmarking suite for tc-otel using criterion.rs.

## Overview

This crate provides comprehensive benchmarks for the tc-otel logging system, measuring:
- **Protocol Parsing**: ADS binary protocol deserialization
- **Memory Allocation**: LogEntry creation patterns
- **OTEL Conversion**: LogEntry → LogRecord mapping overhead
- **End-to-End Performance**: Full pipeline throughput and latency

## Quick Start

### Run All Benchmarks
```bash
cargo bench -p tc-otel-benches
```

### Run Specific Benchmark Suite
```bash
cargo bench -p tc-otel-benches --bench otel_conversion
cargo bench -p tc-otel-benches --bench ads_parser
cargo bench -p tc-otel-benches --bench log_entry_creation
cargo bench -p tc-otel-benches --bench end_to_end
```

### Run Specific Test and Show Details
```bash
cargo bench -p tc-otel-benches --bench otel_conversion -- convert_typical --verbose
```

### Generate HTML Report
```bash
cargo bench -p tc-otel-benches
# Results in: target/criterion/report/index.html
```

## Benchmark Suites

### 1. ADS Parser (`benches/ads_parser.rs`)

Measures deserialization performance of the ADS binary protocol.

**Tests**:
- `parse_minimal_message` - Baseline: minimal valid message
- `parse_typical_message` - Realistic message with arguments
- `parse_scaling` - Varying argument/context counts

**Metrics**: 
- Latency per message parse
- Throughput (messages/sec)
- Allocation patterns

**Expected Performance**:
- Minimal: ~2-3 µs/msg
- Typical: ~5-8 µs/msg
- Scaling: <20 µs/msg for up to 20 args

### 2. Log Entry Creation (`benches/log_entry_creation.rs`)

Measures memory allocation overhead during entry construction.

**Tests**:
- `create_simple_log_entry` - No arguments or context
- `create_typical_log_entry` - 3 arguments, 3 context properties
- `create_complex_log_entry` - 10 arguments, 14 context properties
- `create_variable_complexity` - Scaling from 1 to 20 args/context

**Metrics**:
- Allocation count
- Memory usage
- Timestamp generation overhead

**Expected Performance**:
- Simple: ~2-3 µs/entry
- Typical: ~5-8 µs/entry
- Complex: ~15-20 µs/entry

### 3. OTEL Conversion (`benches/otel_conversion.rs`)

Measures LogEntry → LogRecord conversion overhead (most critical for optimization).

**Tests**:
- `convert_simple_to_otel` - Minimal conversion
- `convert_typical_to_otel` - Realistic conversion with context
- `convert_complex_to_otel` - Complex conversion with many properties
- `convert_scaling` - Scaling with argument/context counts

**Metrics**:
- Conversion latency
- HashMap allocation/cloning overhead
- Attribute building cost

**Expected Performance** (After optimizations):
- Simple: ~3-5 µs/conversion
- Typical: ~8-12 µs/conversion
- Complex: ~20-30 µs/conversion

### 4. End-to-End Pipeline (`benches/end_to_end.rs`)

Measures full pipeline performance: parse → convert → ready for export.

**Tests**:
- `e2e_parse_and_convert_minimal/typical` - Single message latency
- `e2e_throughput_simple/typical/complex_*` - Batch throughput (1000, 100, 10 messages)
- `e2e_batch_processing` - Scaling with batch sizes (10, 50, 100, 500)

**Metrics**:
- End-to-end latency
- Messages/sec throughput
- Batch efficiency (amortized cost per message)

**Expected Performance**:
- Single message: 13-20 µs
- Simple throughput: 50k+ msgs/sec
- Typical throughput: 40k+ msgs/sec
- Complex throughput: 20k+ msgs/sec

## Test Fixtures

### LogEntryFixtures

Pre-built test data for consistent benchmarking:

```rust
use tc_otel_benches::LogEntryFixtures;

// Simple message: 0 arguments, 0 context properties
let simple = LogEntryFixtures::simple_message();

// Typical message: 3 arguments, 3 context properties
let typical = LogEntryFixtures::typical_message();

// Complex message: 10 arguments, 14 context properties
let complex = LogEntryFixtures::complex_message();

// Custom scaling: N arguments, M context properties
let custom = LogEntryFixtures::with_counts(5, 10);
```

### AdsFixtures

Binary protocol test data:

```rust
use tc_otel_benches::AdsFixtures;

// Minimal valid ADS message
let minimal = AdsFixtures::minimal_ads_message();

// Typical message with arguments
let typical = AdsFixtures::typical_ads_message();
```

## Interpreting Results

### Criterion Output Format

```
parse_minimal_message           time:   [2.34 us 2.56 us 2.79 us]
                                change: [-15.23% -8.45% -2.13%] (within noise)
                                slope   [N/A N/A N/A] (within noise)
```

- **Time**: Measured latency (with confidence intervals)
- **Change**: Comparison to baseline (if using --baseline flag)
- **Slope**: Trend over time (if benchmark is unstable)

### Performance Targets

| Metric | Target | Status |
|--------|--------|--------|
| Throughput | 10k+ msgs/sec | ✓ |
| Latency (p99) | <2ms | ✓ |
| Memory baseline | <30MB | ✓ |

## Comparing Performance

### Against Baseline

```bash
# First run: establish baseline
cargo bench -p tc-otel-benches --bench otel_conversion -- --save-baseline v0

# Later: compare against baseline
cargo bench -p tc-otel-benches --bench otel_conversion -- --baseline v0
```

### Manual Comparison

```bash
# Before optimization
cargo bench -p tc-otel-benches > before.txt

# After optimization
cargo bench -p tc-otel-benches > after.txt

# Compare
diff before.txt after.txt
```

## Profiling Performance

### Generate Flamegraph

```bash
# Install flamegraph if not present
cargo install flamegraph

# Profile a specific benchmark
cargo flamegraph --bench otel_conversion -o /tmp/flamegraph.svg

# View in browser
firefox /tmp/flamegraph.svg
```

### Memory Profiling (Linux)

```bash
# Using Valgrind Massif
valgrind --tool=massif --massif-out-file=massif.out \
  cargo bench -p tc-otel-benches --bench otel_conversion

# View results
ms_print massif.out | head -50
```

## Advanced Options

### Custom Sample Size

```bash
# Increase samples for more statistical accuracy
cargo bench -p tc-otel-benches -- --sample-size 500

# Decrease for faster (less accurate) results
cargo bench -p tc-otel-benches -- --sample-size 50
```

### Benchmark-Specific Options

```bash
# Only run benchmarks matching pattern
cargo bench -p tc-otel-benches -- "parse"

# Run with debug output
cargo bench -p tc-otel-benches -- --verbose
```

### Warm-up Configuration

```bash
# Increase CPU warm-up time
cargo bench -p tc-otel-benches -- --warm-up-time 5

# Measure time per iteration
cargo bench -p tc-otel-benches -- --measurement-time 10
```

## Integration with CI/CD

### GitHub Actions Example

```yaml
- name: Run benchmarks
  run: cargo bench -p tc-otel-benches --bench otel_conversion

- name: Compare to baseline
  run: |
    cargo bench -p tc-otel-benches --bench otel_conversion -- --baseline main || true
    
- name: Upload results
  uses: actions/upload-artifact@v3
  with:
    name: benchmark-results
    path: target/criterion/
```

## Performance Tuning Tips

### For Accurate Results

1. **Warm up CPU**: Run on quiet machine, close other applications
2. **Use release mode**: `cargo bench` automatically uses release build
3. **Increase sample size**: Use `--sample-size 1000` for more accurate p99
4. **Run multiple times**: Results may vary slightly between runs
5. **Same environment**: Always run on same hardware for comparisons

### For Faster Iteration

1. **Reduce sample size**: Use `--sample-size 10` for quick feedback
2. **Run single benchmark**: Target specific test to debug
3. **Use --measurement-time 1**: Reduce measurement duration

## Debugging Failed Benchmarks

### Benchmark Panics

If a benchmark panics:
```bash
# Run with backtrace
RUST_BACKTRACE=1 cargo bench -p tc-otel-benches --bench ads_parser
```

### Unexpected Results

1. Check if system is under load: `top`, `htop`
2. Verify input data is correct (check fixture building)
3. Profile with flamegraph to find bottleneck
4. Check for memory issues with Valgrind

## Performance Optimization Workflow

1. **Establish baseline**
   ```bash
   cargo bench -p tc-otel-benches -- --save-baseline v0
   ```

2. **Make code change**
   - Edit source files in main crates

3. **Run benchmarks**
   ```bash
   cargo bench -p tc-otel-benches -- --baseline v0
   ```

4. **Analyze results**
   - Look for improvements (negative change)
   - Check for regressions (positive change >5%)

5. **Profile if needed**
   ```bash
   cargo flamegraph --bench otel_conversion
   ```

6. **Iterate**
   - Make next optimization
   - Re-benchmark

## Known Limitations

1. **Microbenchmarks**: Small allocations/operations may have measurement noise
2. **System dependent**: Results vary by CPU, memory, system load
3. **Not I/O bound**: Benchmarks don't measure network/disk I/O
4. **Single core**: Thread contention effects not captured

## Contributing

When adding new benchmarks:

1. **Use fixtures** for consistent test data
2. **Document expected performance** in test comments
3. **Add scaling tests** to show complexity impact
4. **Include comments** explaining what's being measured
5. **Keep baseline comparable** (don't change fixture data)

## References

- [criterion.rs documentation](https://bheisler.github.io/criterion.rs/book/)
- [Performance Profiling Guide](../../docs/performance-profiling-guide.md)
- [PERFORMANCE.md](../../PERFORMANCE.md)
- [Test Fixtures](src/lib.rs)
