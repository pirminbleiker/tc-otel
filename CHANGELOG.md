# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- V2 protocol support with simplified TwinCAT API
- Direct Victoria-Logs export with batched async worker
- OpenTelemetry (OTLP) export pipeline with HTTP and gRPC support
- AMS/TCP server implementation (port 48898)
- Multi-platform Docker support (amd64/arm64)
- Debian package (.deb) releases
- GitHub Actions CI/CD (build, benchmark, release, security)
- Comprehensive benchmark suite (ADS parser, OTEL conversion, end-to-end)
- Security tests (connection limits, message limits)

### Changed
- Rewrote service from C#/.NET to Rust for performance and cross-platform support
- Replaced NLog/Graylog/InfluxDB output plugins with unified OTEL export
- Removed Beckhoff licensing requirement -- fully open source
- Zero-alloc hot path optimizations (CPU usage reduced from 116% to 0.05%)

### Fixed
- Timestamps before Unix epoch now accepted (PLC RTC not synced)
- Numeric placeholders `{0}`, `{1}` correctly map to 1-based PLC arguments
- TwinCAT-conform formatting for TIME, LTIME, DATE, DT, TOD types
- Parse all log entries per ADS buffer, not just the first one
- Use PLC timestamp for log ordering instead of receive time
- AMS/TCP Write responses include correct header

### Removed
- Windows-only .NET service (replaced by cross-platform Rust service)
- NLog, Graylog, InfluxDB, SQL output plugins (replaced by OTEL)
- Beckhoff license mechanism
- Azure Pipelines CI/CD (replaced by GitHub Actions)
- DocFx documentation system (replaced by markdown docs)
