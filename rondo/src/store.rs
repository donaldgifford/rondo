//! Store module for the Rondo time-series storage engine.
//!
//! This module provides the top-level API that ties all components together.
//! The Store manages the complete storage system including schemas, series
//! registration, and ring buffers for actual time-series data storage.
//!
//! # Design
//!
//! The Store acts as the central coordinator:
//! - Manages store directory with meta.json and slab files
//! - Owns SeriesRegistry for series registration and handle management
//! - Maintains ring buffers indexed by [schema_index][tier_index]
//! - Provides zero-allocation record() hot path for writes
//! - Handles store lifecycle (create, open, close)
//!
//! # File Layout
//!
//! ```text
//! store_dir/
//! ├── meta.json                   <- Schema definitions and metadata
//! ├── series_index.bin            <- Serialized series registry
//! ├── schema_0/                   <- Directory for first schema
//! │   ├── tier_0.slab            <- Highest resolution tier
//! │   ├── tier_1.slab            <- Lower resolution tier
//! │   └── tier_N.slab            <- Lowest resolution tier
//! ├── schema_1/                   <- Directory for second schema
//! │   ├── tier_0.slab
//! │   └── ...
//! └── ...
//! ```
//!
//! # Example Usage
//!
//! ```rust,no_run
//! use rondo::store::Store;
//! use rondo::schema::{SchemaConfig, LabelMatcher, TierConfig, ConsolidationFn};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let schemas = vec![
//!     SchemaConfig {
//!         name: "cpu_metrics".to_string(),
//!         label_matcher: LabelMatcher::new([("type", "cpu")]),
//!         tiers: vec![
//!             TierConfig::new(Duration::from_secs(1), Duration::from_secs(3600), None)?,
//!             TierConfig::new(Duration::from_secs(60), Duration::from_secs(86400), Some(ConsolidationFn::Average))?,
//!         ],
//!         max_series: 1000,
//!     }
//! ];
//!
//! // Create or open store
//! let mut store = Store::open("./data", schemas)?;
//!
//! // Register a time series
//! let handle = store.register("cpu.usage", &[
//!     ("type".to_string(), "cpu".to_string()),
//!     ("host".to_string(), "web1".to_string())
//! ])?;
//!
//! // Record data (hot path - zero allocation)
//! store.record(handle, 85.5, 1_640_000_000_000_000_000u64)?;
//!
//! // Batch record multiple series at once
//! store.record_batch(&[
//!     (handle, 87.2),
//!     // ... more entries
//! ], 1_640_000_001_000_000_000u64)?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::ring::RingBuffer;
use crate::schema::SchemaConfig;
use crate::series::{SeriesHandle, SeriesRegistry};
use crate::slab::Slab;

/// Metadata file format version.
const METADATA_VERSION: u32 = 1;

/// Name of the metadata file in the store directory.
const METADATA_FILE: &str = "meta.json";

/// Name of the series index file in the store directory.
const SERIES_INDEX_FILE: &str = "series_index.bin";

/// Top-level store handle for rondo time-series storage.
///
/// The Store provides the main API for interacting with the time-series storage
/// system. It manages schemas, series registration, and the ring buffers that
/// contain actual time-series data.
///
/// # Thread Safety
///
/// The Store is designed for single-threaded access patterns. External
/// synchronization must be provided if used across multiple threads.
#[derive(Debug)]
pub struct Store {
    /// Path to the store directory.
    path: PathBuf,
    /// Schema configurations defining storage tiers and label routing.
    schemas: Vec<SchemaConfig>,
    /// Series registry managing series registration and handles.
    registry: SeriesRegistry,
    /// Ring buffers indexed by [schema_index][tier_index].
    rings: Vec<Vec<RingBuffer>>,
}

/// Metadata stored in the store's meta.json file.
#[derive(Debug, Serialize, Deserialize)]
struct StoreMetadata {
    /// Metadata format version.
    version: u32,
    /// Schema configurations with their hashes for validation.
    schemas: Vec<SchemaWithHash>,
}

/// Schema configuration with computed hash for validation.
#[derive(Debug, Serialize, Deserialize)]
struct SchemaWithHash {
    /// The schema configuration.
    #[serde(flatten)]
    config: SchemaConfig,
    /// Pre-computed stable hash for validation.
    hash: u64,
}

