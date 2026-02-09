//! Integration tests for the query functionality.

use rondo::error::QueryError;
use rondo::schema::{ConsolidationFn, LabelMatcher, SchemaConfig, TierConfig};
use rondo::store::Store;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn test_query_integration() {
    // Setup store with multi-tier schema
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("query_integration");

    let schemas = vec![SchemaConfig {
        name: "metrics".to_string(),
        label_matcher: LabelMatcher::new([("type", "system")]),
        tiers: vec![
            TierConfig {
                interval: Duration::from_secs(1),   // 1s resolution
                retention: Duration::from_secs(60), // 1 minute retention
                consolidation_fn: None,
            },
            TierConfig {
                interval: Duration::from_secs(60),    // 1m resolution
                retention: Duration::from_secs(3600), // 1 hour retention
                consolidation_fn: Some(ConsolidationFn::Average),
            },
        ],
        max_series: 1000,
    }];

    let mut store = Store::open(&store_path, schemas).unwrap();

    // Register series
    let cpu_handle = store
        .register("cpu.usage", &[("type".to_string(), "system".to_string())])
        .unwrap();
    let mem_handle = store
        .register("mem.usage", &[("type".to_string(), "system".to_string())])
        .unwrap();

    // Write some test data
    let base_time = 1_640_000_000_000_000_000u64;
    for i in 0u32..10 {
        let timestamp = base_time + u64::from(i) * 1_000_000_000; // 1 second intervals
        store
            .record(cpu_handle, f64::from(i * 10), timestamp)
            .unwrap();
        store
            .record(mem_handle, f64::from(i * 5), timestamp)
            .unwrap();
    }

    // Test direct tier query
    let result = store
        .query(cpu_handle, 0, base_time, base_time + 10_000_000_000)
        .unwrap();

    assert_eq!(result.tier_used(), 0);
    assert!(!result.may_be_incomplete());

    let cpu_data: Vec<_> = result.collect_all();
    assert_eq!(cpu_data.len(), 10);

    // Verify data values
    for (i, (timestamp, value)) in cpu_data.iter().enumerate() {
        let expected_time = base_time + i as u64 * 1_000_000_000;
        #[allow(clippy::cast_possible_truncation)]
        let expected_value = f64::from(i as u32 * 10);
        assert_eq!(*timestamp, expected_time);
        assert_eq!(*value, expected_value);
    }

    // Test auto-tier selection
    let result = store
        .query_auto(
            mem_handle,
            base_time + 2_000_000_000,
            base_time + 8_000_000_000,
        )
        .unwrap();

    assert_eq!(result.tier_used(), 0); // Should use high-res tier
    let mem_data: Vec<_> = result.collect_all();
    assert_eq!(mem_data.len(), 6); // Should get 6 data points

    // Test query metadata
    let result = store
        .query(
            cpu_handle,
            0,
            base_time - 5_000_000_000,
            base_time + 15_000_000_000,
        )
        .unwrap();

    assert_eq!(result.tier_used(), 0);
    assert_eq!(
        result.requested_range(),
        (base_time - 5_000_000_000, base_time + 15_000_000_000)
    );

    let (oldest, newest) = result.available_range();
    assert_eq!(oldest, Some(base_time));
    assert_eq!(newest, Some(base_time + 9_000_000_000));

    // Should be incomplete because we requested data from before the first timestamp
    assert!(result.may_be_incomplete());
}

#[test]
fn test_query_error_handling() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("query_errors");

    let schemas = vec![SchemaConfig {
        name: "test".to_string(),
        label_matcher: LabelMatcher::new([("env", "test")]),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(3600),
            consolidation_fn: None,
        }],
        max_series: 100,
    }];

    let mut store = Store::open(&store_path, schemas).unwrap();
    let handle = store
        .register("test.metric", &[("env".to_string(), "test".to_string())])
        .unwrap();

    // Test invalid tier
    let result = store.query(handle, 5, 1000, 2000);
    assert!(result.is_err());
    match result.unwrap_err() {
        rondo::error::RondoError::Query(QueryError::InvalidTier { tier, max_tiers }) => {
            assert_eq!(tier, 5);
            assert_eq!(max_tiers, 1);
        }
        other => panic!("Expected InvalidTier error, got: {:?}", other),
    }

    // Test invalid time range
    let result = store.query(handle, 0, 2000, 1000);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        rondo::error::RondoError::Query(QueryError::InvalidTimeRange { .. })
    ));

    // Same for query_auto
    let result = store.query_auto(handle, 2000, 1000);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        rondo::error::RondoError::Query(QueryError::InvalidTimeRange { .. })
    ));
}

#[test]
fn test_tier_selection_logic() {
    let temp_dir = tempdir().unwrap();
    let store_path = temp_dir.path().join("tier_selection");

    // Create schemas with different retention windows for testing tier selection
    let schemas = vec![SchemaConfig {
        name: "tiered".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(10), // Very short retention
                consolidation_fn: None,
            },
            TierConfig {
                interval: Duration::from_secs(5),
                retention: Duration::from_secs(100), // Longer retention
                consolidation_fn: Some(ConsolidationFn::Average),
            },
        ],
        max_series: 100,
    }];

    let mut store = Store::open(&store_path, schemas).unwrap();
    let handle = store.register("test.metric", &[]).unwrap();

    // Write data that will be in both tiers (simulated)
    let base_time = 1_640_000_000_000_000_000u64;
    store.record(handle, 42.0, base_time).unwrap();

    // Query recent data - should prefer tier 0
    let result = store
        .query_auto(handle, base_time, base_time + 1_000_000_000)
        .unwrap();
    assert_eq!(result.tier_used(), 0);

    // For empty store, should still return a valid result
    let empty_handle = store.register("empty.metric", &[]).unwrap();
    let result = store
        .query_auto(empty_handle, base_time, base_time + 1_000_000_000)
        .unwrap();
    assert_eq!(result.tier_used(), 0); // Default to tier 0
    assert_eq!(result.count(), 0);

    // For a query that requests data from before any available data,
    // it should be marked as potentially incomplete
    let old_time = base_time - 3600 * 1_000_000_000; // 1 hour ago
    let result = store
        .query_auto(handle, old_time, base_time + 1_000_000_000)
        .unwrap();
    assert!(result.may_be_incomplete()); // Should be incomplete since we ask for old data
}
