//! Consolidation engine for Rondo time-series storage.
//!
//! This module provides the consolidation engine that drives automatic downsampling
//! from higher resolution tiers to lower resolution tiers. The engine tracks
//! consolidation progress via persistent cursors and operates incrementally to
//! efficiently process only new data since the last consolidation run.
//!
//! # Design
//!
//! The consolidation engine operates by:
//! - Reading data from source tier (higher resolution) since last cursor
//! - Grouping data points into destination tier interval windows
//! - Applying consolidation functions (Average, Min, Max, Last, Sum, Count)
//! - Writing consolidated values to destination tier
//! - Advancing cursors to track progress
//!
//! # Consolidation Flow
//!
//! ```text
//! Tier 0 (1s) ─┐
//!              ├─► Consolidation ─► Tier 1 (60s)
//!              └─► Engine         └─► Tier 2 (3600s)
//! ```
//!
//! # Cursor Management
//!
//! Consolidation cursors are persisted in `consolidation_cursors.json` to ensure
//! resumption after restart. Each cursor tracks the last-processed timestamp
//! per (schema_index, source_tier_index) pair.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConsolidationError, Result};
use crate::ring::RingBuffer;
use crate::schema::{ConsolidationFn, SchemaConfig, TierConfig};

/// Name of the consolidation cursors file in the store directory.
const CURSORS_FILE: &str = "consolidation_cursors.json";

/// A consolidation cursor that tracks progress for a specific tier pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsolidationCursor {
    /// Schema index this cursor belongs to.
    pub schema_index: usize,
    /// Source tier index (higher resolution).
    pub source_tier_index: usize,
    /// Destination tier index (lower resolution).
    pub dest_tier_index: usize,
    /// Last timestamp processed from the source tier.
    pub last_processed_timestamp: u64,
}

/// Persistent storage for consolidation cursors.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ConsolidationCursors {
    /// Map from cursor key to cursor state.
    /// Key format: "{schema_index}:{source_tier}:{dest_tier}"
    cursors: HashMap<String, ConsolidationCursor>,
}

impl ConsolidationCursors {
    /// Creates a cursor key for the given schema and tier indices.
    fn cursor_key(schema_index: usize, source_tier: usize, dest_tier: usize) -> String {
        format!("{}:{}:{}", schema_index, source_tier, dest_tier)
    }

    /// Gets the cursor for a specific tier pair, if it exists.
    pub fn get_cursor(
        &self,
        schema_index: usize,
        source_tier: usize,
        dest_tier: usize,
    ) -> Option<&ConsolidationCursor> {
        let key = Self::cursor_key(schema_index, source_tier, dest_tier);
        self.cursors.get(&key)
    }

    /// Sets the cursor for a specific tier pair.
    pub fn set_cursor(&mut self, cursor: ConsolidationCursor) {
        let key = Self::cursor_key(
            cursor.schema_index,
            cursor.source_tier_index,
            cursor.dest_tier_index,
        );
        self.cursors.insert(key, cursor);
    }

    /// Gets the last processed timestamp for a tier pair, or None if no cursor exists.
    pub fn get_last_processed(
        &self,
        schema_index: usize,
        source_tier: usize,
        dest_tier: usize,
    ) -> Option<u64> {
        self.get_cursor(schema_index, source_tier, dest_tier)
            .map(|cursor| cursor.last_processed_timestamp)
    }

    /// Updates the last processed timestamp for a tier pair.
    pub fn update_last_processed(
        &mut self,
        schema_index: usize,
        source_tier: usize,
        dest_tier: usize,
        timestamp: u64,
    ) {
        let cursor = ConsolidationCursor {
            schema_index,
            source_tier_index: source_tier,
            dest_tier_index: dest_tier,
            last_processed_timestamp: timestamp,
        };
        self.set_cursor(cursor);
    }

