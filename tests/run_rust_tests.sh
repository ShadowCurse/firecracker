#!/bin/bash

set -e

ARCH=$(uname -m)
TAPS=10

# Clean tap devices if present
for i in $(seq 0 ${TAPS});
do
  ip link del tap$i
done

# Create tap devices
for i in $(seq 0 ${TAPS});
do
  ip tuntap add tap$i mode tap
  sleep 0.1
  ip addr add 172.16.0.$i/24 dev tap$i
  sleep 0.1
  ip link set tap$i up
  sleep 0.1
done

cargo build --all --release --target ${ARCH}-unknown-linux-musl
RUST_TEST_THREADS=1 cargo test -p integration_tests -- --nocapture

# Delete tap devices
for i in $(seq 0 ${TAPS});
do
  ip link del tap$i
done
