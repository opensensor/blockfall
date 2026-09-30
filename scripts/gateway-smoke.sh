#!/usr/bin/env bash
# G4 smoke: build the gateway release binary and run its in-process
# loopback self-test (register/lookup/pin/forward/busy/release).
# Exits with the self-test code (0 = PASS).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p netplay-gateway --release
target/release/netplay-gateway --self-test
