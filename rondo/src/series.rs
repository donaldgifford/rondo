//! Series registration and management for Rondo time-series storage.
//!
//! This module provides the series registration system that maps time series
//! identifiers (name + labels) to storage handles. It manages the allocation
//! of column space in slabs and maintains the mapping between series metadata
//! and their storage locations.
//!
//! # Overview
//!
//! The series registration system consists of:
//!
//! - [`SeriesHandle`] - Opaque handle containing pre-computed storage location
//! - [`SeriesRegistry`] - Main registration manager across all schemas
//! - [`SeriesInfo`] - Metadata about registered series
//!
//! # Registration Flow
//!
//! 1. Client calls `register(name, labels)`
//! 2. Registry finds matching schema using label matchers
//! 3. If series already exists, returns existing handle
//! 4. Otherwise allocates next available column and creates handle
//! 5. Handle contains pre-computed column offset for hot-path writes
//!
//! # Example
//!
//! ```rust,no_run
//! use rondo::series::SeriesRegistry;
//! use rondo::schema::{SchemaConfig, LabelMatcher};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let schemas = vec![
//!     SchemaConfig {
//!         name: "cpu_metrics".to_string(),
//!         label_matcher: LabelMatcher::new([("type", "cpu")]),
//!         tiers: vec![/* tier configs */],
//!         max_series: 1000,
//!     }
//! ];
//!
//! let mut registry = SeriesRegistry::new(schemas);
//!
//! // Register a new series
//! let handle = registry.register(
//!     "cpu.usage",
//!     &[("type".to_string(), "cpu".to_string()), ("host".to_string(), "web1".to_string())]
//! )?;
//!
//! // Use handle for writes (hot path)
//! println!("Series column: {}", handle.column);
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SeriesError};
use crate::schema::SchemaConfig;
use crate::slab::Slab;

/// Opaque handle for a registered time series.
///
/// This handle contains pre-computed information needed for efficient writes
/// to the storage slabs. The handle is `Copy` and designed to be passed around
/// cheaply on the hot path.
///
/// # Fields
///
/// - `schema_index` - Which schema this series belongs to
/// - `series_id` - Unique ID within the schema
/// - `column` - Pre-computed column offset in the slab
///
/// The handle is opaque to prevent direct manipulation while providing
/// efficient access to the storage layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeriesHandle {
    /// Index of the schema this series belongs to.
    pub schema_index: usize,
    /// Unique ID within the schema.
    pub series_id: u32,
    /// Pre-computed column offset in the slab for direct writes.
    pub column: u32,
}

impl SeriesHandle {
    /// Creates a new series handle.
    ///
    /// # Arguments
    ///
    /// * `schema_index` - Index of the schema this series belongs to
    /// * `series_id` - Unique ID within the schema
    /// * `column` - Column offset in the slab
    pub fn new(schema_index: usize, series_id: u32, column: u32) -> Self {
        Self {
            schema_index,
            series_id,
            column,
        }
    }
}

/// Information about a registered series.
///
/// Contains the metadata that identifies a time series, including its name
/// and complete set of labels. This is used for lookups and persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesInfo {
    /// The series name.
    pub name: String,
    /// The complete set of labels as key-value pairs.
    pub labels: Vec<(String, String)>,
    /// The schema index this series belongs to.
    pub schema_index: usize,
    /// The unique series ID within the schema.
    pub series_id: u32,
    /// The assigned column in the slab.
    pub column: u32,
}

impl SeriesInfo {
    /// Creates a new series info.
    pub fn new(
        name: String,
        labels: Vec<(String, String)>,
        schema_index: usize,
        series_id: u32,
        column: u32,
    ) -> Self {
        Self {
            name,
            labels,
            schema_index,
            series_id,
            column,
        }
    }

    /// Returns a handle for this series.
    pub fn handle(&self) -> SeriesHandle {
        SeriesHandle::new(self.schema_index, self.series_id, self.column)
    }
}

