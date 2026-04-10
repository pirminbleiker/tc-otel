# Contributing to tc-otel

We welcome contributions! Whether it's bug reports, feature proposals, documentation improvements, or code changes.

## Getting Started

### Prerequisites

**Rust service:**
- [Rust](https://rustup.rs/) 1.75 or later
- A running TwinCAT PLC (optional, for integration testing)

**PLC library:**
- TwinCAT 3 XAE (Visual Studio 2019+) with TwinCAT 4024.x or later
- Set **Separate LineIDs** to TRUE: Tools > Options > TwinCAT > PLC Environment > Write options

### Clone and build

```bash
git clone https://github.com/pirminbleiker/tc-otel.git
cd tc-otel

# Build the Rust service
cargo build --release -p tc-otel-service

# Run all tests
cargo test --all

# Run clippy lints
cargo clippy --all -- -D warnings

# Check formatting
cargo fmt --all -- --check
```

### Project structure

```
crates/
  tc-otel-core/          Core types, config, formatting
  tc-otel-ads/           ADS/AMS protocol implementation
  tc-otel-export/        OpenTelemetry OTLP export
  tc-otel-service/       Main service binary
  tc-otel-benches/       Performance benchmarks
  tc-otel-integration-tests/  Integration tests
source/
  TwinCat_Lib/          PLC library source (IEC 61131-3)
  TwinCat_Examples/     PLC example projects
library/
  Log4TC.library        Compiled TwinCAT library (legacy name)
tests/                  Additional Rust integration & security tests
```

## Development Workflow

1. Fork the repo and create your branch from `master`
2. Make your changes
3. Run `cargo test --all` and `cargo clippy --all -- -D warnings`
4. For PLC changes, verify with `checkAllObjects` in TwinCAT XAE
5. Open a pull request and link any related issues

## Code Style

**Rust:**
- Follow `rustfmt` defaults (enforced by CI)
- Use `clippy` with warnings as errors
- Prefer `thiserror` for library errors, `anyhow` in the service binary

**TwinCAT / IEC 61131-3:**
- Follow [Beckhoff naming conventions](https://infosys.beckhoff.com/english.php?content=../content/1033/tc3_plc_intro/18014401873267083.html)
- Use Separate LineIDs (see prerequisites)

## Testing

```bash
# Unit tests
cargo test --all

# Integration tests (requires no PLC connection)
cargo test --test '*'

# Benchmarks
cargo bench

# Security tests
cargo test --test 'security_*'
```

## Reporting Bugs

Use [GitHub Issues](https://github.com/pirminbleiker/tc-otel/issues). A good bug report includes:

- Summary and background
- Steps to reproduce (be specific)
- Expected vs. actual behavior
- TwinCAT version, OS, and tc-otel version
- Relevant config and log output

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
