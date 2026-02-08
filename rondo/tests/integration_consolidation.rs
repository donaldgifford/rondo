//! Integration tests for the tiered consolidation engine (Phase 2).
//!
//! These tests verify that consolidation correctly downsamples data from
//! higher resolution tiers to lower resolution tiers, validates all
//! consolidation functions, handles NaN exclusion, supports cascade
//! across multiple tiers, and that `query_auto` selects appropriate tiers.

use rondo::schema::{ConsolidationFn, LabelMatcher, SchemaConfig, TierConfig};
use rondo::store::Store;
use std::time::Duration;
use tempfile::tempdir;

/// Base timestamp for tests: a round number that aligns to all tier intervals.
const BASE_TIME: u64 = 1_000_000_000_000_000_000; // 1e18 ns

/// Helper: creates a schema with 1s -> 10s(avg) -> 60s(max) tiers.
fn three_tier_schema() -> Vec<SchemaConfig> {
    vec![SchemaConfig {
        name: "test".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(60), // 60 slots
                consolidation_fn: None,
            },
            TierConfig {
                interval: Duration::from_secs(10),
                retention: Duration::from_secs(600), // 60 slots
                consolidation_fn: Some(ConsolidationFn::Average),
            },
            TierConfig {
                interval: Duration::from_secs(60),
                retention: Duration::from_secs(3600), // 60 slots
                consolidation_fn: Some(ConsolidationFn::Max),
            },
        ],
        max_series: 10,
    }]
}

/// Helper: creates a schema with a specific consolidation function for tier 1.
fn schema_with_fn(consolidation_fn: ConsolidationFn) -> Vec<SchemaConfig> {
    vec![SchemaConfig {
        name: "test".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(60),
                consolidation_fn: None,
            },
            TierConfig {
                interval: Duration::from_secs(5),
                retention: Duration::from_secs(300),
                consolidation_fn: Some(consolidation_fn),
            },
        ],
        max_series: 10,
    }]
}

/// Write 15 simulated minutes of 1s data and verify tier 1 (10s) has consolidated values.
#[test]
fn test_15_minutes_consolidation_to_tier1() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("15min_store");

    let mut store = Store::open(&store_path, three_tier_schema()).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write 900 data points (15 minutes at 1s intervals)
    // Use a pattern: value = i * 1.0 so we can verify averages
    for i in 0u32..900 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store.record(handle, f64::from(i), timestamp).unwrap();
    }

    // Run consolidation repeatedly until no more work
    let mut total_ops = 0;
    for _ in 0..20 {
        let ops = store.consolidate().unwrap();
        if ops == 0 {
            break;
        }
        total_ops += ops;
    }

    assert!(total_ops > 0, "Should have performed consolidation operations");

    // Query tier 1 (10s averages)
    let tier1_result = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 900_000_000_000)
        .unwrap();
    let tier1_data: Vec<_> = tier1_result.collect();

    // With 900 seconds of data at 10s intervals, expect up to 90 consolidated points
    assert!(
        !tier1_data.is_empty(),
        "Tier 1 should have consolidated data points"
    );

    // Verify consolidated values are reasonable averages
    // Each 10s window averages 10 consecutive values
    for &(_ts, value) in &tier1_data {
        assert!(
            value.is_finite(),
            "Consolidated values should be finite, got {}",
            value
        );
        assert!(
            (0.0..900.0).contains(&value),
            "Consolidated average should be within source data range, got {}",
            value
        );
    }
}

/// Verify tier cascade: tier 0 -> tier 1 -> tier 2.
///
/// Strategy: write data in batches, consolidating between each batch.
/// This way, tier 0 data is consolidated before it wraps out of retention.
/// Tier 0 has 60s retention, so we write 30s at a time with consolidation in between.
#[test]
fn test_tier_cascade() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("cascade_store");

    let mut store = Store::open(&store_path, three_tier_schema()).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write data in batches of 50s, consolidating between each batch.
    // This ensures data is consolidated before tier 0 wraps (60s retention).
    for batch in 0u32..4 {
        for i in 0u32..50 {
            let offset = batch * 50 + i;
            let timestamp = BASE_TIME + u64::from(offset) * 1_000_000_000;
            store.record(handle, f64::from(offset * 10), timestamp).unwrap();
        }

        // Consolidate after each batch
        for _ in 0..10 {
            let ops = store.consolidate().unwrap();
            if ops == 0 {
                break;
            }
        }
    }

    // Total: 200s of data written, consolidated in batches

    // Tier 1 should have data (10s averages spanning 200s)
    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 210_000_000_000)
        .unwrap()
        .collect();
    assert!(
        !tier1_data.is_empty(),
        "Tier 1 should have consolidated data"
    );

    // Tier 2 should have data (cascaded from tier 1, 60s max)
    // With tier 1 data spanning 200s and 60s windows, should have ~3 points
    let tier2_data: Vec<_> = store
        .query(handle, 2, BASE_TIME, BASE_TIME + 210_000_000_000)
        .unwrap()
        .collect();

    assert!(
        !tier2_data.is_empty(),
        "Tier 2 should have cascaded data from tier 1 (tier1 had {} points)",
        tier1_data.len()
    );
}

