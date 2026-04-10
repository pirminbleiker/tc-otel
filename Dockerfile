# Multi-platform Dockerfile for tc-otel service
# Supports: linux/amd64, linux/arm64
#
# Build:
#   docker buildx build --platform linux/amd64,linux/arm64 -t tc-otel .
#
# For local single-platform:
#   docker build -t tc-otel .

# Stage 1: Cache dependencies (only rebuilds when Cargo.toml/Cargo.lock change)
FROM --platform=$BUILDPLATFORM rust:latest AS deps
ARG TARGETARCH

# Install cross-compilation tools for arm64
RUN if [ "$TARGETARCH" = "arm64" ]; then \
      apt-get update && apt-get install -y gcc-aarch64-linux-gnu && \
      rustup target add aarch64-unknown-linux-gnu; \
    fi

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/tc-otel-core/Cargo.toml crates/tc-otel-core/Cargo.toml
COPY crates/tc-otel-ads/Cargo.toml crates/tc-otel-ads/Cargo.toml
COPY crates/tc-otel-export/Cargo.toml crates/tc-otel-export/Cargo.toml
COPY crates/tc-otel-service/Cargo.toml crates/tc-otel-service/Cargo.toml
COPY crates/tc-otel-benches/Cargo.toml crates/tc-otel-benches/Cargo.toml
COPY crates/tc-otel-integration-tests/Cargo.toml crates/tc-otel-integration-tests/Cargo.toml

# Create dummy source files to cache dependencies
RUN mkdir -p crates/tc-otel-core/src && echo "pub fn dummy(){}" > crates/tc-otel-core/src/lib.rs \
    && mkdir -p crates/tc-otel-ads/src && echo "pub fn dummy(){}" > crates/tc-otel-ads/src/lib.rs \
    && mkdir -p crates/tc-otel-export/src && echo "pub fn dummy(){}" > crates/tc-otel-export/src/lib.rs \
    && mkdir -p crates/tc-otel-service/src && echo "fn main(){}" > crates/tc-otel-service/src/main.rs \
    && mkdir -p crates/tc-otel-benches/src && echo "pub fn dummy(){}" > crates/tc-otel-benches/src/lib.rs \
    && mkdir -p crates/tc-otel-integration-tests/src && echo "" > crates/tc-otel-integration-tests/src/lib.rs

# Pre-build dependencies
RUN if [ "$TARGETARCH" = "arm64" ]; then \
      CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
      cargo build --release -p tc-otel-service --target aarch64-unknown-linux-gnu 2>/dev/null; exit 0; \
    else \
      cargo build --release -p tc-otel-service 2>/dev/null; exit 0; \
    fi

# Stage 2: Build actual source
FROM deps AS builder
ARG TARGETARCH
COPY crates/ crates/
RUN touch crates/*/src/*.rs

RUN if [ "$TARGETARCH" = "arm64" ]; then \
      CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
      cargo build --release -p tc-otel-service --target aarch64-unknown-linux-gnu && \
      cp target/aarch64-unknown-linux-gnu/release/tc-otel /app/tc-otel; \
    else \
      cargo build --release -p tc-otel-service && \
      cp target/release/tc-otel /app/tc-otel; \
    fi

# Stage 3: Minimal runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/tc-otel /usr/local/bin/tc-otel
COPY config.example.json /etc/tc-otel/config.json

ENV TC_OTEL_CONFIG=/etc/tc-otel/config.json
EXPOSE 48898 16150 4317 4318

ENTRYPOINT ["tc-otel", "--config", "/etc/tc-otel/config.json"]
