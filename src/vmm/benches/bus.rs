// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Benchmarking cases:
//   * `Bus.read` hitting a range, cycling over all ranges
//   * `Bus.read` missing all ranges
//   * `Bus.read` from several threads, each to its own range
//   * `Bus.move_range` back and forth

#![allow(clippy::cast_possible_truncation)]

use std::hint::black_box;
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use vmm::vstate::bus::{Bus, BusDevice};

const RANGE_SIZE: u64 = 0x1000;
// Leaves a gap after every range to benchmark misses.
const RANGE_STRIDE: u64 = 0x2000;
const RANGE_COUNTS: [u64; 4] = [4, 16, 64, 256];
const THREAD_COUNTS: [u64; 4] = [1, 2, 4, 8];
const _: () = assert!(RANGE_SIZE < RANGE_STRIDE);

struct BenchDevice;
impl BusDevice for BenchDevice {
    fn read(&mut self, _tag: u8, offset: u64, data: &mut [u8]) {
        data[0] = offset as u8;
    }
}

// The devices must outlive the bus, which only holds weak references.
fn bus_create(range_count: u64) -> (Bus, Vec<Arc<Mutex<BenchDevice>>>) {
    assert!(range_count <= 256);
    let bus = Bus::new(256);
    let mut devices = Vec::new();
    for index in 0..range_count {
        let device = Arc::new(Mutex::new(BenchDevice));
        let base = index * RANGE_STRIDE;
        bus.insert(device.clone(), base, RANGE_SIZE, 0).unwrap();
        devices.push(device);
    }
    (bus, devices)
}

fn bench_read_hit(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("bus_read_hit");
    for range_count in RANGE_COUNTS {
        let (bus, _devices) = bus_create(range_count);
        let mut index = 0;
        group.bench_function(BenchmarkId::from_parameter(range_count), |bencher| {
            bencher.iter(|| {
                let address = index * RANGE_STRIDE + 0x10;
                index = (index + 1) % range_count;
                let mut data = [0u8; 4];
                bus.read(black_box(address), &mut data).unwrap();
                black_box(data);
            })
        });
    }
    group.finish();
}

fn bench_read_miss(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("bus_read_miss");
    for range_count in RANGE_COUNTS {
        let (bus, _devices) = bus_create(range_count);
        let mut index = 0;
        group.bench_function(BenchmarkId::from_parameter(range_count), |bencher| {
            bencher.iter(|| {
                let address = index * RANGE_STRIDE + RANGE_SIZE + 0x10;
                index = (index + 1) % range_count;
                let mut data = [0u8; 4];
                bus.read(black_box(address), &mut data).unwrap_err();
                black_box(data);
            })
        });
    }
    group.finish();
}

// Wall time of `iteration_count` reads done by each of `thread_count` threads in parallel.
fn bench_read_parallel_run(bus: &Bus, thread_count: u64, iteration_count: u64) -> Duration {
    let barrier = Barrier::new(thread_count as usize + 1);
    std::thread::scope(|scope| {
        for thread_index in 0..thread_count {
            let barrier = &barrier;
            scope.spawn(move || {
                let address = thread_index * RANGE_STRIDE + 0x10;
                barrier.wait();
                for _ in 0..iteration_count {
                    let mut data = [0u8; 4];
                    bus.read(black_box(address), &mut data).unwrap();
                    black_box(data);
                }
            });
        }
        barrier.wait();
        Instant::now()
    })
    .elapsed()
}

fn bench_read_parallel(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("bus_read_parallel");
    let (bus, _devices) = bus_create(16);
    for thread_count in THREAD_COUNTS {
        group.bench_function(BenchmarkId::from_parameter(thread_count), |bencher| {
            bencher.iter_custom(|iteration_count| {
                bench_read_parallel_run(&bus, thread_count, iteration_count)
            })
        });
    }
    group.finish();
}

fn bench_move_range(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("bus_move_range");
    for range_count in RANGE_COUNTS {
        let (bus, _devices) = bus_create(range_count);
        let base_top = range_count * RANGE_STRIDE;
        group.bench_function(BenchmarkId::from_parameter(range_count), |bencher| {
            bencher.iter(|| {
                bus.move_range(0, base_top, RANGE_SIZE).unwrap();
                bus.move_range(base_top, 0, RANGE_SIZE).unwrap();
            })
        });
    }
    group.finish();
}

criterion_group! {
    name = bus_benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .noise_threshold(0.05);
    targets = bench_read_hit, bench_read_miss, bench_read_parallel, bench_move_range
}

criterion_main! {
    bus_benches
}
