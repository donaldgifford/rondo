//! Error types for the Rondo time-series storage engine.

use std::time::Duration;

use thiserror::Error;

/// The main error type for all Rondo operations.
///
/// This enum covers all possible error conditions that can occur during store
/// operations, from initial creation to runtime queries and I/O.
#[derive(Error, Debug)]
pub enum RondoError {
    /// Error opening or creating a store.
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    /// Error during series registration.
    #[error("series error: {0}")]
    Series(#[from] SeriesError),

    /// Error during record operation (write path).
    #[error("record error: {0}")]
    Record(#[from] RecordError),

    /// Error during query operation (read path).
    #[error("query error: {0}")]
    Query(#[from] QueryError),

    /// Error during schema validation or processing.
    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),

    /// Error during slab I/O operations.
    #[error("slab I/O error: {0}")]
    SlabIo(#[from] SlabIoError),

    /// Error during consolidation operations.
    #[error("consolidation error: {0}")]
    Consolidation(#[from] ConsolidationError),

    /// Error during export/drain operations.
    #[error("export error: {0}")]
    Export(#[from] ExportError),

    /// Error during remote write operations.
    #[cfg(feature = "prometheus-remote-write")]
    #[error("remote write error: {0}")]
    RemoteWrite(#[from] RemoteWriteError),
}

/// Errors that can occur when opening or creating a store.
#[derive(Error, Debug)]
pub enum StoreError {
    /// The store directory could not be created or accessed.
    #[error("failed to access store directory '{path}': {source}")]
    DirectoryAccess {
        /// The path that could not be accessed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The metadata file (meta.json) is corrupted or invalid.
    #[error("corrupted metadata file: {reason}")]
    CorruptedMetadata {
        /// Description of what was invalid about the metadata.
        reason: String,
    },

    /// Schema validation failed when opening an existing store.
    #[error("schema validation failed: existing schema hash {existing:x} does not match expected {expected:x}")]
    SchemaMismatch {
        /// Hash of the schema found in the existing store.
        existing: u64,
        /// Hash of the schema being used to open the store.
        expected: u64,
    },

    /// Failed to serialize metadata to JSON.
    #[error("failed to serialize metadata: {0}")]
    MetadataSerialize(#[from] serde_json::Error),

    /// Memory mapping failed.
    #[error("memory mapping failed for file '{path}': {source}")]
    MemoryMap {
        /// The file path that failed to map.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// Store is already locked by another process.
    #[error("store is locked by another process")]
    StoreLocked,
}

/// Errors that can occur during series registration.
#[derive(Error, Debug)]
pub enum SeriesError {
    /// Maximum number of series for a schema has been exceeded.
    #[error("maximum series count ({max_series}) exceeded for schema")]
    MaxSeriesExceeded {
        /// The maximum number of series allowed.
        max_series: u32,
    },

    /// No schema matches the provided labels.
    #[error("no schema found matching labels: {labels:?}")]
    NoMatchingSchema {
        /// The labels that didn't match any schema.
        labels: Vec<(String, String)>,
    },

    /// A series with these labels is already registered.
    #[error("series with labels {labels:?} is already registered")]
    SeriesAlreadyExists {
        /// The conflicting labels.
        labels: Vec<(String, String)>,
    },

    /// Invalid label key or value.
    #[error("invalid label {key}={value}: {reason}")]
    InvalidLabel {
        /// The label key.
        key: String,
        /// The label value.
        value: String,
        /// Why the label is invalid.
        reason: String,
    },
}

/// Errors that can occur during record operations (write path).
#[derive(Error, Debug)]
pub enum RecordError {
    /// The provided series handle is invalid or stale.
    #[error("invalid series handle: {handle}")]
    InvalidHandle {
        /// The invalid handle value.
        handle: u64,
    },

    /// The timestamp is outside the valid range for recording.
    #[error("timestamp {timestamp} is outside valid range")]
    InvalidTimestamp {
        /// The invalid timestamp.
        timestamp: u64,
    },

    /// The value is invalid (e.g., infinite or otherwise unacceptable).
    #[error("invalid value: {value} ({reason})")]
    InvalidValue {
        /// The invalid value.
        value: f64,
        /// Why the value is invalid.
        reason: String,
    },

    /// Write would exceed the ring buffer bounds.
    #[error("write would exceed ring buffer capacity")]
    BufferOverflow,
}

/// Errors that can occur during query operations (read path).
#[derive(Error, Debug)]
pub enum QueryError {
    /// The requested tier index is invalid.
    #[error("invalid tier {tier}: only {max_tiers} tiers available")]
    InvalidTier {
        /// The requested tier index.
        tier: usize,
        /// The maximum number of tiers available.
        max_tiers: usize,
    },

    /// The time range is invalid (start >= end).
    #[error("invalid time range: start {start} >= end {end}")]
    InvalidTimeRange {
        /// The start time.
        start: u64,
        /// The end time.
        end: u64,
    },

    /// The series handle is invalid for query operations.
    #[error("invalid series handle for query: {handle}")]
    InvalidSeriesHandle {
        /// The invalid handle.
        handle: u64,
    },

    /// No data available for the requested time range.
    #[error("no data available for time range {start}..{end}")]
    NoData {
        /// The start of the requested range.
        start: u64,
        /// The end of the requested range.
        end: u64,
    },
}

/// Errors that can occur during schema validation or processing.
#[derive(Error, Debug)]
pub enum SchemaError {
    /// A tier configuration is invalid.
    #[error("invalid tier configuration: {reason}")]
    InvalidTierConfig {
        /// Description of what makes the tier configuration invalid.
        reason: String,
    },

    /// Tier durations would result in too many slots.
    #[error("tier {tier} would have {slot_count} slots (max {max_slots}): duration {duration:?} / interval {interval:?}")]
    TooManySlots {
        /// The tier index that's problematic.
        tier: usize,
        /// The computed slot count.
        slot_count: u64,
        /// The maximum allowed slots.
        max_slots: u64,
        /// The retention duration.
        duration: Duration,
        /// The sample interval.
        interval: Duration,
    },

    /// Tiers are not properly ordered by resolution.
    #[error("tiers must be ordered from highest resolution to lowest resolution")]
    TiersNotOrdered,

    /// A consolidation function is specified for the highest resolution tier.
    #[error("consolidation functions are not applicable to the highest resolution tier")]
    ConsolidationOnHighestTier,

    /// No tiers are configured.
    #[error("at least one tier must be configured")]
    NoTiers,

    /// Maximum series count is invalid.
    #[error("invalid max_series count: {count} (must be > 0)")]
    InvalidMaxSeries {
        /// The invalid count.
        count: u32,
    },

    /// Label matcher configuration is invalid.
    #[error("invalid label matcher: {reason}")]
    InvalidLabelMatcher {
        /// Description of what makes the matcher invalid.
        reason: String,
    },
}

/// Errors that can occur during slab I/O operations.
#[derive(Error, Debug)]
pub enum SlabIoError {
    /// Failed to read from a slab file.
    #[error("failed to read slab '{path}' at offset {offset}: {source}")]
    ReadFailed {
        /// The slab file path.
        path: String,
        /// The byte offset where the read failed.
        offset: u64,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to write to a slab file.
    #[error("failed to write slab '{path}' at offset {offset}: {source}")]
    WriteFailed {
        /// The slab file path.
        path: String,
        /// The byte offset where the write failed.
        offset: u64,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to sync slab file to disk.
    #[error("failed to sync slab '{path}' to disk: {source}")]
    SyncFailed {
        /// The slab file path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Slab file is corrupted or has invalid format.
    #[error("slab '{path}' is corrupted: {reason}")]
    CorruptedSlab {
        /// The slab file path.
        path: String,
        /// Description of the corruption.
        reason: String,
    },

    /// Attempted to access beyond slab boundaries.
    #[error("access beyond slab bounds: offset {offset} + length {length} > slab size {slab_size}")]
    BoundsViolation {
        /// The attempted offset.
        offset: u64,
        /// The attempted read/write length.
        length: u64,
        /// The actual slab size.
        slab_size: u64,
    },
}

/// Errors that can occur during consolidation operations.
#[derive(Error, Debug)]
pub enum ConsolidationError {
    /// Failed to load consolidation cursors from file.
    #[error("failed to load consolidation cursors from '{path}': {source}")]
    CursorLoad {
        /// The cursor file path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse consolidation cursors from JSON.
    #[error("failed to parse consolidation cursors from '{path}': {source}")]
    CursorParse {
        /// The cursor file path.
        path: String,
        /// The underlying JSON parsing error.
        #[source]
        source: serde_json::Error,
    },

    /// Failed to save consolidation cursors to file.
    #[error("failed to save consolidation cursors to '{path}': {source}")]
    CursorSave {
        /// The cursor file path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to serialize consolidation cursors to JSON.
    #[error("failed to serialize consolidation cursors: {source}")]
    CursorSerialize {
        /// The underlying JSON serialization error.
        #[source]
        source: serde_json::Error,
    },

    /// Tier does not have a consolidation function configured.
    #[error("tier {tier_index} in schema {schema_index} has no consolidation function")]
    NoConsolidationFunction {
        /// The schema index.
        schema_index: usize,
        /// The tier index missing consolidation function.
        tier_index: usize,
    },

    /// Consolidation window processing failed.
    #[error("failed to process consolidation window {start_timestamp}..{end_timestamp}: {reason}")]
    WindowProcessingFailed {
        /// Window start timestamp.
        start_timestamp: u64,
        /// Window end timestamp.
        end_timestamp: u64,
        /// Description of the failure.
        reason: String,
    },

    /// Invalid consolidation configuration.
    #[error("invalid consolidation configuration: {reason}")]
    InvalidConfiguration {
        /// Description of what's invalid.
        reason: String,
    },
}

/// Errors that can occur during export/drain operations.
#[derive(Error, Debug)]
pub enum ExportError {
    /// Failed to load export cursor from file.
    #[error("failed to load export cursor from '{}': {source}", path.display())]
    CursorLoad {
        /// The cursor file path.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse export cursor from JSON.
    #[error("failed to parse export cursor from '{}': {source}", path.display())]
    CursorParse {
        /// The cursor file path.
        path: std::path::PathBuf,
        /// The underlying JSON parsing error.
        #[source]
        source: serde_json::Error,
    },

    /// Failed to save export cursor to file.
    #[error("failed to save export cursor to '{}': {source}", path.display())]
    CursorSave {
        /// The cursor file path.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to serialize export cursor to JSON.
    #[error("failed to serialize export cursor: {source}")]
    CursorSerialize {
        /// The underlying JSON serialization error.
        #[source]
        source: serde_json::Error,
    },
}

/// Errors that can occur during Prometheus remote-write operations.
#[cfg(feature = "prometheus-remote-write")]
#[derive(Error, Debug)]
pub enum RemoteWriteError {
    /// Failed to serialize `WriteRequest` to protobuf.
    #[error("failed to serialize write request: {source}")]
    Serialization {
        /// The protobuf encoding error.
        #[source]
        source: prost::EncodeError,
    },

    /// Failed to compress data with Snappy.
    #[error("failed to compress data: {source}")]
    Compression {
        /// The snappy compression error.
        #[source]
        source: snap::Error,
    },

    /// Failed to create HTTP client.
    #[error("failed to create HTTP client: {source}")]
    ClientCreate {
        /// The underlying reqwest error.
        #[source]
        source: reqwest::Error,
    },

    /// HTTP request failed after retries.
    #[error("HTTP request failed: {source}")]
    RequestFailed {
        /// The underlying reqwest error.
        #[source]
        source: reqwest::Error,
    },

    /// Server returned non-2xx status after retries.
    #[error("server returned status {status}: {body}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// The response body text.
        body: String,
    },

    /// Series handle not found in registry.
    #[error("series not found for schema_index={schema_index} column={column}")]
    SeriesNotFound {
        /// The schema index.
        schema_index: usize,
        /// The column.
        column: u32,
    },
}

/// Type alias for `Result<T, RondoError>`.
pub type Result<T> = std::result::Result<T, RondoError>;