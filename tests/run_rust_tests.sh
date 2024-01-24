#!/bin/bash

set -e

ARCH=$(uname -m)

ip tuntap add tap0 mode tap
sleep 0.1

ip addr add 172.16.0.1/24 dev tap0
sleep 0.1

ip link set tap0 up
sleep 0.1

cargo build --all --release --target ${ARCH}-unknown-linux-musl
RUST_TEST_THREADS=1 cargo test -p integration_tests -- --nocapture