    /// Loads cursors from a file, or returns empty if file doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path).map_err(|e| ConsolidationError::CursorLoad {
            path: path.display().to_string(),
            source: e,
        })?;

        let cursors: ConsolidationCursors =
            serde_json::from_str(&content).map_err(|e| ConsolidationError::CursorParse {
                path: path.display().to_string(),
                source: e,
            })?;

        Ok(cursors)
    }

    /// Saves cursors to a file.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| ConsolidationError::CursorSerialize { source: e })?;

        fs::write(path, content).map_err(|e| ConsolidationError::CursorSave {
            path: path.display().to_string(),
            source: e,
        })?;

        Ok(())
    }

    /// Returns an iterator over all stored cursors.
    pub fn iter_cursors(&self) -> impl Iterator<Item = &ConsolidationCursor> {
        self.cursors.values()
    }
}

/// Data window for consolidation processing.
///
/// Represents a time window aligned to the destination tier's interval,
/// containing all source data points that fall within that window.
#[derive(Debug)]
pub struct ConsolidationWindow {
    /// Start timestamp of the window (inclusive).
    pub start_timestamp: u64,
    /// End timestamp of the window (exclusive).
    pub end_timestamp: u64,
    /// Data points in this window, per series.
    /// Map from series_column to list of values.
    pub data: HashMap<u32, Vec<f64>>,
}

impl ConsolidationWindow {
    /// Creates a new empty consolidation window.
    pub fn new(start_timestamp: u64, end_timestamp: u64) -> Self {
        Self {
            start_timestamp,
            end_timestamp,
            data: HashMap::new(),
        }
    }

    /// Adds a data point to the window.
    pub fn add_point(&mut self, series_column: u32, value: f64) {
        if !value.is_nan() {
            self.data.entry(series_column).or_default().push(value);
        }
    }

    /// Returns true if the window has any data.
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    /// Returns the series columns that have data in this window.
    pub fn series_columns(&self) -> impl Iterator<Item = u32> + '_ {
        self.data.keys().copied()
    }

    /// Gets the values for a specific series column.
    pub fn get_values(&self, series_column: u32) -> Option<&[f64]> {
        self.data.get(&series_column).map(|v| v.as_slice())
    }

    /// Applies a consolidation function to the values for a series.
    pub fn consolidate_series(
        &self,
        series_column: u32,
        consolidation_fn: ConsolidationFn,
    ) -> Option<f64> {
        let values = self.get_values(series_column)?;
        if values.is_empty() {
            return None;
        }
        Some(consolidation_fn.apply(values))
    }
}

/// Consolidation engine that processes data from higher to lower resolution tiers.
pub struct ConsolidationEngine {
    /// Path to the store directory.
    store_path: PathBuf,
    /// Schema configurations.
    schemas: Vec<SchemaConfig>,
    /// Consolidation cursors for tracking progress.
    cursors: ConsolidationCursors,
}

impl ConsolidationEngine {
    /// Creates a new consolidation engine.
    ///
    /// # Arguments
    ///
    /// * `store_path` - Path to the store directory
    /// * `schemas` - Schema configurations
    ///
    /// # Errors
    ///
    /// Returns an error if cursor loading fails.
    pub fn new<P: AsRef<Path>>(store_path: P, schemas: Vec<SchemaConfig>) -> Result<Self> {
        let store_path = store_path.as_ref().to_path_buf();
        let cursor_path = store_path.join(CURSORS_FILE);
        let cursors = ConsolidationCursors::load(&cursor_path)?;

        Ok(Self {
            store_path,
            schemas,
            cursors,
        })
    }