impl Store {
    /// Creates a new store or opens an existing one at the given path.
    ///
    /// If the directory doesn't exist:
    /// - Creates the directory structure
    /// - Writes meta.json with schema configurations and hashes
    /// - Creates all slab files (one per schema×tier combination)
    /// - Initializes an empty series registry
    ///
    /// If the directory exists:
    /// - Reads meta.json and validates schema hashes match
    /// - Opens existing slab files
    /// - Loads the series registry from series_index.bin
    ///
    /// # Arguments
    ///
    /// * `path` - Directory path for the store
    /// * `schemas` - Schema configurations that define storage behavior
    ///
    /// # Returns
    ///
    /// A Store instance ready for series registration and data recording.
    ///
    /// # Errors
    ///
    /// - [`StoreError::DirectoryAccess`] if directory cannot be created/accessed
    /// - [`StoreError::CorruptedMetadata`] if meta.json is invalid
    /// - [`StoreError::SchemaMismatch`] if schema hashes don't match existing store
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # use rondo::schema::{SchemaConfig, LabelMatcher};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let schemas = vec![/* schema configurations */];
    /// let store = Store::open("./my_store", schemas)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open<P: AsRef<Path>>(path: P, schemas: Vec<SchemaConfig>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Validate schemas
        for schema in &schemas {
            schema.validate()?;
        }

