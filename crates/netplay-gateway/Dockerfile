# syntax=docker/dockerfile:1
# Blockfall netplay gateway — static musl binary on a scratch base.
#
# Build context is the REPO ROOT (the workspace root manifest is required):
#
#   docker build -f crates/netplay-gateway/Dockerfile -t blockfall-gateway .
#
# Run — host networking is the practical way to expose the whole UDP range
# (mapping 984 ports with -p is slow):
#
#   docker run -d --name blockfall-gateway --net=host \
#     --restart=unless-stopped blockfall-gateway
#
# Config is CLI flags only (no env vars, by design — same as the binary).
# Pass overrides as run arguments after the image name:
#
#   docker run --net=host blockfall-gateway \
#     --listen 0.0.0.0:27016 --data-start 27017 --data-count 983 --idle-secs 15
#
# Firewall: allow inbound UDP 27016-27999 on the host.
# Smoke: `docker run --rm blockfall-gateway --self-test` prints PASS and
# exits 0 (loopback-only, needs no port exposure).

FROM rust:1.95-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Toolchain file first: rustup resolves the pinned toolchain per-directory,
# so `target add` must run where rust-toolchain.toml is visible (the image's
# default toolchain is NOT what cargo will use inside the workspace).
COPY rust-toolchain.toml ./
RUN rustup target add x86_64-unknown-linux-musl
COPY . .
RUN cargo build --release -p netplay-gateway --target x86_64-unknown-linux-musl

# Zero-trust runtime: no libc, no shell, no userdb — one static binary.
# Runs as root (uid 0) inside the container harmlessly: nothing is written
# and DynamicUser-style hardening belongs to the systemd unit instead.
FROM scratch
COPY --from=builder /src/target/x86_64-unknown-linux-musl/release/netplay-gateway /blockfall-gateway
EXPOSE 27016-27999/udp
ENTRYPOINT ["/blockfall-gateway"]