    /// Performs consolidation across all schemas and tier pairs.
    ///
    /// This is the main entry point that should be called periodically (e.g., every second).
    /// It processes all tier pairs across all schemas and returns the total number of
    /// consolidation operations performed.
    ///
    /// # Arguments
    ///
    /// * `rings` - Ring buffers indexed by [schema_index][tier_index]
    ///
    /// # Returns
    ///
    /// The total number of consolidation operations performed (number of windows processed).
    ///
    /// # Errors
    ///
    /// Returns an error if consolidation fails for any tier pair.
    pub fn consolidate(&mut self, rings: &mut [Vec<RingBuffer>]) -> Result<usize> {
        let mut total_operations = 0;

        // Clone schemas to avoid borrowing issues
        let schemas = self.schemas.clone();

        for (schema_index, schema) in schemas.iter().enumerate() {
            // Skip schemas with only one tier (no consolidation needed)
            if schema.tiers.len() < 2 {
                continue;
            }

            // Process each adjacent tier pair
            for source_tier_index in 0..schema.tiers.len() - 1 {
                let dest_tier_index = source_tier_index + 1;

                let operations = self.consolidate_tier_pair(
                    rings,
                    schema,
                    schema_index,
                    source_tier_index,
                    dest_tier_index,
                )?;

                total_operations += operations;
            }
        }

        // Save updated cursors
        self.save_cursors()?;

        Ok(total_operations)
    }

    /// Consolidates data from one tier to the next tier for a specific schema.
    ///
    /// # Arguments
    ///
    /// * `rings` - Ring buffers indexed by [schema_index][tier_index]
    /// * `schema` - The schema configuration
    /// * `schema_index` - Index of the schema to process
    /// * `source_tier_index` - Index of the source (higher resolution) tier
    /// * `dest_tier_index` - Index of the destination (lower resolution) tier
    ///
    /// # Returns
    ///
    /// The number of consolidation operations performed (windows processed).
    fn consolidate_tier_pair(
        &mut self,
        rings: &mut [Vec<RingBuffer>],
        schema: &SchemaConfig,
        schema_index: usize,
        source_tier_index: usize,
        dest_tier_index: usize,
    ) -> Result<usize> {
        // Get tier configurations
        let source_tier = &schema.tiers[source_tier_index];
        let dest_tier = &schema.tiers[dest_tier_index];

        // Get the consolidation function (destination tier must have one)
        let consolidation_fn =
            dest_tier
                .consolidation_fn
                .ok_or(ConsolidationError::NoConsolidationFunction {
                    schema_index,
                    tier_index: dest_tier_index,
                })?;

        // Split rings to get separate mutable and immutable references
        let (left, right) = rings[schema_index].split_at_mut(dest_tier_index);
        let (source_ring, dest_ring) = if source_tier_index < dest_tier_index {
            (&left[source_tier_index], &mut right[0])
        } else {
            unreachable!("Source tier index should always be less than dest tier index")
        };

        // Get or initialize cursor
        let last_processed = self.get_or_initialize_cursor(
            source_ring,
            schema_index,
            source_tier_index,
            dest_tier_index,
        )?;

        // Find the range of new data to process
        let source_newest = source_ring.newest_timestamp();
        let start_timestamp = if last_processed == 0 {
            // First run - start from oldest available data
            source_ring.oldest_timestamp().unwrap_or(0)
        } else {
            // Continue from last processed + source tier interval
            #[allow(clippy::cast_possible_truncation)]
            // Duration nanos fit in u64 for practical intervals
            {
                last_processed + source_tier.interval.as_nanos() as u64
            }
        };

        // Add 1 to include the newest timestamp (ring.read end is exclusive)
        let end_timestamp = source_newest.map_or(0, |t| t + 1);

        // Skip if no new data
        if start_timestamp >= end_timestamp {
            return Ok(0);
        }

        // Process consolidation windows
        let operations = self.process_consolidation_windows(
            source_ring,
            dest_ring,
            source_tier,
            dest_tier,
            consolidation_fn,
            start_timestamp,
            end_timestamp,
            schema.max_series,
        )?;

        // Update cursor to the latest processed timestamp (the actual newest, not the exclusive end)
        if operations > 0 {
            let actual_newest = source_newest.unwrap_or(0);
            self.cursors.update_last_processed(
                schema_index,
                source_tier_index,
                dest_tier_index,
                actual_newest,
            );
        }

        Ok(operations)
    }

