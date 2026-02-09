//! Demonstration of the consolidation engine in Rondo.
//!
//! This example shows how consolidation automatically downsamples data from
//! higher resolution tiers to lower resolution tiers using different
//! consolidation functions.

use std::time::Duration;

use rondo::Store;
use rondo::schema::{ConsolidationFn, LabelMatcher, SchemaConfig, TierConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a temporary directory for the store
    let store_path = "./consolidation_demo_store";

    // Define a schema with multiple tiers and different consolidation functions
    let schemas = vec![SchemaConfig {
        name: "metrics".to_string(),
        label_matcher: LabelMatcher::any(), // Match any labels
        tiers: vec![
            // Tier 0: High resolution - 1s intervals, keep for 1 minute
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(60),
                consolidation_fn: None, // No consolidation for highest tier
            },
            // Tier 1: Medium resolution - 10s intervals, keep for 10 minutes
            // Use Average consolidation to smooth out noise
            TierConfig {
                interval: Duration::from_secs(10),
                retention: Duration::from_secs(600),
                consolidation_fn: Some(ConsolidationFn::Average),
            },
            // Tier 2: Low resolution - 60s intervals, keep for 1 hour
            // Use Max consolidation to capture peak values
            TierConfig {
                interval: Duration::from_secs(60),
                retention: Duration::from_secs(3600),
                consolidation_fn: Some(ConsolidationFn::Max),
            },
        ],
        max_series: 100,
    }];

    // Create the store
    let mut store = Store::open(store_path, schemas)?;
    println!("Created store with 3 tiers: 1s->10s(avg)->60s(max)");

    // Register a CPU usage series
    let cpu_handle = store.register(
        "cpu.usage",
        &[
            ("host".to_string(), "server1".to_string()),
            ("metric".to_string(), "cpu".to_string()),
        ],
    )?;

    // Register a memory usage series
    let mem_handle = store.register(
        "memory.usage",
        &[
            ("host".to_string(), "server1".to_string()),
            ("metric".to_string(), "memory".to_string()),
        ],
    )?;

    println!("Registered CPU and memory usage series");

    // Simulate writing metrics over time
    let base_time = 1_640_995_200_000_000_000u64; // 2022-01-01 00:00:00 UTC in ns
    println!("\nWriting sample data to tier 0 (1s resolution):");

    for i in 0u32..65 {
        let timestamp = base_time + u64::from(i) * 1_000_000_000; // 1s intervals

        // CPU usage: simulate varying load with some spikes
        let fi = f64::from(i);
        let cpu_value = if i % 10 == 0 {
            95.0 + (fi % 3.0) // Periodic spikes
        } else {
            45.0 + 15.0 * ((fi * 0.1).sin()) // Sinusoidal base load
        };

        // Memory usage: simulate gradual increase
        let mem_value = 60.0 + (fi * 0.3);

        store.record(cpu_handle, cpu_value, timestamp)?;
        store.record(mem_handle, mem_value, timestamp)?;

        if i % 10 == 0 {
            println!(
                "  t+{}s: CPU={:.1}%, Memory={:.1}%",
                i, cpu_value, mem_value
            );
        }
    }

    println!("\nRunning consolidation engine...");

    // Run consolidation multiple times to cascade through all tiers
    let mut total_operations = 0;
    for round in 1..=5 {
        let operations = store.consolidate()?;
        total_operations += operations;
        if operations == 0 {
            println!("  Round {}: No new consolidation needed", round);
            break;
        }
        println!(
            "  Round {}: Performed {} consolidation operations",
            round, operations
        );
    }

    println!("\nTotal consolidation operations: {}", total_operations);

    // Query data from different tiers to show consolidation results
    let query_start = base_time;
    let query_end = base_time + 70_000_000_000; // 70 seconds

    println!("\nQuerying CPU usage from different tiers:");

    // Tier 0 (1s resolution, raw data)
    let tier0_result = store.query(cpu_handle, 0, query_start, query_end)?;
    println!("  Tier 0 (1s): {} data points", tier0_result.count());

    // Tier 1 (10s resolution, averaged)
    let tier1_result = store.query(cpu_handle, 1, query_start, query_end)?;
    let tier1_data: Vec<_> = tier1_result.collect();
    println!("  Tier 1 (10s avg): {} data points", tier1_data.len());
    if !tier1_data.is_empty() {
        println!(
            "    Sample: t={}, value={:.1}%",
            tier1_data[0].0 - base_time,
            tier1_data[0].1
        );
    }

    // Tier 2 (60s resolution, max values)
    let tier2_result = store.query(cpu_handle, 2, query_start, query_end)?;
    let tier2_data: Vec<_> = tier2_result.collect();
    println!("  Tier 2 (60s max): {} data points", tier2_data.len());
    if !tier2_data.is_empty() {
        println!(
            "    Sample: t={}, value={:.1}%",
            tier2_data[0].0 - base_time,
            tier2_data[0].1
        );
    }

    println!("\nQuerying memory usage from different tiers:");

    // Memory tier 1
    let mem_tier1_result = store.query(mem_handle, 1, query_start, query_end)?;
    let mem_tier1_data: Vec<_> = mem_tier1_result.collect();
    println!("  Tier 1 (10s avg): {} data points", mem_tier1_data.len());

    // Memory tier 2
    let mem_tier2_result = store.query(mem_handle, 2, query_start, query_end)?;
    let mem_tier2_data: Vec<_> = mem_tier2_result.collect();
    println!("  Tier 2 (60s max): {} data points", mem_tier2_data.len());

    println!("\nConsolidation complete! Data is now available at multiple resolutions.");
    println!(
        "Cursor state is persisted in: {}/consolidation_cursors.json",
        store_path
    );

    // Clean up (optional)
    if std::path::Path::new(store_path).exists() {
        std::fs::remove_dir_all(store_path)?;
        println!("\nCleaned up demo store directory");
    }

    Ok(())
}
