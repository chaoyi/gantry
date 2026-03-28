FROM rust:1-alpine AS builder
WORKDIR /build
RUN apk add --no-cache pkgconfig openssl-dev openssl-libs-static
RUN rustup component add rustfmt clippy

# PROFILE: "release" (CI/production) or "dev" (demo/e2e — debug_assert enabled)
ARG PROFILE=release
# CHECK: "true" runs fmt/clippy/test during build (CI), "false" skips (local dev)
ARG CHECK=false

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Build with persistent cargo cache (incremental compilation across runs)
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    if [ "$PROFILE" = "dev" ]; then \
      cargo build -p gantry && cp target/debug/gantry /gantry; \
    else \
      CARGO_PROFILE_RELEASE_LTO=true cargo build --release -p gantry && cp target/release/gantry /gantry; \
    fi

# Optional: run checks (fmt, clippy, tests) — only in CI
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    if [ "$CHECK" = "true" ]; then \
      cargo fmt --check \
      && cargo clippy -- -D warnings \
      && cargo test --lib; \
    fi

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /gantry /gantry
ENTRYPOINT ["/gantry"]