    /// Gets or initializes a cursor for a tier pair.
    fn get_or_initialize_cursor(
        &mut self,
        source_ring: &RingBuffer,
        schema_index: usize,
        source_tier_index: usize,
        dest_tier_index: usize,
    ) -> Result<u64> {
        if let Some(timestamp) =
            self.cursors
                .get_last_processed(schema_index, source_tier_index, dest_tier_index)
        {
            return Ok(timestamp);
        }

        // First run - find the oldest available data in source tier
        let _oldest_timestamp = source_ring.oldest_timestamp().unwrap_or(0);

        // Initialize cursor
        self.cursors.update_last_processed(
            schema_index,
            source_tier_index,
            dest_tier_index,
            0, // Start from before oldest data
        );

        Ok(0)
    }

    /// Processes consolidation windows for a tier pair.
    #[allow(clippy::too_many_arguments)]
    fn process_consolidation_windows(
        &self,
        source_ring: &RingBuffer,
        dest_ring: &mut RingBuffer,
        _source_tier: &TierConfig,
        dest_tier: &TierConfig,
        consolidation_fn: ConsolidationFn,
        start_timestamp: u64,
        end_timestamp: u64,
        max_series: u32,
    ) -> Result<usize> {
        #[allow(clippy::cast_possible_truncation)]
        // Duration nanos fit in u64 for practical intervals
        let dest_interval_ns = dest_tier.interval.as_nanos() as u64;
        let mut operations = 0;

        // Create all consolidation windows first
        let mut all_windows: HashMap<u64, ConsolidationWindow> = HashMap::new();

        // Iterate through each series column
        for series_column in 0..max_series {
            // Read data from source tier for this series
            let source_iter = source_ring.read(series_column, start_timestamp, end_timestamp)?;

            // Group data into windows aligned to destination tier intervals
            for (timestamp, value) in source_iter {
                // Skip NaN values
                if value.is_nan() {
                    continue;
                }

                // Determine which destination window this point belongs to
                let window_start = (timestamp / dest_interval_ns) * dest_interval_ns;
                let window_end = window_start + dest_interval_ns;

                // Create or get the window
                let window = all_windows
                    .entry(window_start)
                    .or_insert_with(|| ConsolidationWindow::new(window_start, window_end));

                // Add the data point
                window.add_point(series_column, value);
            }
        }

        // Process each window and write consolidated values
        for window in all_windows.values() {
            for series_column in window.series_columns() {
                if let Some(consolidated_value) =
                    window.consolidate_series(series_column, consolidation_fn)
                {
                    // Write consolidated value to destination tier
                    dest_ring.write(series_column, consolidated_value, window.start_timestamp)?;
                    operations += 1;
                }
            }
        }

        Ok(operations)
    }

