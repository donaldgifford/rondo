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

use crate::consolidate::ConsolidationEngine;
use crate::error::{QueryError, Result, StoreError};
use crate::query::{analyze_coverage, QueryResult};
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

/// Metadata about a single tier in the store.
#[derive(Debug, Clone)]
pub struct TierInfo {
    /// Number of slots in the ring buffer.
    pub slot_count: u32,
    /// Interval between slots in nanoseconds.
    pub interval_ns: u64,
    /// Oldest timestamp in the tier, or `None` if empty.
    pub oldest_timestamp: Option<u64>,
    /// Newest timestamp in the tier, or `None` if empty.
    pub newest_timestamp: Option<u64>,
    /// Whether the tier is empty.
    pub is_empty: bool,
    /// Whether the ring buffer has wrapped around.
    pub has_wrapped: bool,
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

    /// Returns the number of tiers for a given schema.
    pub fn tier_count(&self, schema_index: usize) -> usize {
        self.rings.get(schema_index).map_or(0, Vec::len)
    }

    /// Returns tier metadata for a given schema and tier.
    ///
    /// Returns `(slot_count, interval_ns, oldest_ts, newest_ts, is_empty, has_wrapped)`.
    pub fn tier_info(&self, schema_index: usize, tier_index: usize) -> Option<TierInfo> {
        let ring = self.rings.get(schema_index)?.get(tier_index)?;
        Some(TierInfo {
            slot_count: ring.slab().slot_count(),
            interval_ns: ring.slab().interval_ns(),
            oldest_timestamp: ring.oldest_timestamp(),
            newest_timestamp: ring.newest_timestamp(),
            is_empty: ring.is_empty(),
            has_wrapped: ring.has_wrapped(),
        })
    }

    /// Returns the series count for a specific schema.
    pub fn schema_series_count(&self, schema_index: usize) -> u32 {
        self.registry.series_count(schema_index)
    }

    /// Returns all registered series handles.
    pub fn handles(&self) -> Vec<SeriesHandle> {
        self.registry.handles()
    }

    /// Returns the series name and labels for a handle.
    pub fn series_info(&self, handle: &SeriesHandle) -> Option<(&str, &[(String, String)])> {
        let info = self.registry.series_info(handle)?;
        Some((info.name.as_str(), info.labels.as_slice()))
    }

