//! Example demonstrating series registration and usage.
//!
//! This example shows how to:
//! - Configure schemas with label matchers
//! - Register time series with labels
//! - Use handles for efficient operations
//! - Save and load series indices

use rondo::error::Result;
use rondo::schema::{LabelMatcher, SchemaConfig, TierConfig};
use rondo::series::SeriesRegistry;
use rondo::slab::Slab;
use std::time::Duration;

fn main() -> Result<()> {
    println!("🚀 Rondo Series Registration Example");

    // Create schema configurations for different types of metrics
    let cpu_schema = SchemaConfig {
        name: "cpu_metrics".to_string(),
        label_matcher: LabelMatcher::new([("type", "cpu")]),
        tiers: vec![
            // High resolution: 1-second samples for 1 hour
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(3600),
                consolidation_fn: None, // No consolidation for highest resolution
            },
        ],
        max_series: 100,
    };

    let memory_schema = SchemaConfig {
        name: "memory_metrics".to_string(),
        label_matcher: LabelMatcher::new([("type", "memory")]),
        tiers: vec![
            // Lower resolution: 5-second samples for 2 hours
            TierConfig {
                interval: Duration::from_secs(5),
                retention: Duration::from_secs(7200),
                consolidation_fn: None,
            },
        ],
        max_series: 50,
    };

    // Create series registry with schemas
    let mut registry = SeriesRegistry::new(vec![cpu_schema.clone(), memory_schema.clone()]);

    println!("\n📊 Registering time series...");

    // Register CPU metrics for different hosts
    let cpu_web1_labels = vec![
        ("type".to_string(), "cpu".to_string()),
        ("host".to_string(), "web1".to_string()),
        ("metric".to_string(), "usage".to_string()),
    ];

    let cpu_web2_labels = vec![
        ("type".to_string(), "cpu".to_string()),
        ("host".to_string(), "web2".to_string()),
        ("metric".to_string(), "usage".to_string()),
    ];

    let cpu_handle1 = registry.register("cpu.usage_percent", &cpu_web1_labels)?;
    let cpu_handle2 = registry.register("cpu.usage_percent", &cpu_web2_labels)?;

    println!(
        "   CPU web1: schema={}, series_id={}, column={}",
        cpu_handle1.schema_index, cpu_handle1.series_id, cpu_handle1.column
    );
    println!(
        "   CPU web2: schema={}, series_id={}, column={}",
        cpu_handle2.schema_index, cpu_handle2.series_id, cpu_handle2.column
    );

    // Register memory metrics
    let memory_web1_labels = vec![
        ("type".to_string(), "memory".to_string()),
        ("host".to_string(), "web1".to_string()),
        ("metric".to_string(), "usage".to_string()),
    ];

    let memory_handle = registry.register("memory.usage_bytes", &memory_web1_labels)?;

    println!(
        "   Memory web1: schema={}, series_id={}, column={}",
        memory_handle.schema_index, memory_handle.series_id, memory_handle.column
    );

    // Demonstrate lookup by labels
    println!("\n🔍 Testing lookups...");

    let found_handle = registry
        .get_handle("cpu.usage_percent", &cpu_web1_labels)
        .expect("Should find registered series");
    assert_eq!(found_handle, cpu_handle1);
    println!("   ✅ Found existing series by labels");

    // Try to register the same series again (should return existing handle)
    let duplicate_handle = registry.register("cpu.usage_percent", &cpu_web1_labels)?;
    assert_eq!(duplicate_handle, cpu_handle1);
    println!("   ✅ Duplicate registration returned same handle");

    // Display series info
    if let Some(info) = registry.series_info(&cpu_handle1) {
        println!(
            "   📋 Series info: name='{}', labels={:?}",
            info.name, info.labels
        );
    }

    // Show registry statistics
    println!("\n📈 Registry statistics:");
    println!("   Total series: {}", registry.total_series_count());
    println!("   CPU schema series: {}", registry.series_count(0));
    println!("   Memory schema series: {}", registry.series_count(1));

    // Create slabs for storage
    println!("\n💾 Creating storage slabs...");
    let temp_dir = tempfile::tempdir().map_err(|e| {
        rondo::error::RondoError::Store(rondo::error::StoreError::DirectoryAccess {
            path: "temp".to_string(),
            source: e,
        })
    })?;

    let cpu_slab_path = temp_dir.path().join("cpu_metrics.slab");
    let memory_slab_path = temp_dir.path().join("memory_metrics.slab");

    let mut cpu_slab = Slab::create(
        &cpu_slab_path,
        cpu_schema.stable_hash(),
        3600, // 1 hour of 1-second samples
        cpu_schema.max_series,
        1_000_000_000, // 1 second in nanoseconds
    )?;

    let mut memory_slab = Slab::create(
        &memory_slab_path,
        memory_schema.stable_hash(),
        1440, // 2 hours of 5-second samples
        memory_schema.max_series,
        5_000_000_000, // 5 seconds in nanoseconds
    )?;

    // Sync registry to slabs
    registry.sync_to_slabs(&mut [&mut cpu_slab, &mut memory_slab])?;

    println!(
        "   ✅ CPU slab: {} series registered",
        cpu_slab.series_count()
    );
    println!(
        "   ✅ Memory slab: {} series registered",
        memory_slab.series_count()
    );

    // Verify slab series directory is updated
    assert_eq!(
        cpu_slab.get_series_column(cpu_handle1.series_id),
        Some(cpu_handle1.column)
    );
    println!("   ✅ Slab series directory updated");

    // Save series index to disk
    println!("\n💾 Persisting series index...");
    let index_path = temp_dir.path().join("series_index.json");
    registry.save(&index_path)?;
    println!("   ✅ Series index saved to disk");

    // Load it back to verify persistence
    let loaded_registry = SeriesRegistry::load(&index_path, vec![cpu_schema, memory_schema])?;
    assert_eq!(
        loaded_registry.total_series_count(),
        registry.total_series_count()
    );

    let loaded_handle = loaded_registry
        .get_handle("cpu.usage_percent", &cpu_web1_labels)
        .expect("Should find series in loaded registry");
    assert_eq!(loaded_handle, cpu_handle1);
    println!("   ✅ Series index loaded and verified");

    println!("\n🎉 Series registration example completed successfully!");
    println!("\nKey benefits demonstrated:");
    println!("• 🚀 Hot path efficiency: SeriesHandle is Copy and contains pre-computed column");
    println!("• 🏷️  Label-based routing: Different schemas for different metric types");
    println!("• 🔒 Resource bounds: max_series prevents unbounded memory growth");
    println!("• 💾 Persistence: Series registrations survive store restarts");
    println!("• 🎯 Type safety: Handles prevent invalid storage access");

    Ok(())
}