/// Verify the Average consolidation function produces correct results.
#[test]
fn test_consolidation_fn_average() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("avg_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Average)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write 5 values in one 5s window: 10, 20, 30, 40, 50 → avg = 30
    for i in 0u32..5 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store
            .record(handle, f64::from((i + 1) * 10), timestamp)
            .unwrap();
    }

    for _ in 0..5 {
        let ops = store.consolidate().unwrap();
        if ops == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    assert!(!tier1_data.is_empty(), "Should have consolidated data");
    // Average of 10, 20, 30, 40, 50 = 30.0
    let consolidated_value = tier1_data[0].1;
    assert!(
        (consolidated_value - 30.0).abs() < 0.01,
        "Average should be 30.0, got {}",
        consolidated_value
    );
}

/// Verify the Min consolidation function.
#[test]
fn test_consolidation_fn_min() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("min_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Min)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write values: 50, 10, 30, 20, 40 → min = 10
    let values = [50.0, 10.0, 30.0, 20.0, 40.0];
    for (i, &v) in values.iter().enumerate() {
        let timestamp = BASE_TIME + (i as u64) * 1_000_000_000;
        store.record(handle, v, timestamp).unwrap();
    }

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    assert!(!tier1_data.is_empty());
    assert!(
        (tier1_data[0].1 - 10.0).abs() < 0.01,
        "Min should be 10.0, got {}",
        tier1_data[0].1
    );
}

/// Verify the Max consolidation function.
#[test]
fn test_consolidation_fn_max() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("max_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Max)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    let values = [10.0, 50.0, 30.0, 20.0, 40.0];
    for (i, &v) in values.iter().enumerate() {
        let timestamp = BASE_TIME + (i as u64) * 1_000_000_000;
        store.record(handle, v, timestamp).unwrap();
    }

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    assert!(!tier1_data.is_empty());
    assert!(
        (tier1_data[0].1 - 50.0).abs() < 0.01,
        "Max should be 50.0, got {}",
        tier1_data[0].1
    );
}

/// Verify the Sum consolidation function.
#[test]
fn test_consolidation_fn_sum() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("sum_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Sum)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // 10 + 20 + 30 + 40 + 50 = 150
    for i in 0u32..5 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store
            .record(handle, f64::from((i + 1) * 10), timestamp)
            .unwrap();
    }

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    assert!(!tier1_data.is_empty());
    assert!(
        (tier1_data[0].1 - 150.0).abs() < 0.01,
        "Sum should be 150.0, got {}",
        tier1_data[0].1
    );
}

/// Verify the Count consolidation function.
#[test]
fn test_consolidation_fn_count() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("count_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Count)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write 5 values → count = 5
    for i in 0u32..5 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store.record(handle, f64::from(i * 100), timestamp).unwrap();
    }

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    assert!(!tier1_data.is_empty());
    assert!(
        (tier1_data[0].1 - 5.0).abs() < 0.01,
        "Count should be 5.0, got {}",
        tier1_data[0].1
    );
}

/// Verify the Last consolidation function.
#[test]
fn test_consolidation_fn_last() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("last_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Last)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write values: 10, 20, 30, 40, 50 → last = 50
    for i in 0u32..5 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store
            .record(handle, f64::from((i + 1) * 10), timestamp)
            .unwrap();
    }

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    assert!(!tier1_data.is_empty());
    assert!(
        (tier1_data[0].1 - 50.0).abs() < 0.01,
        "Last should be 50.0, got {}",
        tier1_data[0].1
    );
}

