//! Benchmark C: Ephemeral VM data capture comparison.
//!
//! Simulates 30-second and 45-second VM lifecycles and compares data
//! points captured by:
//! - Embedded rondo (records every second → 100% capture)
//! - Traditional 15s scrape interval (0-3 data points)
//!
//! Demonstrates that embedded recording captures every data point for
//! short-lived workloads, while scrape-based systems lose most of the data.
//!
//! Run with: `cargo run -p rondo --release --example ephemeral_vm_benchmark`

#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::time::Duration;

use rondo::schema::{LabelMatcher, SchemaConfig, TierConfig};
use rondo::store::Store;

fn main() {
    println!("=== Ephemeral VM Data Capture Comparison ===");
    println!();

    for vm_lifetime_secs in [30, 45, 10, 5] {
        run_comparison(vm_lifetime_secs);
        println!();
    }
}

fn run_comparison(vm_lifetime_secs: u32) {
    println!("--- VM lifetime: {vm_lifetime_secs} seconds ---");

    let embedded_points = simulate_embedded_recording(vm_lifetime_secs);
    let scrape_15s_points = simulate_scrape_interval(vm_lifetime_secs, 15);
    let scrape_30s_points = simulate_scrape_interval(vm_lifetime_secs, 30);

    let capture_rate_15s = if embedded_points > 0 {
        scrape_15s_points as f64 / embedded_points as f64 * 100.0
    } else {
        0.0
    };
    let capture_rate_30s = if embedded_points > 0 {
        scrape_30s_points as f64 / embedded_points as f64 * 100.0
    } else {
        0.0
    };

    println!("  Embedded rondo (1s interval):   {embedded_points:>4} data points (100.0%)");
    println!(
        "  Prometheus scrape (15s interval): {scrape_15s_points:>4} data points ({capture_rate_15s:>5.1}%)"
    );
    println!(
        "  Prometheus scrape (30s interval): {scrape_30s_points:>4} data points ({capture_rate_30s:>5.1}%)"
    );

    // Verify embedded captured every second
    assert_eq!(
        embedded_points, vm_lifetime_secs,
        "Embedded should capture exactly {vm_lifetime_secs} points"
    );
}

/// Simulates embedded rondo recording at 1s intervals for a VM lifecycle.
///
/// Returns the number of data points captured.
fn simulate_embedded_recording(vm_lifetime_secs: u32) -> u32 {
    let temp_dir = std::env::temp_dir().join(format!("rondo_ephemeral_{vm_lifetime_secs}"));
    let _ = std::fs::remove_dir_all(&temp_dir);

    let schemas = vec![SchemaConfig {
        name: "vm".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(600),
            consolidation_fn: None,
        }],
        max_series: 10,
    }];

    let mut store = Store::open(&temp_dir, schemas).unwrap();
    let handle = store
        .register("cpu_usage", &[("vm".to_string(), "ephemeral-1".to_string())])
        .unwrap();

    let base_time = 1_700_000_000_000_000_000u64;

    // Simulate VM lifecycle: record at 1s intervals
    for i in 0..vm_lifetime_secs {
        let ts = base_time + u64::from(i) * 1_000_000_000;
        // Simulate a realistic CPU usage pattern:
        // Startup spike, steady state, shutdown spike
        let value = simulate_cpu_pattern(i, vm_lifetime_secs);
        store.record(handle, value, ts).unwrap();
    }

    // Query all recorded data
    let end_time = base_time + u64::from(vm_lifetime_secs) * 1_000_000_000;
    let result = store.query(handle, 0, base_time, end_time).unwrap();
    let points: Vec<_> = result.collect();

    let _ = std::fs::remove_dir_all(&temp_dir);

    points.len() as u32
}

/// Simulates a traditional scrape-based system collecting data at fixed intervals.
///
/// Returns the number of data points that would be captured.
fn simulate_scrape_interval(vm_lifetime_secs: u32, scrape_interval_secs: u32) -> u32 {
    // The scrape model collects data at fixed intervals.
    // For a VM that lives N seconds, a scrape at interval S captures at most
    // floor(N / S) + 1 points (if the first scrape lands exactly at startup).
    //
    // In practice, scrape timing is not aligned with VM start, so we simulate
    // worst/best/average cases and report the average.

    // Best case: first scrape exactly at VM start
    let best = vm_lifetime_secs / scrape_interval_secs + 1;

    // Worst case: VM starts just after a scrape, ends just before the next
    let worst = if vm_lifetime_secs >= scrape_interval_secs {
        (vm_lifetime_secs - 1) / scrape_interval_secs
    } else {
        0
    };

    // Average case (random phase offset): midpoint of best/worst
    (best + worst) / 2
}

/// Simulates a CPU usage pattern for a VM lifecycle.
fn simulate_cpu_pattern(second: u32, total_seconds: u32) -> f64 {
    let pct = second as f64 / total_seconds as f64;

    if pct < 0.1 {
        // Startup spike: 80-95%
        80.0 + pct * 150.0
    } else if pct > 0.9 {
        // Shutdown spike: 70-85%
        70.0 + (1.0 - pct) * 150.0
    } else {
        // Steady state: 20-50% with some variation
        35.0 + 15.0 * (second as f64 * 0.5).sin()
    }
}
