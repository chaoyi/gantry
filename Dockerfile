FROM rust:1-alpine AS builder
WORKDIR /build
RUN apk add --no-cache pkgconfig openssl-dev openssl-libs-static

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY crates/gantry/Cargo.toml crates/gantry/Cargo.toml
COPY crates/gantry-cue/Cargo.toml crates/gantry-cue/Cargo.toml
RUN mkdir -p crates/gantry/src crates/gantry-cue/src crates/gantry/ui \
    && echo "fn main() {}" > crates/gantry/src/main.rs \
    && echo "fn main() {}" > crates/gantry-cue/src/main.rs \
    && touch crates/gantry/src/lib.rs \
    && cargo build --release -p gantry \
    && rm -rf crates/gantry/src

# Build (LTO for size)
COPY crates/gantry/ crates/gantry/
ENV CARGO_PROFILE_RELEASE_LTO=true
RUN touch crates/gantry/src/main.rs && cargo build --release -p gantry

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /build/target/release/gantry /gantry
ENTRYPOINT ["/gantry"]
