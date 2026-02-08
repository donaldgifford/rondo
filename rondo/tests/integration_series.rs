//! Integration tests for the series registration module.

use rondo::error::Result;
use rondo::schema::{LabelMatcher, SchemaConfig, TierConfig};
use rondo::series::SeriesRegistry;
use rondo::slab::Slab;
use std::time::Duration;

#[test]
fn test_series_registration_integration() -> Result<()> {
    let temp_dir = tempfile::tempdir().unwrap();

    // Create schema configuration
    let schema = SchemaConfig {
        name: "test_metrics".to_string(),
        label_matcher: LabelMatcher::new([("service", "web")]),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(3600),
            consolidation_fn: None,
        }],
        max_series: 100,
    };

    // Create series registry
    let mut registry = SeriesRegistry::new(vec![schema.clone()]);

    // Register some series
    let labels1 = vec![
        ("service".to_string(), "web".to_string()),
        ("host".to_string(), "server1".to_string()),
    ];
    let labels2 = vec![
        ("service".to_string(), "web".to_string()),
        ("host".to_string(), "server2".to_string()),
    ];

    let handle1 = registry.register("cpu.usage", &labels1)?;
    let handle2 = registry.register("memory.usage", &labels2)?;

    // Verify handles are different but in same schema
    assert_eq!(handle1.schema_index, 0);
    assert_eq!(handle2.schema_index, 0);
    assert_ne!(handle1.series_id, handle2.series_id);
    assert_ne!(handle1.column, handle2.column);

    // Create a slab for the schema
    let slab_path = temp_dir.path().join("test.slab");
    let mut slab = Slab::create(
        &slab_path,
        schema.stable_hash(),
        3600, // 1 hour of 1-second samples
        schema.max_series,
        1_000_000_000, // 1 second interval in nanoseconds
    )?;

    // Sync registry to slab
    registry.sync_to_slabs(&mut [&mut slab])?;

    // Verify slab state
    assert_eq!(slab.series_count(), 2);
    assert_eq!(slab.get_series_column(handle1.series_id), Some(handle1.column));
    assert_eq!(slab.get_series_column(handle2.series_id), Some(handle2.column));

    // Test persistence
    let index_path = temp_dir.path().join("series_index.json");
    registry.save(&index_path)?;

    // Load registry from disk
    let loaded_registry = SeriesRegistry::load(&index_path, vec![schema])?;

    // Verify loaded state
    assert_eq!(loaded_registry.total_series_count(), 2);

    let loaded_handle1 = loaded_registry.get_handle("cpu.usage", &labels1).unwrap();
    let loaded_handle2 = loaded_registry.get_handle("memory.usage", &labels2).unwrap();

    assert_eq!(handle1, loaded_handle1);
    assert_eq!(handle2, loaded_handle2);

    Ok(())
}

#[test]
fn test_multi_schema_registration() -> Result<()> {
    // Create multiple schemas
    let cpu_schema = SchemaConfig {
        name: "cpu_metrics".to_string(),
        label_matcher: LabelMatcher::new([("type", "cpu")]),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(3600),
            consolidation_fn: None,
        }],
        max_series: 50,
    };

    let memory_schema = SchemaConfig {
        name: "memory_metrics".to_string(),
        label_matcher: LabelMatcher::new([("type", "memory")]),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(5),
            retention: Duration::from_secs(7200),
            consolidation_fn: None,
        }],
        max_series: 30,
    };

    let mut registry = SeriesRegistry::new(vec![cpu_schema, memory_schema]);

    // Register series in different schemas
    let cpu_labels = vec![
        ("type".to_string(), "cpu".to_string()),
        ("host".to_string(), "server1".to_string()),
    ];
    let memory_labels = vec![
        ("type".to_string(), "memory".to_string()),
        ("host".to_string(), "server1".to_string()),
    ];

    let cpu_handle = registry.register("cpu.usage", &cpu_labels)?;
    let memory_handle = registry.register("memory.usage", &memory_labels)?;

    // Should be in different schemas
    assert_eq!(cpu_handle.schema_index, 0);
    assert_eq!(memory_handle.schema_index, 1);

    // Both should start from column 0 in their respective schemas
    assert_eq!(cpu_handle.column, 0);
    assert_eq!(memory_handle.column, 0);

    // Check series counts
    assert_eq!(registry.series_count(0), 1);
    assert_eq!(registry.series_count(1), 1);
    assert_eq!(registry.total_series_count(), 2);

    Ok(())
}