#!/bin/bash

set -e

ARCH=$(uname -m)
TAPS=10

# Clean tap devices if present
for i in $(seq 0 ${TAPS});
do
  ip link del tap$i || true
done

# Create tap devices
for i in $(seq 0 ${TAPS});
do
  tap=tap$i
  ip=172.16.0.${i+1}/24

  ip tuntap add $tap mode tap
  sleep 0.1
  ip addr add $ip dev tap$i
  sleep 0.1
  ip link set $tap up
  sleep 0.1
done

# cargo build -p firecracker --release --target ${ARCH}-unknown-linux-musl
RUST_TEST_THREADS=1 cargo test -p integration_tests -- --nocapture

# Delete tap devices
for i in $(seq 0 ${TAPS});
do
  ip link del tap$i || true
done
