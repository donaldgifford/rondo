//! Integration tests for the full store lifecycle.
//!
//! These tests exercise the complete flow from store creation through
//! data ingestion and querying, including edge cases specified in the
//! implementation plan (Task 1.8).

use rondo::schema::{ConsolidationFn, LabelMatcher, SchemaConfig, TierConfig};
use rondo::store::Store;
use std::time::Duration;
use tempfile::tempdir;

/// Helper to create a standard multi-tier schema for tests.
fn vmm_schema() -> Vec<SchemaConfig> {
    vec![SchemaConfig {
        name: "vmm".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(600), // 10 min
                consolidation_fn: None,
            },
            TierConfig {
                interval: Duration::from_secs(10),
                retention: Duration::from_secs(21600), // 6 hours
                consolidation_fn: Some(ConsolidationFn::Average),
            },
            TierConfig {
                interval: Duration::from_secs(300),
                retention: Duration::from_secs(604800), // 7 days
                consolidation_fn: Some(ConsolidationFn::Average),
            },
        ],
        max_series: 100,
    }]
}

#[test]
fn test_full_store_lifecycle() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("lifecycle_test");

    let base_time = 1_700_000_000_000_000_000u64;
    let one_sec = 1_000_000_000u64;

    // Phase 1: Create store and register series
    let (cpu_handle, mem_handle) = {
        let mut store = Store::open(&store_path, vmm_schema()).unwrap();

        let cpu = store
            .register("vcpu_usage", &[("vcpu".to_string(), "0".to_string())])
            .unwrap();

        let mem = store
            .register(
                "mem_rss_bytes",
                &[("instance".to_string(), "vm-1".to_string())],
            )
            .unwrap();

        // Write 60 seconds of data
        for i in 0u32..60 {
            let ts = base_time + u64::from(i) * one_sec;
            store.record(cpu, f64::from(i % 100), ts).unwrap();
            store.record(mem, f64::from(1024 + i * 10), ts).unwrap();
        }

        (cpu, mem)
    };

    // Phase 2: Reopen store and verify data persisted
    {
        let store = Store::open(&store_path, vmm_schema()).unwrap();

        // Query all CPU data
        let result = store
            .query(cpu_handle, 0, base_time, base_time + 60 * one_sec)
            .unwrap();
        let data: Vec<_> = result.collect_all();
        assert_eq!(data.len(), 60, "expected 60 data points for 60 seconds");

        // Verify first and last values
        assert_eq!(data[0], (base_time, 0.0));
        assert_eq!(data[59], (base_time + 59 * one_sec, 59.0));

        // Query memory data in a sub-range
        let result = store
            .query(
                mem_handle,
                0,
                base_time + 10 * one_sec,
                base_time + 20 * one_sec,
            )
            .unwrap();
        let mem_data: Vec<_> = result.collect_all();
        assert_eq!(mem_data.len(), 10);
        assert_eq!(mem_data[0].1, f64::from(1024 + 10 * 10));
    }
}

#[test]
fn test_ring_buffer_wraparound_end_to_end() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("wraparound_test");

    // Small retention: 10 seconds at 1s interval = 10 slots
    let schemas = vec![SchemaConfig {
        name: "small".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(10),
            consolidation_fn: None,
        }],
        max_series: 10,
    }];

    let mut store = Store::open(&store_path, schemas).unwrap();
    let handle = store.register("metric", &[]).unwrap();

    let base_time = 1_700_000_000_000_000_000u64;
    let one_sec = 1_000_000_000u64;

    // Write 25 seconds — should wrap the 10-slot buffer twice
    for i in 0u32..25 {
        store
            .record(handle, f64::from(i), base_time + u64::from(i) * one_sec)
            .unwrap();
    }

    // Query all — should only get the last ~10 data points (the ring buffer capacity)
    let result = store.query(handle, 0, 0, u64::MAX).unwrap();
    let data: Vec<_> = result.collect_all();

    // Data should be from the most recent writes, not the oldest
    assert!(!data.is_empty());
    assert!(data.len() <= 10, "should not exceed ring buffer capacity");

    // The newest point should be the last write
    let last = data.last().unwrap();
    assert_eq!(last.0, base_time + 24 * one_sec);
    assert_eq!(last.1, 24.0);
}

