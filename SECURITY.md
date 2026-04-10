# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in tc-otel, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please email [pirmin.bleiker@outlook.com](mailto:pirmin.bleiker@outlook.com) with:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge your report within 48 hours and aim to release a fix within 7 days for critical issues.

## Scope

This policy applies to:
- The tc-otel Rust service (`crates/`)
- The TwinCAT PLC library (`source/`, `library/`)
- Official Docker images (`ghcr.io/pirminbleiker/tc-otel`)
- The CI/CD pipeline and release process

## Security Measures

- **Dependency auditing**: Automated daily via `cargo audit` and `cargo deny`
- **Static analysis**: Clippy with strict warnings on every PR
- **Connection limits**: Configurable max connections and message sizes
- **No secrets in config**: Configuration files contain no credentials by design