/// Registry for managing series registration across all schemas.
///
/// The registry maintains mappings between series identifiers (name + labels)
/// and their storage handles. It enforces schema constraints and manages
/// column allocation within each schema.
///
/// # Thread Safety
///
/// The registry is designed for single-threaded access patterns. External
/// synchronization must be provided if used across multiple threads.
pub struct SeriesRegistry {
    /// Schema configurations.
    schemas: Vec<SchemaConfig>,
    /// Map from (name, labels) to series info.
    series_map: HashMap<SeriesKey, SeriesInfo>,
    /// Next available series ID for each schema.
    next_series_id: Vec<u32>,
    /// Next available column for each schema.
    next_column: Vec<u32>,
}

/// Key type for looking up series in the registry.
///
/// Combines series name and labels into a hashable key for efficient lookups.
/// Labels are sorted to ensure consistent hashing regardless of input order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SeriesKey {
    name: String,
    labels: Vec<(String, String)>,
}

impl SeriesKey {
    /// Creates a new series key with sorted labels.
    fn new(name: String, labels: &[(String, String)]) -> Self {
        let mut sorted_labels = labels.to_vec();
        sorted_labels.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            name,
            labels: sorted_labels,
        }
    }
}

impl SeriesRegistry {
    /// Creates a new series registry with the given schemas.
    ///
    /// # Arguments
    ///
    /// * `schemas` - Schema configurations that define storage tiers and label routing
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rondo::series::SeriesRegistry;
    /// use rondo::schema::{SchemaConfig, LabelMatcher, TierConfig, ConsolidationFn};
    /// use std::time::Duration;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let schemas = vec![
    ///     SchemaConfig {
    ///         name: "cpu_metrics".to_string(),
    ///         label_matcher: LabelMatcher::new([("type", "cpu")]),
    ///         tiers: vec![
    ///             TierConfig::new(
    ///                 Duration::from_secs(1),
    ///                 Duration::from_secs(3600),
    ///                 None,
    ///             )?,
    ///         ],
    ///         max_series: 100,
    ///     },
    /// ];
    ///
    /// let registry = SeriesRegistry::new(schemas);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(schemas: Vec<SchemaConfig>) -> Self {
        let schema_count = schemas.len();
        Self {
            schemas,
            series_map: HashMap::new(),
            next_series_id: vec![0; schema_count],
            next_column: vec![0; schema_count],
        }
    }

    /// Registers a time series and returns a handle for writes.
    ///
    /// This is the main entry point for series registration. The method:
    ///
    /// 1. Validates the labels
    /// 2. Finds a matching schema based on label matchers
    /// 3. Returns existing handle if series already registered
    /// 4. Allocates a new column and creates a handle if not registered
    ///
    /// # Arguments
    ///
    /// * `name` - The series name (must be non-empty)
    /// * `labels` - Label key-value pairs for the series
    ///
    /// # Returns
    ///
    /// A [`SeriesHandle`] that can be used for efficient writes.
    ///
    /// # Errors
    ///
    /// - [`SeriesError::InvalidLabel`] if any label is invalid
    /// - [`SeriesError::NoMatchingSchema`] if no schema matches the labels
    /// - [`SeriesError::MaxSeriesExceeded`] if schema capacity is exceeded
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::series::SeriesRegistry;
    /// # use rondo::schema::{SchemaConfig, LabelMatcher};
    /// # let mut registry = SeriesRegistry::new(vec![]);
    /// let handle = registry.register(
    ///     "cpu.usage",
    ///     &[
    ///         ("type".to_string(), "cpu".to_string()),
    ///         ("host".to_string(), "web1".to_string()),
    ///     ]
    /// )?;
    ///
    /// // Handle can now be used for writes
    /// println!("Series assigned to column {}", handle.column);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn register(
        &mut self,
        name: &str,
        labels: &[(String, String)],
    ) -> Result<SeriesHandle> {
        // Validate inputs
        self.validate_name(name)?;
        self.validate_labels(labels)?;

        // Check if series already exists
        let key = SeriesKey::new(name.to_string(), labels);
        if let Some(info) = self.series_map.get(&key) {
            return Ok(info.handle());
        }

        // Find matching schema
        let schema_index = self.find_matching_schema(labels)?;

        // Check capacity
        if self.next_series_id[schema_index] >= self.schemas[schema_index].max_series {
            return Err(SeriesError::MaxSeriesExceeded {
                max_series: self.schemas[schema_index].max_series,
            }
            .into());
        }

        // Allocate series ID and column
        let series_id = self.next_series_id[schema_index];
        let column = self.next_column[schema_index];

        // Create series info
        let info = SeriesInfo::new(
            name.to_string(),
            labels.to_vec(),
            schema_index,
            series_id,
            column,
        );

        // Update registry state
        self.series_map.insert(key, info.clone());
        self.next_series_id[schema_index] += 1;
        self.next_column[schema_index] += 1;

        Ok(info.handle())
    }

    /// Looks up a series handle by name and labels.
    ///
    /// Returns the handle if the series is registered, or `None` if not found.
    ///
    /// # Arguments
    ///
    /// * `name` - The series name
    /// * `labels` - Label key-value pairs
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::series::SeriesRegistry;
    /// # let registry = SeriesRegistry::new(vec![]);
    /// let handle = registry.get_handle(
    ///     "cpu.usage",
    ///     &[("type".to_string(), "cpu".to_string())]
    /// );
    ///
    /// if let Some(h) = handle {
    ///     println!("Found series at column {}", h.column);
    /// }
    /// ```
    pub fn get_handle(&self, name: &str, labels: &[(String, String)]) -> Option<SeriesHandle> {
        let key = SeriesKey::new(name.to_string(), labels);
        self.series_map.get(&key).map(|info| info.handle())
    }

    /// Returns information about a registered series.
    ///
    /// # Arguments
    ///
    /// * `handle` - The series handle to look up
    ///
    /// # Returns
    ///
    /// Series information if the handle is valid, `None` otherwise.
    pub fn series_info(&self, handle: &SeriesHandle) -> Option<&SeriesInfo> {
        // Find series info by searching for matching handle
        self.series_map.values().find(|info| info.handle() == *handle)
    }

    /// Returns the number of registered series for a schema.
    ///
    /// # Arguments
    ///
    /// * `schema_index` - The schema index
    ///
    /// # Returns
    ///
    /// The number of series registered for this schema, or 0 if the index is invalid.
    pub fn series_count(&self, schema_index: usize) -> u32 {
        if schema_index < self.next_series_id.len() {
            self.next_series_id[schema_index]
        } else {
            0
        }
    }

    /// Returns the total number of registered series across all schemas.
    pub fn total_series_count(&self) -> usize {
        self.series_map.len()
    }

    /// Returns references to all schema configurations.
    pub fn schemas(&self) -> &[SchemaConfig] {
        &self.schemas
    }

    /// Updates the slab series directory with current registrations.
    ///
    /// This method synchronizes the registry state with the slab's series
    /// directory, ensuring that handles can be used for direct writes.
    ///
    /// # Arguments
    ///
    /// * `slabs` - Mutable references to slabs for each schema
    ///
    /// # Errors
    ///
    /// Returns an error if slab updates fail.
    pub fn sync_to_slabs(&self, slabs: &mut [&mut Slab]) -> Result<()> {
        for (schema_index, slab) in slabs.iter_mut().enumerate() {
            // Update series count in slab header
            let count = self.series_count(schema_index);
            slab.set_series_count(count);

            // Update series directory entries
            for info in self.series_map.values() {
                if info.schema_index == schema_index {
                    slab.set_series_column(info.series_id, info.column);
                }
            }
        }
        Ok(())
    }

    /// Saves the series index to a file.
    ///
    /// The series index is serialized as JSON for simplicity in the MVP.
    /// This includes all series metadata needed to reconstruct handles
    /// when reopening the store.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where to save the series index
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let index = SeriesIndex {
            series: self.series_map.values().cloned().collect(),
            next_series_id: self.next_series_id.clone(),
            next_column: self.next_column.clone(),
        };

        let json = serde_json::to_string_pretty(&index)
            .map_err(crate::error::StoreError::MetadataSerialize)?;

        std::fs::write(path, json).map_err(|e| {
            crate::error::StoreError::DirectoryAccess {
                path: "series_index.bin".to_string(),
                source: e,
            }
        })?;

        Ok(())
    }

    /// Loads a series index from a file.
    ///
    /// Reconstructs the registry state from a previously saved index file.
    /// This is called when opening an existing store to restore all
    /// registered series and their handles.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the series index file
    /// * `schemas` - Schema configurations (must match the saved schemas)
    ///
    /// # Returns
    ///
    /// A reconstructed registry with all series registrations restored.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed, or if the
    /// schemas don't match the saved index.
    pub fn load<P: AsRef<Path>>(path: P, schemas: Vec<SchemaConfig>) -> Result<Self> {
        let json = std::fs::read_to_string(path).map_err(|e| {
            crate::error::StoreError::DirectoryAccess {
                path: "series_index.bin".to_string(),
                source: e,
            }
        })?;

        let index: SeriesIndex = serde_json::from_str(&json)
            .map_err(crate::error::StoreError::MetadataSerialize)?;

        // Validate schema count matches
        if schemas.len() != index.next_series_id.len() {
            return Err(crate::error::StoreError::CorruptedMetadata {
                reason: format!(
                    "schema count mismatch: {} schemas configured, {} in index",
                    schemas.len(),
                    index.next_series_id.len()
                ),
            }
            .into());
        }

        // Reconstruct series map
        let mut series_map = HashMap::new();
        for info in index.series {
            let key = SeriesKey::new(info.name.clone(), &info.labels);
            series_map.insert(key, info);
        }

        Ok(Self {
            schemas,
            series_map,
            next_series_id: index.next_series_id,
            next_column: index.next_column,
        })
    }

    /// Validates a series name.
    fn validate_name(&self, name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(SeriesError::InvalidLabel {
                key: "__name__".to_string(),
                value: name.to_string(),
                reason: "name cannot be empty".to_string(),
            }
            .into());
        }
        Ok(())
    }

    /// Validates label key-value pairs.
    fn validate_labels(&self, labels: &[(String, String)]) -> Result<()> {
        for (key, value) in labels {
            // Check for empty keys or values
            if key.is_empty() {
                return Err(SeriesError::InvalidLabel {
                    key: key.clone(),
                    value: value.clone(),
                    reason: "key cannot be empty".to_string(),
                }
                .into());
            }

            if value.is_empty() {
                return Err(SeriesError::InvalidLabel {
                    key: key.clone(),
                    value: value.clone(),
                    reason: "value cannot be empty".to_string(),
                }
                .into());
            }

            // Check for reserved prefix
            if key.starts_with("__") {
                return Err(SeriesError::InvalidLabel {
                    key: key.clone(),
                    value: value.clone(),
                    reason: "keys starting with '__' are reserved for internal use".to_string(),
                }
                .into());
            }
        }
        Ok(())
    }

    /// Finds a schema that matches the given labels.
    fn find_matching_schema(&self, labels: &[(String, String)]) -> Result<usize> {
        for (index, schema) in self.schemas.iter().enumerate() {
            if schema.matches_labels(labels) {
                return Ok(index);
            }
        }

        Err(SeriesError::NoMatchingSchema {
            labels: labels.to_vec(),
        }
        .into())
    }
}