    /// Queries data from a specific tier of a time series.
    ///
    /// This method provides direct access to a specific storage tier with
    /// explicit validation of tier index and time range. Use this when you
    /// need precise control over which tier to query, such as for debugging
    /// or when you know the optimal tier for your use case.
    ///
    /// # Arguments
    ///
    /// * `handle` - The series handle obtained from registration
    /// * `tier` - The tier index to query (0 = highest resolution)
    /// * `start_ns` - Start timestamp in nanoseconds (inclusive)
    /// * `end_ns` - End timestamp in nanoseconds (exclusive)
    ///
    /// # Returns
    ///
    /// A [`QueryResult`] containing the iterator and metadata about the query.
    ///
    /// # Errors
    ///
    /// - [`QueryError::InvalidTier`] if tier index is out of range
    /// - [`QueryError::InvalidTimeRange`] if start >= end
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let current_time_ns = 1_640_000_000_000_000_000u64;
    /// // Query high-resolution data (tier 0) for the last hour
    /// let one_hour_ago = current_time_ns - 3600 * 1_000_000_000;
    /// let result = store.query(handle, 0, one_hour_ago, current_time_ns)?;
    ///
    /// for (timestamp, value) in result {
    ///     println!("CPU at {}: {}%", timestamp, value);
    /// }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn query(
        &self,
        handle: SeriesHandle,
        tier: usize,
        start_ns: u64,
        end_ns: u64,
    ) -> Result<QueryResult<'_>> {
        // Validate tier index
        let schema = &self.schemas[handle.schema_index];
        if tier >= schema.tiers.len() {
            return Err(QueryError::InvalidTier {
                tier,
                max_tiers: schema.tiers.len(),
            }
            .into());
        }

        // Validate time range
        if start_ns >= end_ns {
            return Err(QueryError::InvalidTimeRange {
                start: start_ns,
                end: end_ns,
            }
            .into());
        }

        // Get the ring buffer for this schema and tier
        let ring = &self.rings[handle.schema_index][tier];

        // Get available time range from the ring buffer
        let oldest = ring.oldest_timestamp();
        let newest = ring.newest_timestamp();
        let available_range = (oldest, newest);

        // Analyze coverage to determine if data may be incomplete
        let (_, may_be_incomplete) = analyze_coverage(oldest, newest, start_ns, end_ns);

        // Create iterator from ring buffer
        let iterator = ring.read(handle.column, start_ns, end_ns)?;

        Ok(QueryResult::new(
            iterator,
            tier,
            available_range,
            (start_ns, end_ns),
            may_be_incomplete,
        ))
    }

    /// Queries data with automatic tier selection based on retention coverage.
    ///
    /// This method automatically selects the best tier to serve the query by
    /// choosing the highest-resolution tier whose retention window covers the
    /// requested time range. If no tier fully covers the range, it falls back
    /// to lower-resolution tiers to maximize data availability.
    ///
    /// The selection algorithm:
    /// 1. Start with the highest resolution tier (tier 0)
    /// 2. Check if its retention window covers the requested range
    /// 3. If yes, use that tier (best quality)
    /// 4. If no, try the next lower resolution tier
    /// 5. Continue until a tier with coverage is found
    /// 6. If no tier has coverage, use the lowest resolution tier
    ///
    /// # Arguments
    ///
    /// * `handle` - The series handle obtained from registration
    /// * `start_ns` - Start timestamp in nanoseconds (inclusive)
    /// * `end_ns` - End timestamp in nanoseconds (exclusive)
    ///
    /// # Returns
    ///
    /// A [`QueryResult`] containing the iterator and metadata about the selected
    /// tier and data completeness.
    ///
    /// # Errors
    ///
    /// - [`QueryError::InvalidTimeRange`] if start >= end
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let current_time_ns = 1_640_000_000_000_000_000u64;
    /// // Query historical data - let rondo pick the best tier
    /// let yesterday = current_time_ns - 24 * 3600 * 1_000_000_000;
    /// let result = store.query_auto(handle, yesterday, current_time_ns)?;
    ///
    /// println!("Used tier {} for query", result.tier_used());
    /// if result.may_be_incomplete() {
    ///     println!("Warning: some data may be missing due to retention limits");
    /// }
    ///
    /// for (timestamp, value) in result {
    ///     println!("CPU at {}: {}%", timestamp, value);
    /// }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn query_auto(
        &self,
        handle: SeriesHandle,
        start_ns: u64,
        end_ns: u64,
    ) -> Result<QueryResult<'_>> {
        // Validate time range
        if start_ns >= end_ns {
            return Err(QueryError::InvalidTimeRange {
                start: start_ns,
                end: end_ns,
            }
            .into());
        }

        let schema = &self.schemas[handle.schema_index];
        let mut selected_tier = 0;
        let mut best_coverage = false;

        // Find the best tier based on retention coverage
        for (tier_index, _tier_config) in schema.tiers.iter().enumerate() {
            let ring = &self.rings[handle.schema_index][tier_index];
            let oldest = ring.oldest_timestamp();
            let newest = ring.newest_timestamp();

            let (fully_covered, _) = analyze_coverage(oldest, newest, start_ns, end_ns);

            if fully_covered {
                // This tier fully covers the range, use it (prefer highest resolution)
                selected_tier = tier_index;
                best_coverage = true;
                break;
            } else if oldest.is_some() && newest.is_some() {
                // This tier has some data, consider it as fallback
                selected_tier = tier_index;
            }
        }

        // If no tier had full coverage and we haven't found any data at all,
        // just use tier 0 (this handles empty store case)
        if !best_coverage && schema.tiers.is_empty() {
            return Err(QueryError::InvalidTier {
                tier: 0,
                max_tiers: 0,
            }
            .into());
        }

        // Query the selected tier
        self.query(handle, selected_tier, start_ns, end_ns)
    }

    /// Performs consolidation across all schemas and tier pairs.
    ///
    /// This method creates a consolidation engine and runs consolidation for all
    /// configured tier pairs. It should be called periodically (e.g., every second)
    /// to keep lower resolution tiers up to date with new data in higher resolution tiers.
    ///
    /// The consolidation process:
    /// 1. For each schema with multiple tiers
    /// 2. For each adjacent tier pair (tier N → tier N+1)
    /// 3. Read new data from source tier since last consolidation cursor
    /// 4. Group data points into destination tier interval windows
    /// 5. Apply the destination tier's consolidation function to each window
    /// 6. Write consolidated values to destination tier
    /// 7. Update consolidation cursors to track progress
    ///
    /// # Returns
    ///
    /// The total number of consolidation operations performed. Returns 0 if no
    /// new data needed consolidation (idempotent behavior).
    ///
    /// # Errors
    ///
    /// Returns an error if consolidation fails for any tier pair, cursor loading/saving
    /// fails, or if there are I/O errors during the consolidation process.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// // Run consolidation - typically called from a periodic timer
    /// let operations = store.consolidate()?;
    /// println!("Performed {} consolidation operations", operations);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn consolidate(&mut self) -> Result<usize> {
        // Create consolidation engine
        let mut engine = ConsolidationEngine::new(&self.path, self.schemas.clone())?;

        // Run consolidation
        engine.consolidate(&mut self.rings)
    }

    /// Drains new data from the store for all registered series at the specified tier.
    ///
    /// Returns data points that haven't been exported yet according to the provided
    /// cursor. The cursor is updated to track the latest exported timestamp for each
    /// series.
    ///
    /// This is designed for periodic push to a remote TSDB. Each call returns only
    /// new data since the last drain.
    ///
    /// # Arguments
    ///
    /// * `tier` - The tier index to drain from (0 = highest resolution)
    /// * `cursor` - Export cursor tracking progress; updated in place
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the ring buffer fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # use rondo::export::ExportCursor;
    /// # let store = Store::open("./data", vec![])?;
    /// let mut cursor = ExportCursor::load_or_new("./data/export_cursor.json")?;
    ///
    /// // Drain new data from tier 0
    /// let exports = store.drain(0, &mut cursor)?;
    /// for export in &exports {
    ///     println!("Series {:?}: {} new points", export.handle, export.points.len());
    /// }
    ///
    /// // Persist cursor for next run
    /// cursor.save()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn drain(
        &self,
        tier: usize,
        cursor: &mut crate::export::ExportCursor,
    ) -> Result<Vec<crate::export::SeriesExport>> {
        let handles = self.registry.handles();
        let mut all_exports = Vec::new();

        for schema_index in 0..self.schemas.len() {
            let schema_handles: Vec<_> = handles
                .iter()
                .filter(|h| h.schema_index == schema_index)
                .copied()
                .collect();

            if schema_handles.is_empty() || tier >= self.rings[schema_index].len() {
                continue;
            }

            let exports =
                crate::export::drain_tier(&self.rings, schema_index, tier, &schema_handles, cursor)?;
            all_exports.extend(exports);
        }

        Ok(all_exports)
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

    #[test]
    fn test_query_specific_tier() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("query_tier_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        // Register a series
        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Write some data
        let base_time = 1_640_000_000_000_000_000u64;
        store.record(handle, 10.0, base_time).unwrap();
        store.record(handle, 20.0, base_time + 1_000_000_000).unwrap();
        store.record(handle, 30.0, base_time + 2_000_000_000).unwrap();

        // Query tier 0 (high resolution)
        let result = store.query(handle, 0, base_time, base_time + 3_000_000_000).unwrap();

        assert_eq!(result.tier_used(), 0);
        assert_eq!(result.requested_range(), (base_time, base_time + 3_000_000_000));

        let data: Vec<_> = result.collect_all();
        assert_eq!(data.len(), 3);
        assert_eq!(data[0], (base_time, 10.0));
        assert_eq!(data[1], (base_time + 1_000_000_000, 20.0));
        assert_eq!(data[2], (base_time + 2_000_000_000, 30.0));
    }

    #[test]
    fn test_query_invalid_tier() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("invalid_tier_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Try to query tier 99 (doesn't exist)
        let result = store.query(handle, 99, 1000, 2000);
        assert!(result.is_err());

        match result.unwrap_err() {
            crate::error::RondoError::Query(QueryError::InvalidTier { tier, max_tiers }) => {
                assert_eq!(tier, 99);
                assert_eq!(max_tiers, 2); // CPU schema has 2 tiers
            }
            other => panic!("Expected InvalidTier error, got: {:?}", other),
        }
    }

    #[test]
    fn test_query_invalid_time_range() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("invalid_range_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Try to query with start >= end
        let result1 = store.query(handle, 0, 2000, 2000);
        assert!(result1.is_err());

        let result2 = store.query(handle, 0, 3000, 2000);
        assert!(result2.is_err());

        // Both should be InvalidTimeRange errors
        assert!(matches!(result1.unwrap_err(), crate::error::RondoError::Query(QueryError::InvalidTimeRange { .. })));
        assert!(matches!(result2.unwrap_err(), crate::error::RondoError::Query(QueryError::InvalidTimeRange { .. })));
    }

    #[test]
    fn test_query_empty_result() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("empty_query_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Query without any data
        let result = store.query(handle, 0, 1000, 2000).unwrap();
        assert_eq!(result.tier_used(), 0);
        assert_eq!(result.available_range(), (None, None));
        assert!(result.may_be_incomplete());

        let data: Vec<_> = result.collect_all();
        assert_eq!(data.len(), 0);
    }

    #[test]
    fn test_query_range_filtering() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("range_filter_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Write data at different times
        let base_time = 1_640_000_000_000_000_000u64;
        store.record(handle, 10.0, base_time).unwrap();
        store.record(handle, 20.0, base_time + 1_000_000_000).unwrap();
        store.record(handle, 30.0, base_time + 2_000_000_000).unwrap();
        store.record(handle, 40.0, base_time + 3_000_000_000).unwrap();
        store.record(handle, 50.0, base_time + 4_000_000_000).unwrap();

        // Query a subset of the time range
        let start_query = base_time + 1_500_000_000; // Between second and third points
        let end_query = base_time + 3_500_000_000;   // Between fourth and fifth points

        let result = store.query(handle, 0, start_query, end_query).unwrap();
        let data: Vec<_> = result.collect_all();

        // Should only get the third and fourth data points
        assert_eq!(data.len(), 2);
        assert_eq!(data[0], (base_time + 2_000_000_000, 30.0));
        assert_eq!(data[1], (base_time + 3_000_000_000, 40.0));
    }

    #[test]
    fn test_query_auto_tier_selection() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("auto_select_store");

        // Create schemas with different retention windows
        let schemas = vec![
            SchemaConfig {
                name: "short_term".to_string(),
                label_matcher: LabelMatcher::new([("type", "cpu")]),
                tiers: vec![
                    TierConfig {
                        interval: Duration::from_secs(1),
                        retention: Duration::from_secs(60),  // 1 minute retention
                        consolidation_fn: None,
                    },
                    TierConfig {
                        interval: Duration::from_secs(60),
                        retention: Duration::from_secs(3600), // 1 hour retention
                        consolidation_fn: Some(ConsolidationFn::Average),
                    },
                ],
                max_series: 1000,
            }
        ];

        let mut store = Store::open(&store_path, schemas).unwrap();
        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Write data across time ranges
        let base_time = 1_640_000_000_000_000_000u64;

        // Recent data (tier 0 should handle this)
        store.record(handle, 10.0, base_time).unwrap();
        store.record(handle, 20.0, base_time + 30_000_000_000).unwrap();

        // Query recent data - should use tier 0
        let result = store.query_auto(handle, base_time, base_time + 45_000_000_000).unwrap();
        assert_eq!(result.tier_used(), 0);

        let data: Vec<_> = result.collect_all();
        assert_eq!(data.len(), 2);
    }

    #[test]
    fn test_query_auto_with_empty_store() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("empty_auto_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Query auto on empty store
        let result = store.query_auto(handle, 1000, 2000).unwrap();
        assert_eq!(result.tier_used(), 0); // Should default to tier 0
        assert!(result.may_be_incomplete());
        assert_eq!(result.available_range(), (None, None));

        let data: Vec<_> = result.collect_all();
        assert_eq!(data.len(), 0);
    }

    #[test]
    fn test_query_auto_invalid_time_range() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("auto_invalid_range_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Try query_auto with invalid range
        let result = store.query_auto(handle, 2000, 1000);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), crate::error::RondoError::Query(QueryError::InvalidTimeRange { .. })));
    }

    #[test]
    fn test_query_result_metadata() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("metadata_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Write some data
        let base_time = 1_640_000_000_000_000_000u64;
        store.record(handle, 42.0, base_time).unwrap();
        store.record(handle, 84.0, base_time + 1_000_000_000).unwrap();

        let query_start = base_time - 500_000_000; // Start before first data point
        let query_end = base_time + 2_000_000_000;

        let result = store.query(handle, 0, query_start, query_end).unwrap();

        // Test metadata methods
        assert_eq!(result.tier_used(), 0);
        assert_eq!(result.requested_range(), (query_start, query_end));

        let (oldest, newest) = result.available_range();
        assert_eq!(oldest, Some(base_time));
        assert_eq!(newest, Some(base_time + 1_000_000_000));

        // Should be marked as potentially incomplete because we requested
        // data from before the oldest available timestamp
        assert!(result.may_be_incomplete());
    }

    #[test]
    fn test_query_result_count() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("count_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Write multiple data points
        let base_time = 1_640_000_000_000_000_000u64;
        for i in 0u32..5 {
            store.record(handle, f64::from(i * 10), base_time + u64::from(i) * 1_000_000_000).unwrap();
        }

        let result = store.query(handle, 0, base_time, base_time + 5_000_000_000).unwrap();

        // Test count method (this consumes the iterator)
        assert_eq!(result.count(), 5);
    }

    #[test]
    fn test_query_multiple_schemas() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("multi_schema_query_store");
        let schemas = create_test_schemas();
        let mut store = Store::open(&store_path, schemas).unwrap();

        // Register series in different schemas
        let cpu_handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();
        let mem_handle = store.register("mem.usage", &[("type".to_string(), "memory".to_string())]).unwrap();

        // Write data to both
        let base_time = 1_640_000_000_000_000_000u64;
        store.record(cpu_handle, 80.0, base_time).unwrap();
        store.record(mem_handle, 60.0, base_time).unwrap();

        // Query both schemas
        let cpu_result = store.query(cpu_handle, 0, base_time, base_time + 1_000_000_000).unwrap();
        let mem_result = store.query(mem_handle, 0, base_time, base_time + 1_000_000_000).unwrap();

        // Verify they're in different schemas but queries work correctly
        assert_eq!(cpu_handle.schema_index, 0);
        assert_eq!(mem_handle.schema_index, 1);

        let cpu_data: Vec<_> = cpu_result.collect_all();
        let mem_data: Vec<_> = mem_result.collect_all();

        assert_eq!(cpu_data, vec![(base_time, 80.0)]);
        assert_eq!(mem_data, vec![(base_time, 60.0)]);
    }

    #[test]
    fn test_consolidation_basic() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("consolidation_store");

        // Create schema with multiple tiers
        let schemas = vec![
            SchemaConfig {
                name: "multi_tier".to_string(),
                label_matcher: LabelMatcher::new([("type", "cpu")]),
                tiers: vec![
                    TierConfig {
                        interval: Duration::from_secs(1),
                        retention: Duration::from_secs(60),
                        consolidation_fn: None,
                    },
                    TierConfig {
                        interval: Duration::from_secs(10),
                        retention: Duration::from_secs(600),
                        consolidation_fn: Some(ConsolidationFn::Average),
                    },
                ],
                max_series: 100,
            }
        ];

        let mut store = Store::open(&store_path, schemas).unwrap();
        let handle = store.register("cpu.usage", &[("type".to_string(), "cpu".to_string())]).unwrap();

        // Write some data to tier 0
        let base_time = 1_000_000_000_000_000_000u64; // 1s intervals in ns
        for i in 0u32..15 {
            let timestamp = base_time + u64::from(i) * 1_000_000_000;
            let value = f64::from(i * 10);
            store.record(handle, value, timestamp).unwrap();
        }

        // Run consolidation
        let operations = store.consolidate().unwrap();
        assert!(operations > 0, "Should have performed consolidation operations");

        // Verify consolidated data exists in tier 1
        let tier1_data: Vec<_> = store.query(handle, 1, base_time - 1, base_time + 20_000_000_000).unwrap().collect();
        assert!(!tier1_data.is_empty(), "Tier 1 should have consolidated data");

        // Second consolidation run should be idempotent (no new data)
        let operations2 = store.consolidate().unwrap();
        assert_eq!(operations2, 0, "Second consolidation should be a no-op");
    }

    #[test]
    fn test_consolidation_with_no_multi_tier_schemas() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("single_tier_store");

        // Create schema with only one tier
        let schemas = vec![
            SchemaConfig {
                name: "single_tier".to_string(),
                label_matcher: LabelMatcher::any(),
                tiers: vec![
                    TierConfig {
                        interval: Duration::from_secs(1),
                        retention: Duration::from_secs(60),
                        consolidation_fn: None,
                    },
                ],
                max_series: 100,
            }
        ];

        let mut store = Store::open(&store_path, schemas).unwrap();
        let handle = store.register("metric", &[]).unwrap();

        store.record(handle, 42.0, 1_000_000_000_000_000_000).unwrap();

        // Consolidation should be a no-op (no multi-tier schemas)
        let operations = store.consolidate().unwrap();
        assert_eq!(operations, 0);
    }

    #[test]
    fn test_consolidation_functions() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("consolidation_functions_store");

        // Create schema with different consolidation functions
        let schemas = vec![
            SchemaConfig {
                name: "test_functions".to_string(),
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
                        consolidation_fn: Some(ConsolidationFn::Min),
                    },
                    TierConfig {
                        interval: Duration::from_secs(15),
                        retention: Duration::from_secs(900),
                        consolidation_fn: Some(ConsolidationFn::Max),
                    },
                ],
                max_series: 50,
            }
        ];

        let mut store = Store::open(&store_path, schemas).unwrap();
        let handle = store.register("test_metric", &[]).unwrap();

        let base_time = 1_000_000_000_000_000_000u64;

        // Write data with varying values
        let values = [100.0, 50.0, 75.0, 25.0, 90.0, 10.0, 80.0, 60.0];
        for (i, &value) in values.iter().enumerate() {
            let timestamp = base_time + (i as u64 * 1_000_000_000);
            store.record(handle, value, timestamp).unwrap();
        }

        // Run consolidation multiple times to cascade through tiers
        for _ in 0..5 {
            let operations = store.consolidate().unwrap();
            if operations == 0 {
                break;
            }
        }

        // Check that data exists in tier 1 (Min consolidation)
        let tier1_data: Vec<_> = store.query(handle, 1, base_time - 1, base_time + 10_000_000_000).unwrap().collect();

        // Check that data exists in tier 2 (Max consolidation)
        let tier2_data: Vec<_> = store.query(handle, 2, base_time - 1, base_time + 20_000_000_000).unwrap().collect();

        // At least tier 1 should have data
        assert!(!tier1_data.is_empty() || !tier2_data.is_empty());
    }

    #[test]
    fn test_consolidation_cursor_persistence() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("cursor_persistence_store");

        let schemas = vec![
            SchemaConfig {
                name: "persistence_test".to_string(),
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
                        consolidation_fn: Some(ConsolidationFn::Average),
                    },
                ],
                max_series: 50,
            }
        ];

        let base_time = 1_000_000_000_000_000_000u64;

        // First store instance
        {
            let mut store = Store::open(&store_path, schemas.clone()).unwrap();
            let handle = store.register("persist_metric", &[]).unwrap();

            // Write initial data
            for i in 0u32..10 {
                let timestamp = base_time + u64::from(i) * 1_000_000_000;
                let value = f64::from(i * 5);
                store.record(handle, value, timestamp).unwrap();
            }

            // Consolidate
            let _operations = store.consolidate().unwrap();
        }

        // Second store instance (simulates restart)
        {
            let mut store = Store::open(&store_path, schemas).unwrap();
            let handle = store.register("persist_metric", &[]).unwrap();

            // Add more data
            for i in 10u32..15 {
                let timestamp = base_time + u64::from(i) * 1_000_000_000;
                let value = f64::from(i * 5);
                store.record(handle, value, timestamp).unwrap();
            }

            // Consolidate - should only process new data
            let _operations = store.consolidate().unwrap();

            // Verify that consolidation cursors file exists
            let cursor_file = store_path.join("consolidation_cursors.json");
            assert!(cursor_file.exists(), "Consolidation cursors should be persisted");
        }
    }

    #[test]
    fn test_consolidation_multiple_series() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("multi_series_consolidation_store");

        let schemas = vec![
            SchemaConfig {
                name: "multi_series_test".to_string(),
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
                        consolidation_fn: Some(ConsolidationFn::Sum),
                    },
                ],
                max_series: 10,
            }
        ];

        let mut store = Store::open(&store_path, schemas).unwrap();

        // Register multiple series
        let handle1 = store.register("metric1", &[]).unwrap();
        let handle2 = store.register("metric2", &[]).unwrap();
        let handle3 = store.register("metric3", &[]).unwrap();

        let base_time = 1_000_000_000_000_000_000u64;

        // Write data to multiple series
        for i in 0..8 {
            let timestamp = base_time + (i * 1_000_000_000);
            store.record(handle1, 10.0, timestamp).unwrap();
            store.record(handle2, 20.0, timestamp).unwrap();
            store.record(handle3, 30.0, timestamp).unwrap();
        }

        // Consolidate
        let operations = store.consolidate().unwrap();
        assert!(operations > 0);

        // Verify all series have consolidated data
        let series1_tier1: Vec<_> = store.query(handle1, 1, base_time - 1, base_time + 10_000_000_000).unwrap().collect();
        let series2_tier1: Vec<_> = store.query(handle2, 1, base_time - 1, base_time + 10_000_000_000).unwrap().collect();
        let series3_tier1: Vec<_> = store.query(handle3, 1, base_time - 1, base_time + 10_000_000_000).unwrap().collect();

        // At least one series should have consolidated data
        assert!(!series1_tier1.is_empty() || !series2_tier1.is_empty() || !series3_tier1.is_empty());
    }

    #[test]
    fn test_consolidation_empty_store() {
        let temp_dir = tempdir().unwrap();
        let store_path = temp_dir.path().join("empty_consolidation_store");

        let schemas = vec![
            SchemaConfig {
                name: "empty_test".to_string(),
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
                        consolidation_fn: Some(ConsolidationFn::Average),
                    },
                ],
                max_series: 10,
            }
        ];

        let mut store = Store::open(&store_path, schemas).unwrap();

        // Consolidate empty store - should be a no-op
        let operations = store.consolidate().unwrap();
        assert_eq!(operations, 0);

        // Register series but don't write data
        let _handle = store.register("empty_metric", &[]).unwrap();

        // Consolidate again - still should be a no-op
        let operations = store.consolidate().unwrap();
        assert_eq!(operations, 0);
    }
}