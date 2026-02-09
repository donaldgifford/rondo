//! Export module for draining data from the store.
//!
//! Provides cursor-based data export designed for periodic push to remote
//! time-series databases. The drain operation reads all new data since
//! the last export and advances the cursor atomically.
//!
//! # Design
//!
//! The export system uses persistent cursors to track what data has already
//! been exported. Each cursor is identified by a name (e.g., "prometheus")
//! and tracks the last exported timestamp per series per tier.
//!
//! # Example
//!
//! ```rust,no_run
//! use rondo::store::Store;
//! use rondo::export::ExportCursor;
//! # use rondo::schema::{SchemaConfig, LabelMatcher, TierConfig};
//! # use std::time::Duration;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let schemas = vec![SchemaConfig {
//! #     name: "test".to_string(),
//! #     label_matcher: LabelMatcher::any(),
//! #     tiers: vec![TierConfig::new(Duration::from_secs(1), Duration::from_secs(60), None)?],
//! #     max_series: 10,
//! # }];
//! # let store = Store::open("/tmp/export_example", schemas)?;
//! let cursor = ExportCursor::load_or_new("/tmp/export_example/cursor_prometheus.json")?;
//! // Use store.drain() to get new data since cursor
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ExportError, Result};
use crate::ring::RingBuffer;
use crate::series::SeriesHandle;

/// A data point exported from the store.
#[derive(Debug, Clone, Copy)]
pub struct ExportPoint {
    /// Timestamp in nanoseconds since epoch.
    pub timestamp: u64,
    /// The data value.
    pub value: f64,
    /// Schema index this point belongs to.
    pub schema_index: usize,
    /// Tier index this point was read from.
    pub tier_index: usize,
    /// Series column within the tier.
    pub series_column: u32,
}

/// A batch of exported data points for a single series.
#[derive(Debug)]
pub struct SeriesExport {
    /// The series handle identifying this series.
    pub handle: SeriesHandle,
    /// The exported data points, ordered by timestamp.
    pub points: Vec<(u64, f64)>,
}

/// Persistent cursor tracking export progress.
///
/// Stores the last exported timestamp per (schema, tier, series_column) triple.
/// This allows incremental exports — each drain call only returns data newer
/// than the last export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCursor {
    /// File path for persistence.
    #[serde(skip)]
    path: PathBuf,
    /// Map of "schema:tier:column" -> last exported timestamp.
    cursors: HashMap<String, u64>,
}

impl ExportCursor {
    /// Creates a new empty cursor.
    pub fn new() -> Self {
        Self {
            path: PathBuf::new(),
            cursors: HashMap::new(),
        }
    }

    /// Loads a cursor from a file, or creates a new one if the file doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load_or_new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if path.exists() {
            let data = std::fs::read_to_string(&path).map_err(|e| ExportError::CursorLoad {
                path: path.clone(),
                source: e,
            })?;
            let mut cursor: Self =
                serde_json::from_str(&data).map_err(|e| ExportError::CursorParse {
                    path: path.clone(),
                    source: e,
                })?;
            cursor.path = path;
            Ok(cursor)
        } else {
            Ok(Self {
                path,
                cursors: HashMap::new(),
            })
        }
    }

    /// Saves the cursor to its file path.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| ExportError::CursorSerialize { source: e })?;
        std::fs::write(&self.path, data).map_err(|e| ExportError::CursorSave {
            path: self.path.clone(),
            source: e,
        })?;
        Ok(())
    }

    /// Gets the last exported timestamp for a (schema, tier, column) triple.
    fn get(&self, schema_index: usize, tier_index: usize, series_column: u32) -> Option<u64> {
        let key = Self::make_key(schema_index, tier_index, series_column);
        self.cursors.get(&key).copied()
    }

    /// Updates the last exported timestamp for a (schema, tier, column) triple.
    fn update(
        &mut self,
        schema_index: usize,
        tier_index: usize,
        series_column: u32,
        timestamp: u64,
    ) {
        let key = Self::make_key(schema_index, tier_index, series_column);
        self.cursors.insert(key, timestamp);
    }

    fn make_key(schema_index: usize, tier_index: usize, series_column: u32) -> String {
        format!("{schema_index}:{tier_index}:{series_column}")
    }
}

impl Default for ExportCursor {
    fn default() -> Self {
        Self::new()
    }
}

/// Drains new data from the specified tier of a ring buffer for a given series.
///
/// Returns all data points newer than the cursor position for this series.
/// The cursor is advanced to the newest timestamp found.
pub(crate) fn drain_series(
    ring: &RingBuffer,
    schema_index: usize,
    tier_index: usize,
    series_column: u32,
    cursor: &mut ExportCursor,
) -> Result<Vec<(u64, f64)>> {
    let last_exported = cursor.get(schema_index, tier_index, series_column);

    // Determine read range
    let oldest = ring.oldest_timestamp();
    let newest = ring.newest_timestamp();

    let (Some(_oldest_ts), Some(newest_ts)) = (oldest, newest) else {
        // No data in ring
        return Ok(Vec::new());
    };

    // Start from after the last exported timestamp, or from oldest available
    let start = match last_exported {
        Some(ts) => ts + 1,
        None => _oldest_ts,
    };

    if start > newest_ts {
        // No new data
        return Ok(Vec::new());
    }

    // Read data from ring buffer (end is exclusive, so add 1)
    let iter = ring.read(series_column, start, newest_ts + 1)?;
    let points: Vec<(u64, f64)> = iter.collect();

    // Update cursor to newest timestamp we read
    if let Some(&(last_ts, _)) = points.last() {
        cursor.update(schema_index, tier_index, series_column, last_ts);
    }

    Ok(points)
}