/// Verify that NaN values in source tier are excluded from consolidation.
#[test]
fn test_nan_excluded_from_consolidation() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("nan_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Average)).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write: 10, NaN (unwritten slot), 30 in a 5s window
    // Only write at timestamps 0, 2 — leaving slot 1 as NaN
    store.record(handle, 10.0, BASE_TIME).unwrap();
    store
        .record(handle, 30.0, BASE_TIME + 2_000_000_000)
        .unwrap();

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let tier1_data: Vec<_> = store
        .query(handle, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    if !tier1_data.is_empty() {
        // Average of 10 and 30 (NaN excluded) = 20.0
        let value = tier1_data[0].1;
        assert!(
            value.is_finite(),
            "NaN should be excluded, got NaN in output"
        );
        assert!(
            (value - 20.0).abs() < 0.01,
            "Average excluding NaN should be 20.0, got {}",
            value
        );
    }
}

/// Verify query_auto selects the highest-resolution tier with data coverage.
#[test]
fn test_query_auto_selects_best_tier() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("auto_tier_store");

    let mut store = Store::open(&store_path, three_tier_schema()).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write 30 data points
    for i in 0u32..30 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store.record(handle, f64::from(i), timestamp).unwrap();
    }

    // Consolidate
    for _ in 0..10 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    // Query within the data range (fully covered by tier 0)
    // Tier 0 has data from BASE_TIME to BASE_TIME + 29e9
    let result = store
        .query_auto(handle, BASE_TIME, BASE_TIME + 29_000_000_000)
        .unwrap();
    assert_eq!(
        result.tier_used(),
        0,
        "Fully covered range should use tier 0 (highest resolution)"
    );

    let data: Vec<_> = result.collect();
    assert_eq!(data.len(), 29, "Should get 29 data points from tier 0 (end exclusive)");
}

/// Verify consolidation is idempotent — repeated calls with no new data do nothing.
#[test]
fn test_consolidation_idempotent() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("idempotent_store");

    let mut store = Store::open(&store_path, three_tier_schema()).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    // Write some data
    for i in 0u32..20 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store.record(handle, f64::from(i), timestamp).unwrap();
    }

    // First consolidation pass
    let _ops1 = store.consolidate().unwrap();

    // Second pass — no new data
    let ops2 = store.consolidate().unwrap();
    assert_eq!(ops2, 0, "Second consolidation with no new data should be a no-op");

    // Third pass — still no-op
    let ops3 = store.consolidate().unwrap();
    assert_eq!(ops3, 0, "Third consolidation should also be a no-op");
}

/// Verify consolidation works correctly with multiple series.
#[test]
fn test_consolidation_multiple_series() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("multi_series_store");

    let mut store = Store::open(&store_path, schema_with_fn(ConsolidationFn::Sum)).unwrap();
    let h1 = store.register("metric1", &[]).unwrap();
    let h2 = store.register("metric2", &[]).unwrap();

    // Write different values to each series
    for i in 0u32..5 {
        let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
        store.record(h1, 10.0, timestamp).unwrap(); // Sum = 50
        store.record(h2, 20.0, timestamp).unwrap(); // Sum = 100
    }

    for _ in 0..5 {
        if store.consolidate().unwrap() == 0 {
            break;
        }
    }

    let s1_data: Vec<_> = store
        .query(h1, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();
    let s2_data: Vec<_> = store
        .query(h2, 1, BASE_TIME, BASE_TIME + 10_000_000_000)
        .unwrap()
        .collect();

    // Both series should have consolidated data
    assert!(!s1_data.is_empty(), "Series 1 should have consolidated data");
    assert!(!s2_data.is_empty(), "Series 2 should have consolidated data");

    // Verify they have different consolidated values
    assert!(
        (s1_data[0].1 - 50.0).abs() < 0.01,
        "Series 1 sum should be 50.0, got {}",
        s1_data[0].1
    );
    assert!(
        (s2_data[0].1 - 100.0).abs() < 0.01,
        "Series 2 sum should be 100.0, got {}",
        s2_data[0].1
    );
}

/// Verify consolidation cursor persistence across store reopens.
#[test]
fn test_consolidation_persists_across_restarts() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("restart_store");
    let schemas = three_tier_schema();

    // First session: write data and consolidate
    {
        let mut store = Store::open(&store_path, schemas.clone()).unwrap();
        let handle = store.register("metric", &[]).unwrap();

        for i in 0u32..20 {
            let timestamp = BASE_TIME + u64::from(i) * 1_000_000_000;
            store.record(handle, f64::from(i), timestamp).unwrap();
        }

        let ops = store.consolidate().unwrap();
        assert!(ops > 0, "First session should consolidate data");
    }

    // Second session: reopen and verify no reprocessing
    {
        let mut store = Store::open(&store_path, schemas).unwrap();
        let _handle = store.register("metric", &[]).unwrap();

        // Without new data, consolidation should be a no-op
        let ops = store.consolidate().unwrap();
        assert_eq!(
            ops, 0,
            "After restart with no new data, consolidation should be a no-op"
        );
    }
}