#[test]
fn test_nan_handling_in_queries() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("nan_test");

    let schemas = vec![SchemaConfig {
        name: "nan".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(100),
            consolidation_fn: None,
        }],
        max_series: 10,
    }];

    let mut store = Store::open(&store_path, schemas).unwrap();

    let series_a = store.register("a", &[]).unwrap();
    let series_b = store.register("b", &[]).unwrap();

    let base_time = 1_700_000_000_000_000_000u64;
    let one_sec = 1_000_000_000u64;

    // Write to series_a at seconds 1, 3, 5 (gaps at 2, 4)
    store.record(series_a, 10.0, base_time + one_sec).unwrap();
    store
        .record(series_a, 30.0, base_time + 3 * one_sec)
        .unwrap();
    store
        .record(series_a, 50.0, base_time + 5 * one_sec)
        .unwrap();

    // Write to series_b at seconds 2, 4 (different gaps)
    store
        .record(series_b, 20.0, base_time + 2 * one_sec)
        .unwrap();
    store
        .record(series_b, 40.0, base_time + 4 * one_sec)
        .unwrap();

    // Query series_a — should only get 3 points (NaN slots from series_b timestamps skipped)
    let result = store.query(series_a, 0, 0, u64::MAX).unwrap();
    let data_a: Vec<_> = result.collect_all();
    assert_eq!(data_a.len(), 3, "series_a should have 3 data points");
    // Verify all expected values are present
    let values_a: Vec<f64> = data_a.iter().map(|(_, v)| *v).collect();
    assert!(values_a.contains(&10.0));
    assert!(values_a.contains(&30.0));
    assert!(values_a.contains(&50.0));

    // Query series_b — should only get 2 points
    let result = store.query(series_b, 0, 0, u64::MAX).unwrap();
    let data_b: Vec<_> = result.collect_all();
    assert_eq!(data_b.len(), 2, "series_b should have 2 data points");
    let values_b: Vec<f64> = data_b.iter().map(|(_, v)| *v).collect();
    assert!(values_b.contains(&20.0));
    assert!(values_b.contains(&40.0));
}

#[test]
fn test_multiple_schemas_routing() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("multi_schema_test");

    let schemas = vec![
        SchemaConfig {
            name: "cpu".to_string(),
            label_matcher: LabelMatcher::new([("type", "cpu")]),
            tiers: vec![TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(60),
                consolidation_fn: None,
            }],
            max_series: 50,
        },
        SchemaConfig {
            name: "disk".to_string(),
            label_matcher: LabelMatcher::new([("type", "disk")]),
            tiers: vec![TierConfig {
                interval: Duration::from_secs(5),
                retention: Duration::from_secs(300),
                consolidation_fn: None,
            }],
            max_series: 50,
        },
    ];

    let mut store = Store::open(&store_path, schemas).unwrap();

    let cpu_handle = store
        .register("usage", &[("type".to_string(), "cpu".to_string())])
        .unwrap();
    let disk_handle = store
        .register("iops", &[("type".to_string(), "disk".to_string())])
        .unwrap();

    // Verify they're in different schemas
    assert_ne!(cpu_handle.schema_index, disk_handle.schema_index);

    let base_time = 1_700_000_000_000_000_000u64;

    // Write to both
    store.record(cpu_handle, 85.0, base_time).unwrap();
    store.record(disk_handle, 1200.0, base_time).unwrap();

    // Query each independently
    let cpu_data: Vec<_> = store
        .query(cpu_handle, 0, 0, u64::MAX)
        .unwrap()
        .collect_all();
    let disk_data: Vec<_> = store
        .query(disk_handle, 0, 0, u64::MAX)
        .unwrap()
        .collect_all();

    assert_eq!(cpu_data.len(), 1);
    assert_eq!(cpu_data[0].1, 85.0);
    assert_eq!(disk_data.len(), 1);
    assert_eq!(disk_data[0].1, 1200.0);
}

#[test]
fn test_store_reopen_preserves_series() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("reopen_series_test");

    let schemas = vmm_schema();

    // Create and register
    {
        let mut store = Store::open(&store_path, schemas.clone()).unwrap();
        let handle = store
            .register("cpu", &[("host".to_string(), "a".to_string())])
            .unwrap();
        store
            .record(handle, 42.0, 1_700_000_000_000_000_000)
            .unwrap();
    }

    // Reopen and verify the series still exists with data
    {
        let store = Store::open(&store_path, schemas).unwrap();
        assert_eq!(store.series_count(), 1);

        // Re-register same series — should return same handle
        // (need mutable for register, but data should be there from before)
        let mut store = store;
        let handle = store
            .register("cpu", &[("host".to_string(), "a".to_string())])
            .unwrap();

        let data: Vec<_> = store.query(handle, 0, 0, u64::MAX).unwrap().collect_all();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].1, 42.0);
    }
}