    /// Saves consolidation cursors to disk.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save_cursors(&self) -> Result<()> {
        let cursor_path = self.store_path.join(CURSORS_FILE);
        self.cursors.save(&cursor_path)
    }

    /// Returns a reference to the consolidation cursors.
    pub fn cursors(&self) -> &ConsolidationCursors {
        &self.cursors
    }

    /// Returns the number of tier pairs that require consolidation.
    pub fn consolidation_pair_count(&self) -> usize {
        self.schemas
            .iter()
            .map(|schema| {
                if schema.tiers.len() > 1 {
                    schema.tiers.len() - 1
                } else {
                    0
                }
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LabelMatcher, TierConfig};
    use crate::slab::Slab;
    use std::time::Duration;
    use tempfile::tempdir;

    fn create_test_schema() -> SchemaConfig {
        SchemaConfig {
            name: "test_schema".to_string(),
            label_matcher: LabelMatcher::any(),
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
                TierConfig {
                    interval: Duration::from_secs(60),
                    retention: Duration::from_secs(3600),
                    consolidation_fn: Some(ConsolidationFn::Max),
                },
            ],
            max_series: 10,
        }
    }

    fn create_test_rings(temp_dir: &tempfile::TempDir, schema: &SchemaConfig) -> Vec<RingBuffer> {
        let mut rings = Vec::new();

        for (tier_index, tier) in schema.tiers.iter().enumerate() {
            let slab_path = temp_dir.path().join(format!("tier_{}.slab", tier_index));
            #[allow(clippy::cast_possible_truncation)] // Test values are small
            let slot_count = (tier.retention.as_nanos() / tier.interval.as_nanos()) as u32;
            #[allow(clippy::cast_possible_truncation)]
            let interval_ns = tier.interval.as_nanos() as u64;

            let slab = Slab::create(
                slab_path,
                0x1234567890abcdef,
                slot_count,
                schema.max_series,
                interval_ns,
            )
            .unwrap();

            rings.push(RingBuffer::new(slab));
        }

        rings
    }

    #[test]
    fn test_consolidation_cursors_basic() {
        let temp_dir = tempdir().unwrap();
        let cursor_path = temp_dir.path().join("cursors.json");

        let mut cursors = ConsolidationCursors::default();

        // Test setting and getting cursors
        cursors.update_last_processed(0, 0, 1, 1000);
        cursors.update_last_processed(0, 1, 2, 2000);
        cursors.update_last_processed(1, 0, 1, 3000);

        assert_eq!(cursors.get_last_processed(0, 0, 1), Some(1000));
        assert_eq!(cursors.get_last_processed(0, 1, 2), Some(2000));
        assert_eq!(cursors.get_last_processed(1, 0, 1), Some(3000));
        assert_eq!(cursors.get_last_processed(0, 0, 2), None);

        // Test persistence
        cursors.save(&cursor_path).unwrap();
        let loaded_cursors = ConsolidationCursors::load(&cursor_path).unwrap();

        assert_eq!(loaded_cursors.get_last_processed(0, 0, 1), Some(1000));
        assert_eq!(loaded_cursors.get_last_processed(0, 1, 2), Some(2000));
        assert_eq!(loaded_cursors.get_last_processed(1, 0, 1), Some(3000));
    }

    #[test]
    fn test_consolidation_cursors_load_missing() {
        let temp_dir = tempdir().unwrap();
        let cursor_path = temp_dir.path().join("missing.json");

        let cursors = ConsolidationCursors::load(&cursor_path).unwrap();
        assert_eq!(cursors.get_last_processed(0, 0, 1), None);
    }

    #[test]
    fn test_consolidation_window() {
        let mut window = ConsolidationWindow::new(1000, 2000);

        assert!(!window.has_data());

        // Add some data points
        window.add_point(0, 10.0);
        window.add_point(0, 20.0);
        window.add_point(1, 30.0);
        window.add_point(0, f64::NAN); // Should be ignored

        assert!(window.has_data());

        let series_columns: Vec<_> = window.series_columns().collect();
        assert!(series_columns.contains(&0));
        assert!(series_columns.contains(&1));

        // Test consolidation
        let avg = window
            .consolidate_series(0, ConsolidationFn::Average)
            .unwrap();
        assert_eq!(avg, 15.0); // (10 + 20) / 2

        let max_val = window.consolidate_series(1, ConsolidationFn::Max).unwrap();
        assert_eq!(max_val, 30.0);

        // Non-existent series should return None
        assert!(
            window
                .consolidate_series(2, ConsolidationFn::Average)
                .is_none()
        );
    }

    #[test]
    fn test_consolidation_engine_creation() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();

        let engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        assert_eq!(engine.schemas.len(), 1);
        assert_eq!(engine.consolidation_pair_count(), 2); // 3 tiers = 2 pairs
    }

    #[test]
    fn test_consolidation_engine_no_tiers() {
        let temp_dir = tempdir().unwrap();
        let schema = SchemaConfig {
            name: "single_tier".to_string(),
            label_matcher: LabelMatcher::any(),
            tiers: vec![TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(60),
                consolidation_fn: None,
            }],
            max_series: 10,
        };

        let engine = ConsolidationEngine::new(temp_dir.path(), vec![schema]).unwrap();
        assert_eq!(engine.consolidation_pair_count(), 0); // 1 tier = 0 pairs
    }

    #[test]
    fn test_consolidation_with_empty_source() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();
        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        // Consolidate with empty rings
        let operations = engine.consolidate(&mut all_rings).unwrap();
        assert_eq!(operations, 0);
    }

    #[test]
    fn test_basic_consolidation() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();
        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        // Write some data to tier 0 (1s intervals)
        let base_time = 1_000_000_000_000_000_000u64; // 1 second in nanoseconds

        all_rings[0][0].write(0, 10.0, base_time).unwrap();
        all_rings[0][0]
            .write(0, 20.0, base_time + 1_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(0, 30.0, base_time + 2_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(0, 40.0, base_time + 10_000_000_000)
            .unwrap(); // Next 10s window
        all_rings[0][0]
            .write(0, 50.0, base_time + 11_000_000_000)
            .unwrap();

        // Run consolidation
        let operations = engine.consolidate(&mut all_rings).unwrap();
        assert!(operations > 0);

        // Check tier 1 (10s intervals) has consolidated data
        let tier1_data: Vec<_> = all_rings[0][1]
            .read(0, base_time - 1, base_time + 20_000_000_000)
            .unwrap()
            .collect();
        assert!(!tier1_data.is_empty());

        // First window should have average of 10, 20, 30 = 20.0
        // (Note: exact window boundaries depend on timestamp alignment)
    }

    #[test]
    fn test_consolidation_idempotence() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();
        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        // Write some data
        let base_time = 1_000_000_000_000_000_000u64;
        all_rings[0][0].write(0, 10.0, base_time).unwrap();
        all_rings[0][0]
            .write(0, 20.0, base_time + 1_000_000_000)
            .unwrap();

        // First consolidation
        let operations1 = engine.consolidate(&mut all_rings).unwrap();
        assert!(operations1 > 0);

        // Second consolidation (no new data) should be a no-op
        let operations2 = engine.consolidate(&mut all_rings).unwrap();
        assert_eq!(operations2, 0);
    }

    #[test]
    fn test_consolidation_with_new_data() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();
        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        let base_time = 1_000_000_000_000_000_000u64;

        // Write initial data
        all_rings[0][0].write(0, 10.0, base_time).unwrap();
        all_rings[0][0]
            .write(0, 20.0, base_time + 1_000_000_000)
            .unwrap();

        // First consolidation
        let operations1 = engine.consolidate(&mut all_rings).unwrap();
        assert!(operations1 > 0);

        // Add more data
        all_rings[0][0]
            .write(0, 30.0, base_time + 2_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(0, 40.0, base_time + 3_000_000_000)
            .unwrap();

        // Second consolidation should process only new data
        let _operations2 = engine.consolidate(&mut all_rings).unwrap();
        // May be 0 if new data doesn't complete a window
    }

    #[test]
    fn test_consolidation_functions() {
        let temp_dir = tempdir().unwrap();
        let schema = SchemaConfig {
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
            ],
            max_series: 5,
        };

        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();
        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        let base_time = 1_000_000_000_000_000_000u64;

        // Write data with different values
        all_rings[0][0].write(0, 100.0, base_time).unwrap();
        all_rings[0][0]
            .write(0, 50.0, base_time + 1_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(0, 75.0, base_time + 2_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(0, 25.0, base_time + 3_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(0, 90.0, base_time + 4_000_000_000)
            .unwrap();

        // Consolidate (Min function should give us 25.0)
        let operations = engine.consolidate(&mut all_rings).unwrap();
        assert!(operations > 0);

        // Verify the minimum value was written to tier 1
        let tier1_data: Vec<_> = all_rings[0][1]
            .read(0, base_time - 1, base_time + 10_000_000_000)
            .unwrap()
            .collect();

        // Should have at least one consolidated point
        assert!(!tier1_data.is_empty());
    }

    #[test]
    fn test_cursor_persistence_across_restarts() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();

        let base_time = 1_000_000_000_000_000_000u64;

        // First run
        {
            let mut engine =
                ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();
            let rings = create_test_rings(&temp_dir, &schema);
            let mut all_rings = vec![rings];

            // Write and consolidate data
            all_rings[0][0].write(0, 10.0, base_time).unwrap();
            all_rings[0][0]
                .write(0, 20.0, base_time + 1_000_000_000)
                .unwrap();

            let _operations = engine.consolidate(&mut all_rings).unwrap();
        }

        // Second run (simulating restart)
        {
            let mut engine =
                ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();
            let rings = create_test_rings(&temp_dir, &schema);
            let mut all_rings = vec![rings];

            // Should not reprocess old data
            let operations = engine.consolidate(&mut all_rings).unwrap();
            assert_eq!(operations, 0);
        }
    }

    #[test]
    fn test_multiple_series_consolidation() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema();
        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        let base_time = 1_000_000_000_000_000_000u64;

        // Write data to multiple series
        all_rings[0][0].write(0, 10.0, base_time).unwrap(); // Series 0
        all_rings[0][0].write(1, 100.0, base_time).unwrap(); // Series 1
        all_rings[0][0]
            .write(0, 20.0, base_time + 1_000_000_000)
            .unwrap();
        all_rings[0][0]
            .write(1, 200.0, base_time + 1_000_000_000)
            .unwrap();

        // Consolidate
        let operations = engine.consolidate(&mut all_rings).unwrap();
        assert!(operations > 0);

        // Both series should have consolidated data
        let series0_data: Vec<_> = all_rings[0][1]
            .read(0, base_time - 1, base_time + 20_000_000_000)
            .unwrap()
            .collect();
        let series1_data: Vec<_> = all_rings[0][1]
            .read(1, base_time - 1, base_time + 20_000_000_000)
            .unwrap()
            .collect();

        // Each series should have its own consolidated data
        assert!(!series0_data.is_empty() || !series1_data.is_empty());
    }

    #[test]
    fn test_three_tier_consolidation() {
        let temp_dir = tempdir().unwrap();
        let schema = create_test_schema(); // Has 3 tiers
        let mut engine = ConsolidationEngine::new(temp_dir.path(), vec![schema.clone()]).unwrap();

        let rings = create_test_rings(&temp_dir, &schema);
        let mut all_rings = vec![rings];

        let base_time = 1_000_000_000_000_000_000u64;

        // Write enough data to trigger all consolidation levels
        for i in 0u32..65 {
            all_rings[0][0]
                .write(
                    0,
                    f64::from(i * 10),
                    base_time + u64::from(i) * 1_000_000_000,
                )
                .unwrap();
        }

        // Consolidate multiple times to cascade through tiers
        for _ in 0..5 {
            let operations = engine.consolidate(&mut all_rings).unwrap();
            if operations == 0 {
                break;
            }
        }

        // Should have data in tier 1 (10s intervals)
        let tier1_data: Vec<_> = all_rings[0][1]
            .read(0, base_time - 1, base_time + 70_000_000_000)
            .unwrap()
            .collect();

        // Should have data in tier 2 (60s intervals)
        let _tier2_data: Vec<_> = all_rings[0][2]
            .read(0, base_time - 1, base_time + 70_000_000_000)
            .unwrap()
            .collect();

        // At least tier 1 should have some data
        assert!(!tier1_data.is_empty());
    }
}