        if path.exists() {
            Self::open_existing(path, schemas)
        } else {
            Self::create_new(path, schemas)
        }
    }

    /// Creates a new store directory with initial files.
    fn create_new(path: PathBuf, schemas: Vec<SchemaConfig>) -> Result<Self> {
        // Create main directory
        fs::create_dir_all(&path).map_err(|e| StoreError::DirectoryAccess {
            path: path.display().to_string(),
            source: e,
        })?;

        // Create schema directories
        for (schema_index, _) in schemas.iter().enumerate() {
            let schema_dir = path.join(format!("schema_{}", schema_index));
            fs::create_dir_all(&schema_dir).map_err(|e| StoreError::DirectoryAccess {
                path: schema_dir.display().to_string(),
                source: e,
            })?;
        }

        // Write metadata file
        let metadata = StoreMetadata {
            version: METADATA_VERSION,
            schemas: schemas
                .iter()
                .map(|config| SchemaWithHash {
                    hash: config.stable_hash(),
                    config: config.clone(),
                })
                .collect(),
        };

        let metadata_path = path.join(METADATA_FILE);
        let metadata_json = serde_json::to_string_pretty(&metadata)
            .map_err(StoreError::MetadataSerialize)?;

        fs::write(&metadata_path, metadata_json).map_err(|e| StoreError::DirectoryAccess {
            path: metadata_path.display().to_string(),
            source: e,
        })?;

        // Create slab files for each schema×tier combination
        let mut rings = Vec::with_capacity(schemas.len());

        for (schema_index, schema) in schemas.iter().enumerate() {
            let mut schema_rings = Vec::with_capacity(schema.tiers.len());

            for (tier_index, tier) in schema.tiers.iter().enumerate() {
                let slab_path = path
                    .join(format!("schema_{}", schema_index))
                    .join(format!("tier_{}.slab", tier_index));

                #[allow(clippy::cast_possible_truncation)] // slot_count validated by TierConfig
                let slot_count = tier.slot_count() as u32;
                #[allow(clippy::cast_possible_truncation)] // Duration nanos fit in u64 for practical intervals
                let interval_ns = tier.interval.as_nanos() as u64;

                let slab = Slab::create(
                    slab_path,
                    schema.stable_hash(),
                    slot_count,
                    schema.max_series,
                    interval_ns,
                )?;

                schema_rings.push(RingBuffer::new(slab));
            }

            rings.push(schema_rings);
        }

        // Create empty series registry
        let registry = SeriesRegistry::new(schemas.clone());

        // Save empty series registry
        let series_index_path = path.join(SERIES_INDEX_FILE);
        registry.save(&series_index_path)?;

        Ok(Self {
            path,
            schemas,
            registry,
            rings,
        })
    }

    /// Opens an existing store directory.
    fn open_existing(path: PathBuf, schemas: Vec<SchemaConfig>) -> Result<Self> {
        // Read and validate metadata
        let metadata_path = path.join(METADATA_FILE);
        let metadata_json = fs::read_to_string(&metadata_path).map_err(|e| {
            StoreError::DirectoryAccess {
                path: metadata_path.display().to_string(),
                source: e,
            }
        })?;

        let metadata: StoreMetadata = serde_json::from_str(&metadata_json)
            .map_err(StoreError::MetadataSerialize)?;

        // Validate metadata version
        if metadata.version != METADATA_VERSION {
            return Err(StoreError::CorruptedMetadata {
                reason: format!(
                    "unsupported metadata version: expected {}, found {}",
                    METADATA_VERSION, metadata.version
                ),
            }
            .into());
        }

        // Validate schema count matches
        if schemas.len() != metadata.schemas.len() {
            return Err(StoreError::CorruptedMetadata {
                reason: format!(
                    "schema count mismatch: expected {}, found {} in metadata",
                    schemas.len(),
                    metadata.schemas.len()
                ),
            }
            .into());
        }

        // Validate schema hashes match
        for (index, (provided, stored)) in schemas.iter().zip(metadata.schemas.iter()).enumerate() {
            let provided_hash = provided.stable_hash();
            if provided_hash != stored.hash {
                return Err(StoreError::SchemaMismatch {
                    existing: stored.hash,
                    expected: provided_hash,
                }
                .into());
            }

            // Validate tier count matches
            if provided.tiers.len() != stored.config.tiers.len() {
                return Err(StoreError::CorruptedMetadata {
                    reason: format!(
                        "tier count mismatch for schema {}: expected {}, found {}",
                        index,
                        provided.tiers.len(),
                        stored.config.tiers.len()
                    ),
                }
                .into());
            }
        }

        // Open existing slab files
        let mut rings = Vec::with_capacity(schemas.len());

        for (schema_index, schema) in schemas.iter().enumerate() {
            let mut schema_rings = Vec::with_capacity(schema.tiers.len());

            for tier_index in 0..schema.tiers.len() {
                let slab_path = path
                    .join(format!("schema_{}", schema_index))
                    .join(format!("tier_{}.slab", tier_index));

                let slab = Slab::open(slab_path)?;
                schema_rings.push(RingBuffer::new(slab));
            }

            rings.push(schema_rings);
        }

        // Load series registry
        let series_index_path = path.join(SERIES_INDEX_FILE);
        let registry = if series_index_path.exists() {
            SeriesRegistry::load(&series_index_path, schemas.clone())?
        } else {
            // Handle case where series index doesn't exist (legacy or corrupted)
            SeriesRegistry::new(schemas.clone())
        };

        Ok(Self {
            path,
            schemas,
            registry,
            rings,
        })
    }

    /// Registers a new time series and returns a handle for efficient writes.
    ///
    /// This method is not on the hot path and can perform allocations.
    /// It delegates to the series registry and synchronizes the new series
    /// to all tier slabs for the matching schema.
    ///
    /// # Arguments
    ///
    /// * `name` - The series name (must be non-empty)
    /// * `labels` - Label key-value pairs that determine schema routing
    ///
    /// # Returns
    ///
    /// A [`SeriesHandle`] that can be used for zero-allocation writes.
    ///
    /// # Errors
    ///
    /// Returns an error if series validation fails, no schema matches,
    /// or maximum series count is exceeded.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("test", vec![])?;
    /// let handle = store.register("cpu.usage", &[
    ///     ("type".to_string(), "cpu".to_string()),
    ///     ("host".to_string(), "web1".to_string()),
    /// ])?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn register(
        &mut self,
        name: &str,
        labels: &[(String, String)],
    ) -> Result<SeriesHandle> {
        // Register with series registry
        let handle = self.registry.register(name, labels)?;

        // Sync the new series to all tier slabs for this schema
        let schema_index = handle.schema_index;
        let mut slab_refs: Vec<&mut Slab> = self.rings[schema_index]
            .iter_mut()
            .map(|ring| ring.slab_mut())
            .collect();

        self.registry.sync_to_slabs(&mut slab_refs)?;

        // Persist the updated series registry
        let series_index_path = self.path.join(SERIES_INDEX_FILE);
        self.registry.save(&series_index_path)?;

        Ok(handle)
    }

    /// Records a single value for a time series.
    ///
    /// This is the primary hot path operation and performs zero allocations.
    /// It writes directly to the highest resolution tier (tier 0) for the
    /// series's schema.
    ///
    /// # Arguments
    ///
    /// * `handle` - Series handle from registration
    /// * `value` - The f64 value to record
    /// * `timestamp_ns` - Timestamp in nanoseconds since Unix epoch
    ///
    /// # Errors
    ///
    /// Returns an error if the value or timestamp is invalid.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("test", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// // Record CPU usage at current time
    /// store.record(handle, 85.5, 1_640_000_000_000_000_000u64)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[inline]
    pub fn record(&mut self, handle: SeriesHandle, value: f64, timestamp_ns: u64) -> Result<()> {
        // Write to the highest resolution tier (tier 0) for this schema
        self.rings[handle.schema_index][0].write(handle.column, value, timestamp_ns)
    }

    /// Records multiple series values at the same timestamp in a batch operation.
    ///
    /// This is more efficient than individual writes as it groups entries by
    /// schema and performs batch writes to minimize overhead.
    ///
    /// # Arguments
    ///
    /// * `entries` - Slice of (handle, value) pairs to record
    /// * `timestamp_ns` - Timestamp in nanoseconds for all entries
    ///
    /// # Errors
    ///
    /// Returns an error if any value or the timestamp is invalid.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("test", vec![])?;
    /// # let cpu_handle = store.register("cpu.usage", &[])?;
    /// # let mem_handle = store.register("mem.usage", &[])?;
    /// // Record multiple metrics at once
    /// store.record_batch(&[
    ///     (cpu_handle, 85.5),
    ///     (mem_handle, 67.2),
    /// ], 1_640_000_000_000_000_000u64)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn record_batch(
        &mut self,
        entries: &[(SeriesHandle, f64)],
        timestamp_ns: u64,
    ) -> Result<()> {
        // Group entries by schema index
        let mut schema_groups: HashMap<usize, Vec<(u32, f64)>> = HashMap::new();

        for &(handle, value) in entries {
            schema_groups
                .entry(handle.schema_index)
                .or_default()
                .push((handle.column, value));
        }

        // Write batch to each schema's tier 0 ring buffer
        for (schema_index, batch_entries) in schema_groups {
            self.rings[schema_index][0].write_batch(&batch_entries, timestamp_ns)?;
        }

        Ok(())
    }

    /// Returns references to the schema configurations.
    pub fn schemas(&self) -> &[SchemaConfig] {
        &self.schemas
    }

    /// Returns the total number of registered series across all schemas.
    pub fn series_count(&self) -> usize {
        self.registry.total_series_count()
    }

    /// Returns the path to the store directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LabelMatcher, TierConfig, ConsolidationFn};
    use std::time::Duration;
    use tempfile::tempdir;

    fn create_test_schemas() -> Vec<SchemaConfig> {
        vec![
            SchemaConfig {
                name: "cpu_metrics".to_string(),
                label_matcher: LabelMatcher::new([("type", "cpu")]),
                tiers: vec![
                    TierConfig {
                        interval: Duration::from_secs(1),
                        retention: Duration::from_secs(3600),
                        consolidation_fn: None,
                    },
                    TierConfig {
                        interval: Duration::from_secs(60),
                        retention: Duration::from_secs(86400),
                        consolidation_fn: Some(ConsolidationFn::Average),
                    },
                ],
                max_series: 1000,
            },
            SchemaConfig {
                name: "memory_metrics".to_string(),
                label_matcher: LabelMatcher::new([("type", "memory")]),
                tiers: vec![TierConfig {
                    interval: Duration::from_secs(5),
                    retention: Duration::from_secs(7200),
                    consolidation_fn: None,
                }],
                max_series: 500,
            },
        ]
    }

    #[test]
    fn test_create_new_store() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("new_store");
        let schemas = create_test_schemas();

        let store = Store::open(&store_path, schemas.clone()).unwrap();

        // Verify directory structure
        assert!(store_path.exists());
        assert!(store_path.join("meta.json").exists());
        assert!(store_path.join("series_index.bin").exists());
        assert!(store_path.join("schema_0").exists());
        assert!(store_path.join("schema_1").exists());
        assert!(store_path.join("schema_0/tier_0.slab").exists());
        assert!(store_path.join("schema_0/tier_1.slab").exists());
        assert!(store_path.join("schema_1/tier_0.slab").exists());

        // Verify store properties
        assert_eq!(store.schemas().len(), 2);
        assert_eq!(store.series_count(), 0);
        assert_eq!(store.path(), store_path);
    }

    #[test]
    fn test_reopen_existing_store() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("existing_store");
        let schemas = create_test_schemas();

        // Create initial store
        let mut store1 = Store::open(&store_path, schemas.clone()).unwrap();

        // Register a series
        let handle = store1
            .register("cpu.usage", &[("type".to_string(), "cpu".to_string())])
            .unwrap();
        assert_eq!(store1.series_count(), 1);

        // Close store by dropping it
        drop(store1);

        // Reopen store
        let store2 = Store::open(&store_path, schemas.clone()).unwrap();

        // Verify state was preserved
        assert_eq!(store2.series_count(), 1);
        assert_eq!(store2.schemas().len(), 2);

        // Verify we can look up the same series
        let same_handle = store2.registry.get_handle("cpu.usage", &[("type".to_string(), "cpu".to_string())]);
        assert_eq!(same_handle, Some(handle));
    }

    #[test]
    fn test_register_and_record_round_trip() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("round_trip_store");
        let schemas = create_test_schemas();

        let mut store = Store::open(&store_path, schemas).unwrap();

        // Register series
        let cpu_handle = store
            .register("cpu.usage", &[
                ("type".to_string(), "cpu".to_string()),
                ("host".to_string(), "web1".to_string()),
            ])
            .unwrap();

        let mem_handle = store
            .register("memory.usage", &[
                ("type".to_string(), "memory".to_string()),
                ("host".to_string(), "web1".to_string()),
            ])
            .unwrap();

        // Verify handles are different and in correct schemas
        assert_ne!(cpu_handle, mem_handle);
        assert_eq!(cpu_handle.schema_index, 0); // CPU schema
        assert_eq!(mem_handle.schema_index, 1); // Memory schema

        // Record some data
        let timestamp = 1_640_000_000_000_000_000u64; // 2021-12-20 16:00:00 UTC in ns

        store.record(cpu_handle, 85.5, timestamp).unwrap();
        store.record(mem_handle, 67.2, timestamp + 1_000_000_000).unwrap();

        // Verify data was written to the correct ring buffers
        let cpu_ring = &store.rings[0][0]; // CPU schema, tier 0
        let mem_ring = &store.rings[1][0]; // Memory schema, tier 0

        // Read back the data
        let cpu_data: Vec<_> = cpu_ring.read(cpu_handle.column, timestamp - 1, timestamp + 1).unwrap().collect();
        let mem_data: Vec<_> = mem_ring.read(mem_handle.column, timestamp, timestamp + 2_000_000_000).unwrap().collect();

        assert_eq!(cpu_data, vec![(timestamp, 85.5)]);
        assert_eq!(mem_data, vec![(timestamp + 1_000_000_000, 67.2)]);
    }

    #[test]
    fn test_record_batch() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("batch_store");
        let schemas = create_test_schemas();

        let mut store = Store::open(&store_path, schemas).unwrap();

        // Register multiple series
        let cpu_handle = store
            .register("cpu.usage", &[("type".to_string(), "cpu".to_string())])
            .unwrap();

        let cpu2_handle = store
            .register("cpu.idle", &[("type".to_string(), "cpu".to_string())])
            .unwrap();

        let mem_handle = store
            .register("memory.usage", &[("type".to_string(), "memory".to_string())])
            .unwrap();

        // Record batch with mixed schemas
        let timestamp = 1_640_000_000_000_000_000u64;
        let entries = &[
            (cpu_handle, 85.5),
            (cpu2_handle, 14.5),
            (mem_handle, 67.2),
        ];

        store.record_batch(entries, timestamp).unwrap();

        // Verify all data was written correctly
        let cpu_ring = &store.rings[0][0];
        let mem_ring = &store.rings[1][0];

        let cpu_data: Vec<_> = cpu_ring.read(cpu_handle.column, timestamp - 1, timestamp + 1).unwrap().collect();
        let cpu2_data: Vec<_> = cpu_ring.read(cpu2_handle.column, timestamp - 1, timestamp + 1).unwrap().collect();
        let mem_data: Vec<_> = mem_ring.read(mem_handle.column, timestamp - 1, timestamp + 1).unwrap().collect();

        assert_eq!(cpu_data, vec![(timestamp, 85.5)]);
        assert_eq!(cpu2_data, vec![(timestamp, 14.5)]);
        assert_eq!(mem_data, vec![(timestamp, 67.2)]);
    }

    #[test]
    fn test_schema_mismatch_detection() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("mismatch_store");
        let schemas = create_test_schemas();

        // Create store with original schemas
        let _store1 = Store::open(&store_path, schemas).unwrap();
        drop(_store1);

        // Try to open with different schemas (same count but different configs)
        let different_schemas = vec![
            SchemaConfig {
                name: "different_cpu_schema".to_string(),
                label_matcher: LabelMatcher::new([("type", "different_cpu")]), // Different matcher
                tiers: vec![TierConfig {
                    interval: Duration::from_secs(10), // Different interval
                    retention: Duration::from_secs(3600),
                    consolidation_fn: None,
                }],
                max_series: 100, // Different max_series
            },
            SchemaConfig {
                name: "different_memory_schema".to_string(),
                label_matcher: LabelMatcher::new([("type", "different_memory")]),
                tiers: vec![TierConfig {
                    interval: Duration::from_secs(10),
                    retention: Duration::from_secs(7200),
                    consolidation_fn: None,
                }],
                max_series: 200,
            },
        ];

        let result = Store::open(&store_path, different_schemas);
        assert!(result.is_err());

        // Should be a schema mismatch error
        match result.unwrap_err() {
            crate::error::RondoError::Store(StoreError::SchemaMismatch { .. }) => {
                // This is expected
            }
            other => panic!("Expected SchemaMismatch error, got: {:?}", other),
        }
    }

    #[test]
    fn test_corrupted_metadata_detection() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("corrupted_store");
        let schemas = create_test_schemas();

        // Create store
        let _store = Store::open(&store_path, schemas.clone()).unwrap();
        drop(_store);

        // Corrupt the metadata file
        let meta_path = store_path.join("meta.json");
        fs::write(&meta_path, "{ invalid json }").unwrap();

        // Try to reopen
        let result = Store::open(&store_path, schemas);
        assert!(result.is_err());

        match result.unwrap_err() {
            crate::error::RondoError::Store(StoreError::MetadataSerialize(_)) => {
                // This is expected
            }
            other => panic!("Expected MetadataSerialize error, got: {:?}", other),
        }
    }

    #[test]
    fn test_directory_structure_validation() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("structure_test");
        let schemas = create_test_schemas();

        let _store = Store::open(&store_path, schemas).unwrap();

        // Verify the expected directory structure exists
        assert!(store_path.is_dir());
        assert!(store_path.join("meta.json").is_file());
        assert!(store_path.join("series_index.bin").is_file());

        // Schema directories
        assert!(store_path.join("schema_0").is_dir());
        assert!(store_path.join("schema_1").is_dir());

        // Slab files for schema 0 (2 tiers)
        assert!(store_path.join("schema_0/tier_0.slab").is_file());
        assert!(store_path.join("schema_0/tier_1.slab").is_file());

        // Slab files for schema 1 (1 tier)
        assert!(store_path.join("schema_1/tier_0.slab").is_file());
        assert!(!store_path.join("schema_1/tier_1.slab").exists());

        // Verify metadata content
        let meta_content = fs::read_to_string(store_path.join("meta.json")).unwrap();
        let metadata: StoreMetadata = serde_json::from_str(&meta_content).unwrap();

        assert_eq!(metadata.version, METADATA_VERSION);
        assert_eq!(metadata.schemas.len(), 2);
        assert_eq!(metadata.schemas[0].config.name, "cpu_metrics");
        assert_eq!(metadata.schemas[1].config.name, "memory_metrics");
    }

    #[test]
    fn test_empty_store_operations() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("empty_store");
        let schemas = create_test_schemas();

        let store = Store::open(&store_path, schemas).unwrap();

        // Empty store should have correct initial state
        assert_eq!(store.series_count(), 0);
        assert_eq!(store.schemas().len(), 2);
        assert_eq!(store.path(), store_path);
    }

    #[test]
    fn test_invalid_schema_rejection() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("invalid_schema_store");

        // Create schema with invalid configuration
        let invalid_schemas = vec![SchemaConfig {
            name: "invalid".to_string(),
            label_matcher: LabelMatcher::any(),
            tiers: vec![], // Empty tiers should be invalid
            max_series: 100,
        }];

        let result = Store::open(&store_path, invalid_schemas);
        assert!(result.is_err());
    }

    #[test]
    fn test_multiple_registrations_same_series() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("multiple_reg_store");
        let schemas = create_test_schemas();

        let mut store = Store::open(&store_path, schemas).unwrap();

        let labels = vec![("type".to_string(), "cpu".to_string())];

        // Register same series twice
        let handle1 = store.register("cpu.usage", &labels).unwrap();
        let handle2 = store.register("cpu.usage", &labels).unwrap();

        // Should return the same handle
        assert_eq!(handle1, handle2);

        // Should not increase series count
        assert_eq!(store.series_count(), 1);
    }
}