/// Serializable representation of the series index for persistence.
#[derive(Debug, Serialize, Deserialize)]
struct SeriesIndex {
    /// All registered series information.
    series: Vec<SeriesInfo>,
    /// Next available series ID for each schema.
    next_series_id: Vec<u32>,
    /// Next available column for each schema.
    next_column: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LabelMatcher, TierConfig};
    use std::time::Duration;

    fn create_test_schema(name: &str, labels: &[(&str, &str)], max_series: u32) -> SchemaConfig {
        SchemaConfig {
            name: name.to_string(),
            label_matcher: LabelMatcher::new(labels.iter().map(|(k, v)| (*k, *v))),
            tiers: vec![TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(3600),
                consolidation_fn: None,
            }],
            max_series,
        }
    }

    fn test_labels() -> Vec<(String, String)> {
        vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web1".to_string()),
        ]
    }

    #[test]
    fn test_series_handle_creation() {
        let handle = SeriesHandle::new(1, 42, 100);
        assert_eq!(handle.schema_index, 1);
        assert_eq!(handle.series_id, 42);
        assert_eq!(handle.column, 100);
    }

    #[test]
    fn test_series_info() {
        let info = SeriesInfo::new(
            "cpu.usage".to_string(),
            test_labels(),
            0,
            1,
            5,
        );

        assert_eq!(info.name, "cpu.usage");
        assert_eq!(info.labels, test_labels());
        assert_eq!(info.schema_index, 0);
        assert_eq!(info.series_id, 1);
        assert_eq!(info.column, 5);

        let handle = info.handle();
        assert_eq!(handle.schema_index, 0);
        assert_eq!(handle.series_id, 1);
        assert_eq!(handle.column, 5);
    }

    #[test]
    fn test_registry_new() {
        let schemas = vec![
            create_test_schema("cpu", &[("type", "cpu")], 100),
            create_test_schema("mem", &[("type", "memory")], 50),
        ];

        let registry = SeriesRegistry::new(schemas);
        assert_eq!(registry.schemas().len(), 2);
        assert_eq!(registry.total_series_count(), 0);
        assert_eq!(registry.series_count(0), 0);
        assert_eq!(registry.series_count(1), 0);
    }

    #[test]
    fn test_register_new_series() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 100)];
        let mut registry = SeriesRegistry::new(schemas);

        let labels = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];

        let handle = registry.register("cpu.usage", &labels).unwrap();

        assert_eq!(handle.schema_index, 0);
        assert_eq!(handle.series_id, 0);
        assert_eq!(handle.column, 0);

        assert_eq!(registry.series_count(0), 1);
        assert_eq!(registry.total_series_count(), 1);
    }

    #[test]
    fn test_register_existing_series_returns_same_handle() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 100)];
        let mut registry = SeriesRegistry::new(schemas);

        let labels = test_labels();

        let handle1 = registry.register("cpu.usage", &labels).unwrap();
        let handle2 = registry.register("cpu.usage", &labels).unwrap();

        assert_eq!(handle1, handle2);
        assert_eq!(registry.series_count(0), 1); // Should not increase
    }

    #[test]
    fn test_register_different_labels_different_handles() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 100)];
        let mut registry = SeriesRegistry::new(schemas);

        let labels1 = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];
        let labels2 = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web2".to_string()),
        ];

        let handle1 = registry.register("cpu.usage", &labels1).unwrap();
        let handle2 = registry.register("cpu.usage", &labels2).unwrap();

        assert_ne!(handle1, handle2);
        assert_eq!(handle1.series_id, 0);
        assert_eq!(handle2.series_id, 1);
        assert_eq!(handle1.column, 0);
        assert_eq!(handle2.column, 1);
    }

    #[test]
    fn test_register_max_series_exceeded() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 1)]; // Max 1 series
        let mut registry = SeriesRegistry::new(schemas);

        let labels1 = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];
        let labels2 = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web2".to_string()),
        ];

        // First registration should succeed
        registry.register("cpu.usage", &labels1).unwrap();

        // Second should fail
        let result = registry.register("cpu.usage", &labels2);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), crate::error::RondoError::Series(SeriesError::MaxSeriesExceeded { .. })));
    }

    #[test]
    fn test_register_no_matching_schema() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 100)];
        let mut registry = SeriesRegistry::new(schemas);

        let labels = vec![("type".to_string(), "memory".to_string())]; // Doesn't match

        let result = registry.register("mem.usage", &labels);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), crate::error::RondoError::Series(SeriesError::NoMatchingSchema { .. })));
    }

    #[test]
    fn test_get_handle() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 100)];
        let mut registry = SeriesRegistry::new(schemas);

        let labels = test_labels();

        // Before registration
        assert!(registry.get_handle("cpu.usage", &labels).is_none());

        // After registration
        let handle = registry.register("cpu.usage", &labels).unwrap();
        let retrieved = registry.get_handle("cpu.usage", &labels).unwrap();
        assert_eq!(handle, retrieved);
    }

    #[test]
    fn test_series_info_lookup() {
        let schemas = vec![create_test_schema("cpu", &[("type", "cpu")], 100)];
        let mut registry = SeriesRegistry::new(schemas);

        let labels = test_labels();
        let handle = registry.register("cpu.usage", &labels).unwrap();

        let info = registry.series_info(&handle).unwrap();
        assert_eq!(info.name, "cpu.usage");
        assert_eq!(info.labels, labels);
        assert_eq!(info.handle(), handle);
    }

    #[test]
    fn test_label_validation() {
        let schemas = vec![create_test_schema("any", &[], 100)]; // Matches any labels
        let mut registry = SeriesRegistry::new(schemas);

        // Empty name should fail
        let result = registry.register("", &test_labels());
        assert!(result.is_err());

        // Empty key should fail
        let invalid_labels = vec![("".to_string(), "value".to_string())];
        let result = registry.register("test", &invalid_labels);
        assert!(result.is_err());

        // Empty value should fail
        let invalid_labels = vec![("key".to_string(), "".to_string())];
        let result = registry.register("test", &invalid_labels);
        assert!(result.is_err());

        // Reserved prefix should fail
        let invalid_labels = vec![("__reserved".to_string(), "value".to_string())];
        let result = registry.register("test", &invalid_labels);
        assert!(result.is_err());
    }

    #[test]
    fn test_series_key_label_sorting() {
        let key1 = SeriesKey::new(
            "test".to_string(),
            &[
                ("b".to_string(), "2".to_string()),
                ("a".to_string(), "1".to_string()),
            ],
        );
        let key2 = SeriesKey::new(
            "test".to_string(),
            &[
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ],
        );

        // Keys should be equal regardless of label order
        assert_eq!(key1, key2);
        assert_eq!(key1.labels[0].0, "a"); // Should be sorted
        assert_eq!(key1.labels[1].0, "b");
    }

    #[test]
    fn test_save_and_load() {
        let temp_dir = tempfile::tempdir().unwrap();
        let index_path = temp_dir.path().join("series_index.json");

        let schemas = vec![
            create_test_schema("cpu", &[("type", "cpu")], 100),
            create_test_schema("mem", &[("type", "memory")], 50),
        ];

        // Create registry and register some series
        let mut registry = SeriesRegistry::new(schemas.clone());

        let cpu_labels = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];
        let mem_labels = vec![
            ("type".to_string(), "memory".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];

        let cpu_handle = registry.register("cpu.usage", &cpu_labels).unwrap();
        let mem_handle = registry.register("mem.usage", &mem_labels).unwrap();

        // Save the registry
        registry.save(&index_path).unwrap();

        // Load it back
        let loaded_registry = SeriesRegistry::load(&index_path, schemas).unwrap();

        // Verify series are restored
        assert_eq!(loaded_registry.total_series_count(), 2);
        assert_eq!(loaded_registry.series_count(0), 1); // CPU schema
        assert_eq!(loaded_registry.series_count(1), 1); // Memory schema

        // Verify handles work
        let loaded_cpu_handle = loaded_registry.get_handle("cpu.usage", &cpu_labels).unwrap();
        let loaded_mem_handle = loaded_registry.get_handle("mem.usage", &mem_labels).unwrap();

        assert_eq!(cpu_handle, loaded_cpu_handle);
        assert_eq!(mem_handle, loaded_mem_handle);

        // Verify series info
        let cpu_info = loaded_registry.series_info(&cpu_handle).unwrap();
        assert_eq!(cpu_info.name, "cpu.usage");
        assert_eq!(cpu_info.labels, cpu_labels);
    }

    #[test]
    fn test_multiple_schemas() {
        let schemas = vec![
            create_test_schema("cpu", &[("type", "cpu")], 100),
            create_test_schema("mem", &[("type", "memory")], 50),
        ];

        let mut registry = SeriesRegistry::new(schemas);

        let cpu_labels = vec![
            ("type".to_string(), "cpu".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];
        let mem_labels = vec![
            ("type".to_string(), "memory".to_string()),
            ("host".to_string(), "web1".to_string()),
        ];

        let cpu_handle = registry.register("cpu.usage", &cpu_labels).unwrap();
        let mem_handle = registry.register("mem.usage", &mem_labels).unwrap();

        // Should be in different schemas
        assert_eq!(cpu_handle.schema_index, 0);
        assert_eq!(mem_handle.schema_index, 1);

        // Should both start from series_id 0 in their respective schemas
        assert_eq!(cpu_handle.series_id, 0);
        assert_eq!(mem_handle.series_id, 0);

        // Should both start from column 0 in their respective schemas
        assert_eq!(cpu_handle.column, 0);
        assert_eq!(mem_handle.column, 0);
    }
}