/// Drains all new data from a store for all registered series at a specific tier.
///
/// Returns a vector of `SeriesExport` containing new data points for each series.
/// The cursor is updated for each series that had new data.
pub(crate) fn drain_tier(
    rings: &[Vec<RingBuffer>],
    schema_index: usize,
    tier_index: usize,
    handles: &[SeriesHandle],
    cursor: &mut ExportCursor,
) -> Result<Vec<SeriesExport>> {
    let mut exports = Vec::new();

    let ring = &rings[schema_index][tier_index];

    for &handle in handles {
        if handle.schema_index != schema_index {
            continue;
        }

        let points = drain_series(ring, schema_index, tier_index, handle.column, cursor)?;

        if !points.is_empty() {
            exports.push(SeriesExport { handle, points });
        }
    }

    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slab::Slab;
    use tempfile::tempdir;

    fn create_test_ring(
        temp_dir: &std::path::Path,
        slot_count: u32,
        interval_ns: u64,
    ) -> RingBuffer {
        let slab_path = temp_dir.join("test.slab");
        let slab =
            Slab::create(slab_path, 0x1234567890abcdef, slot_count, 10, interval_ns).unwrap();
        RingBuffer::new(slab)
    }

    #[test]
    fn test_export_cursor_new() {
        let cursor = ExportCursor::new();
        assert!(cursor.cursors.is_empty());
    }

    #[test]
    fn test_export_cursor_get_set() {
        let mut cursor = ExportCursor::new();

        assert!(cursor.get(0, 0, 0).is_none());

        cursor.update(0, 0, 0, 1000);
        assert_eq!(cursor.get(0, 0, 0), Some(1000));

        cursor.update(0, 0, 0, 2000);
        assert_eq!(cursor.get(0, 0, 0), Some(2000));

        // Different series column
        assert!(cursor.get(0, 0, 1).is_none());
    }

    #[test]
    fn test_export_cursor_persistence() {
        let temp_dir = tempdir().unwrap();
        let cursor_path = temp_dir.path().join("cursor.json");

        // Create and save
        {
            let mut cursor = ExportCursor::load_or_new(&cursor_path).unwrap();
            cursor.update(0, 0, 0, 1000);
            cursor.update(0, 1, 0, 2000);
            cursor.save().unwrap();
        }

        // Load and verify
        {
            let cursor = ExportCursor::load_or_new(&cursor_path).unwrap();
            assert_eq!(cursor.get(0, 0, 0), Some(1000));
            assert_eq!(cursor.get(0, 1, 0), Some(2000));
        }
    }

    #[test]
    fn test_drain_empty_ring() {
        let temp_dir = tempdir().unwrap();
        let ring = create_test_ring(temp_dir.path(), 60, 1_000_000_000);
        let mut cursor = ExportCursor::new();

        let points = drain_series(&ring, 0, 0, 0, &mut cursor).unwrap();
        assert!(points.is_empty());
    }

    #[test]
    fn test_drain_all_data() {
        let temp_dir = tempdir().unwrap();
        let mut ring = create_test_ring(temp_dir.path(), 60, 1_000_000_000);
        let mut cursor = ExportCursor::new();

        let base_time = 1_000_000_000_000_000_000u64;

        // Write 5 data points
        for i in 0u32..5 {
            ring.write(
                0,
                f64::from(i * 10),
                base_time + u64::from(i) * 1_000_000_000,
            )
            .unwrap();
        }

        let points = drain_series(&ring, 0, 0, 0, &mut cursor).unwrap();
        assert_eq!(points.len(), 5);
        assert_eq!(points[0].1, 0.0);
        assert_eq!(points[4].1, 40.0);

        // Cursor should be updated
        assert_eq!(cursor.get(0, 0, 0), Some(base_time + 4_000_000_000));
    }

    #[test]
    fn test_drain_incremental() {
        let temp_dir = tempdir().unwrap();
        let mut ring = create_test_ring(temp_dir.path(), 60, 1_000_000_000);
        let mut cursor = ExportCursor::new();

        let base_time = 1_000_000_000_000_000_000u64;

        // Write first batch
        for i in 0u32..5 {
            ring.write(
                0,
                f64::from(i * 10),
                base_time + u64::from(i) * 1_000_000_000,
            )
            .unwrap();
        }

        let points1 = drain_series(&ring, 0, 0, 0, &mut cursor).unwrap();
        assert_eq!(points1.len(), 5);

        // Write second batch
        for i in 5u32..10 {
            ring.write(
                0,
                f64::from(i * 10),
                base_time + u64::from(i) * 1_000_000_000,
            )
            .unwrap();
        }

        // Second drain should only return new data
        let points2 = drain_series(&ring, 0, 0, 0, &mut cursor).unwrap();
        assert_eq!(points2.len(), 5);
        assert_eq!(points2[0].1, 50.0);
        assert_eq!(points2[4].1, 90.0);
    }

    #[test]
    fn test_drain_no_new_data() {
        let temp_dir = tempdir().unwrap();
        let mut ring = create_test_ring(temp_dir.path(), 60, 1_000_000_000);
        let mut cursor = ExportCursor::new();

        let base_time = 1_000_000_000_000_000_000u64;

        for i in 0u32..5 {
            ring.write(
                0,
                f64::from(i * 10),
                base_time + u64::from(i) * 1_000_000_000,
            )
            .unwrap();
        }

        // First drain
        let _ = drain_series(&ring, 0, 0, 0, &mut cursor).unwrap();

        // Second drain with no new data
        let points = drain_series(&ring, 0, 0, 0, &mut cursor).unwrap();
        assert!(points.is_empty());
    }
}
