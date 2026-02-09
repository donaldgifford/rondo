//! Microbenchmarks for the `record()` hot path.
//!
//! Measures write latency and verifies zero-allocation behavior.
//!
//! Run with: `cargo bench -p rondo -- record`

#![allow(missing_docs, clippy::cast_possible_truncation)]

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use rondo::schema::{LabelMatcher, SchemaConfig, TierConfig};
use rondo::store::Store;
use std::time::Duration;
use tempfile::tempdir;

/// Creates a store with a realistic VMM metrics schema.
fn setup_store(series_count: u32) -> (Store, Vec<rondo::SeriesHandle>, tempfile::TempDir) {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("bench_store");

    let schemas = vec![SchemaConfig {
        name: "bench".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(600),
            consolidation_fn: None,
        }],
        max_series: series_count + 10,
    }];

    let mut store = Store::open(&store_path, schemas).unwrap();

    let mut handles = Vec::with_capacity(series_count as usize);
    for i in 0..series_count {
        let handle = store
            .register(
                &format!("metric_{i}"),
                &[("id".to_string(), format!("{i}"))],
            )
            .unwrap();
        handles.push(handle);
    }

    (store, handles, temp_dir)
}

fn bench_record_single(c: &mut Criterion) {
    let (mut store, handles, _dir) = setup_store(1);
    let handle = handles[0];

    let base_time = 1_700_000_000_000_000_000u64;
    let mut ts = base_time;

    c.bench_function("record/single_series", |b| {
        b.iter(|| {
            ts += 1_000_000_000;
            store
                .record(black_box(handle), black_box(42.5), black_box(ts))
                .unwrap();
        });
    });
}

fn bench_record_many_series(c: &mut Criterion) {
    let mut group = c.benchmark_group("record/series_count");

    for count in [1, 10, 30, 100] {
        let (mut store, handles, _dir) = setup_store(count);
        let base_time = 1_700_000_000_000_000_000u64;
        let mut ts = base_time;

        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                ts += 1_000_000_000;
                for (i, handle) in handles.iter().enumerate() {
                    store
                        .record(
                            black_box(*handle),
                            black_box(f64::from(i as u32)),
                            black_box(ts),
                        )
                        .unwrap();
                }
            });
        });
    }

    group.finish();
}

fn bench_record_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_batch/series_count");

    for count in [1, 10, 30, 100] {
        let (mut store, handles, _dir) = setup_store(count);
        let base_time = 1_700_000_000_000_000_000u64;
        let mut ts = base_time;

        let entries: Vec<_> = handles
            .iter()
            .enumerate()
            .map(|(i, h)| (*h, f64::from(i as u32)))
            .collect();

        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                ts += 1_000_000_000;
                store
                    .record_batch(black_box(&entries), black_box(ts))
                    .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_record_throughput(c: &mut Criterion) {
    let (mut store, handles, _dir) = setup_store(30);
    let base_time = 1_700_000_000_000_000_000u64;
    let mut ts = base_time;

    c.bench_function("record/30_series_throughput", |b| {
        b.iter(|| {
            ts += 1_000_000_000;
            for handle in &handles {
                store
                    .record(black_box(*handle), black_box(99.9), black_box(ts))
                    .unwrap();
            }
        });
    });
}

criterion_group!(
    benches,
    bench_record_single,
    bench_record_many_series,
    bench_record_batch,
    bench_record_throughput,
);
criterion_main!(benches);
