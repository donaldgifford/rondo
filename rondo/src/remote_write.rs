//! Prometheus remote-write client for exporting rondo data.
//!
//! Serializes drain output to the Prometheus remote-write protobuf format
//! and pushes it to a configurable endpoint with snappy compression and
//! basic retry logic.
//!
//! This module is only available when the `prometheus-remote-write` feature
//! is enabled.
//!
//! # Example
//!
//! ```rust,no_run
//! use rondo::store::Store;
//! use rondo::export::ExportCursor;
//! use rondo::remote_write::{push, RemoteWriteConfig};
//! # use rondo::schema::{SchemaConfig, LabelMatcher, TierConfig};
//! # use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let schemas = vec![SchemaConfig {
//! #     name: "test".to_string(),
//! #     label_matcher: LabelMatcher::any(),
//! #     tiers: vec![TierConfig::new(Duration::from_secs(1), Duration::from_secs(60), None)?],
//! #     max_series: 10,
//! # }];
//! # let store = Store::open("/tmp/remote_write_example", schemas)?;
//! let config = RemoteWriteConfig::new("http://localhost:9090/api/v1/write");
//! let mut cursor = ExportCursor::load_or_new("/tmp/cursor_prom.json")?;
//!
//! let exports = store.drain(0, &mut cursor)?;
//! push(&config, &exports, &store, &[])?;
//! cursor.save()?;
//! # Ok(())
//! # }
//! ```

use std::time::Duration;

use prost::Message;

use crate::error::{RemoteWriteError, Result};
use crate::export::SeriesExport;
use crate::store::Store;

/// Prometheus remote-write protobuf types.
///
/// Hand-written types matching `prometheus/prompb/remote.proto`.
/// Using prost derives avoids the need for protoc and proto file management.
pub mod proto {
    /// A write request containing one or more time series.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WriteRequest {
        /// The time series to write.
        #[prost(message, repeated, tag = "1")]
        pub timeseries: Vec<TimeSeries>,
    }

    /// A single time series with labels and samples.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TimeSeries {
        /// Metric labels identifying the series.
        #[prost(message, repeated, tag = "1")]
        pub labels: Vec<Label>,
        /// Data samples for this series.
        #[prost(message, repeated, tag = "2")]
        pub samples: Vec<Sample>,
    }

    /// A key-value label pair.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Label {
        /// Label name.
        #[prost(string, tag = "1")]
        pub name: String,
        /// Label value.
        #[prost(string, tag = "2")]
        pub value: String,
    }

    /// A single data sample (value + timestamp).
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Sample {
        /// The sample value.
        #[prost(double, tag = "1")]
        pub value: f64,
        /// Timestamp in milliseconds since epoch.
        #[prost(int64, tag = "2")]
        pub timestamp: i64,
    }
}

/// Configuration for a Prometheus remote-write endpoint.
#[derive(Debug, Clone)]
pub struct RemoteWriteConfig {
    /// Remote write endpoint URL (e.g., `http://localhost:9090/api/v1/write`).
    pub endpoint: String,
    /// HTTP timeout for write requests.
    pub timeout: Duration,
    /// Maximum number of retry attempts on failure.
    pub max_retries: u32,
    /// Initial backoff duration between retries (doubles each attempt).
    pub retry_backoff: Duration,
    /// Optional HTTP headers (e.g., for authentication).
    pub headers: Vec<(String, String)>,
}

impl RemoteWriteConfig {
    /// Creates a new config with sensible defaults.
    ///
    /// Defaults: 30s timeout, 3 retries, 100ms initial backoff.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_backoff: Duration::from_millis(100),
            headers: Vec::new(),
        }
    }

    /// Adds an HTTP header (e.g., for authentication tokens).
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sets the HTTP timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the maximum number of retries.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }
}

/// Pushes series exports to a Prometheus remote-write endpoint.
///
/// Converts drain output into the Prometheus remote-write protobuf format,
/// compresses with snappy, and POSTs to the configured endpoint with retry logic.
///
/// `external_labels` are merged into every time series' label set, allowing
/// multiple VMM instances to be distinguished in Prometheus (e.g.,
/// `instance=vmm_1`).
///
/// # Errors
///
/// Returns `RemoteWriteError` if serialization fails, the server rejects the
/// request after all retries, or the series cannot be found in the store.
pub fn push(
    config: &RemoteWriteConfig,
    exports: &[SeriesExport],
    store: &Store,
    external_labels: &[(String, String)],
) -> Result<usize> {
    if exports.is_empty() {
        return Ok(0);
    }

    let request = build_write_request(exports, store, external_labels)?;
    let proto_bytes = serialize_write_request(&request)?;
    let compressed = compress_snappy(&proto_bytes)?;
    send_with_retry(config, &compressed)?;

    Ok(exports.len())
}

/// Encodes series exports as a Prometheus remote-write protobuf payload.
///
/// Returns the snappy-compressed protobuf bytes suitable for HTTP POST.
/// This is useful for testing or custom transport implementations.
///
/// `external_labels` are merged into every time series' label set.
///
/// # Errors
///
/// Returns an error if a series handle cannot be found in the store,
/// or if serialization/compression fails.
pub fn encode(
    exports: &[SeriesExport],
    store: &Store,
    external_labels: &[(String, String)],
) -> Result<Vec<u8>> {
    let request = build_write_request(exports, store, external_labels)?;
    let proto_bytes = serialize_write_request(&request)?;
    compress_snappy(&proto_bytes)
}

/// Converts `SeriesExport` data to a Prometheus `WriteRequest`.
fn build_write_request(
    exports: &[SeriesExport],
    store: &Store,
    external_labels: &[(String, String)],
) -> Result<proto::WriteRequest> {
    let mut timeseries = Vec::with_capacity(exports.len());

    for export in exports {
        let (name, labels) =
            store
                .series_info(&export.handle)
                .ok_or_else(|| RemoteWriteError::SeriesNotFound {
                    schema_index: export.handle.schema_index,
                    column: export.handle.column,
                })?;

        let ts = proto::TimeSeries {
            labels: build_labels(name, labels, external_labels),
            samples: build_samples(&export.points),
        };

        timeseries.push(ts);
    }

    Ok(proto::WriteRequest { timeseries })
}

/// Builds Prometheus labels from series name, series labels, and external labels.
///
/// Adds the required `__name__` label, merges in any external labels, and
/// sorts the result alphabetically as required by the Prometheus spec.
fn build_labels(
    name: &str,
    labels: &[(String, String)],
    external_labels: &[(String, String)],
) -> Vec<proto::Label> {
    let mut result = Vec::with_capacity(labels.len() + external_labels.len() + 1);

    result.push(proto::Label {
        name: "__name__".to_string(),
        value: name.to_string(),
    });

    for (key, value) in labels {
        result.push(proto::Label {
            name: key.clone(),
            value: value.clone(),
        });
    }

    for (key, value) in external_labels {
        result.push(proto::Label {
            name: key.clone(),
            value: value.clone(),
        });
    }

    // Prometheus requires labels sorted by name
    result.sort_by(|a, b| a.name.cmp(&b.name));

    result
}

/// Converts timestamp-value pairs to Prometheus samples.
///
/// Timestamps are converted from nanoseconds to milliseconds.
#[allow(clippy::cast_possible_truncation)] // ns-to-ms conversion is safe for current epoch
fn build_samples(points: &[(u64, f64)]) -> Vec<proto::Sample> {
    points
        .iter()
        .map(|&(timestamp_ns, value)| proto::Sample {
            value,
            timestamp: (timestamp_ns / 1_000_000) as i64,
        })
        .collect()
}

/// Serializes a `WriteRequest` to protobuf bytes.
fn serialize_write_request(request: &proto::WriteRequest) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(request.encoded_len());
    request
        .encode(&mut buf)
        .map_err(|e| RemoteWriteError::Serialization { source: e })?;
    Ok(buf)
}

/// Compresses bytes using Snappy (required by Prometheus remote-write spec).
fn compress_snappy(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = snap::raw::Encoder::new();
    encoder
        .compress_vec(data)
        .map_err(|e| RemoteWriteError::Compression { source: e })
        .map_err(Into::into)
}

/// Sends compressed protobuf to the endpoint with exponential backoff retry.
fn send_with_retry(config: &RemoteWriteConfig, body: &[u8]) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(config.timeout)
        .build()
        .map_err(|e| RemoteWriteError::ClientCreate { source: e })?;

    let mut last_error = None;
    let mut backoff = config.retry_backoff;

    for attempt in 0..=config.max_retries {
        let mut request = client
            .post(&config.endpoint)
            .header("Content-Encoding", "snappy")
            .header("Content-Type", "application/x-protobuf")
            .header("X-Prometheus-Remote-Write-Version", "0.1.0");

        for (name, value) in &config.headers {
            request = request.header(name, value);
        }

        match request.body(body.to_vec()).send() {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().unwrap_or_default();
                last_error = Some(RemoteWriteError::HttpStatus { status, body });
            }
            Err(e) => {
                last_error = Some(RemoteWriteError::RequestFailed { source: e });
            }
        }

        if attempt < config.max_retries {
            std::thread::sleep(backoff);
            backoff *= 2;
        }
    }

    Err(last_error.expect("at least one attempt was made").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LabelMatcher, SchemaConfig, TierConfig};

    fn create_test_store(dir: &std::path::Path) -> Store {
        let store_dir = dir.join("store");
        let schemas = vec![SchemaConfig {
            name: "test".to_string(),
            label_matcher: LabelMatcher::any(),
            tiers: vec![
                TierConfig::new(Duration::from_secs(1), Duration::from_secs(60), None).unwrap(),
            ],
            max_series: 10,
        }];
        Store::open(&store_dir, schemas).unwrap()
    }

    #[test]
    fn test_build_labels() {
        let labels = vec![
            ("host".to_string(), "web1".to_string()),
            ("dc".to_string(), "us-east".to_string()),
        ];

        let result = build_labels("cpu_usage", &labels, &[]);

        // Should be sorted alphabetically
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "__name__");
        assert_eq!(result[0].value, "cpu_usage");
        assert_eq!(result[1].name, "dc");
        assert_eq!(result[1].value, "us-east");
        assert_eq!(result[2].name, "host");
        assert_eq!(result[2].value, "web1");
    }

    #[test]
    fn test_build_labels_with_external() {
        let labels = vec![("host".to_string(), "web1".to_string())];
        let external = vec![("instance".to_string(), "vmm_1".to_string())];

        let result = build_labels("cpu_usage", &labels, &external);

        // Should be sorted: __name__, host, instance
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "__name__");
        assert_eq!(result[0].value, "cpu_usage");
        assert_eq!(result[1].name, "host");
        assert_eq!(result[1].value, "web1");
        assert_eq!(result[2].name, "instance");
        assert_eq!(result[2].value, "vmm_1");
    }

    #[test]
    fn test_build_samples() {
        let points = vec![
            (1_700_000_000_000_000_000u64, 42.0),
            (1_700_000_001_000_000_000u64, 43.0),
        ];

        let samples = build_samples(&points);

        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].value, 42.0);
        assert_eq!(samples[0].timestamp, 1_700_000_000_000); // ms
        assert_eq!(samples[1].value, 43.0);
        assert_eq!(samples[1].timestamp, 1_700_000_001_000); // ms
    }

    #[test]
    fn test_build_write_request_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = create_test_store(dir.path());

        let exports: Vec<SeriesExport> = Vec::new();
        let request = build_write_request(&exports, &store, &[]).unwrap();

        assert!(request.timeseries.is_empty());
    }

    #[test]
    fn test_build_write_request_with_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = create_test_store(dir.path());

        let handle = store
            .register("cpu", &[("host".to_string(), "web1".to_string())])
            .unwrap();

        let exports = vec![SeriesExport {
            handle,
            points: vec![
                (1_700_000_000_000_000_000, 85.5),
                (1_700_000_001_000_000_000, 90.0),
            ],
        }];

        let request = build_write_request(&exports, &store, &[]).unwrap();

        assert_eq!(request.timeseries.len(), 1);
        let ts = &request.timeseries[0];
        assert_eq!(ts.labels.len(), 2);
        assert_eq!(ts.labels[0].name, "__name__");
        assert_eq!(ts.labels[0].value, "cpu");
        assert_eq!(ts.labels[1].name, "host");
        assert_eq!(ts.labels[1].value, "web1");
        assert_eq!(ts.samples.len(), 2);
        assert_eq!(ts.samples[0].value, 85.5);
    }

    #[test]
    fn test_serialize_and_compress_roundtrip() {
        let request = proto::WriteRequest {
            timeseries: vec![proto::TimeSeries {
                labels: vec![proto::Label {
                    name: "__name__".to_string(),
                    value: "test".to_string(),
                }],
                samples: vec![proto::Sample {
                    value: 42.0,
                    timestamp: 1_700_000_000_000,
                }],
            }],
        };

        let proto_bytes = serialize_write_request(&request).unwrap();
        assert!(!proto_bytes.is_empty());

        let compressed = compress_snappy(&proto_bytes).unwrap();
        assert!(!compressed.is_empty());

        // Decompress and decode to verify roundtrip
        let mut decoder = snap::raw::Decoder::new();
        let decompressed = decoder.decompress_vec(&compressed).unwrap();
        assert_eq!(decompressed, proto_bytes);

        let decoded = proto::WriteRequest::decode(decompressed.as_slice()).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn test_encode_produces_valid_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = create_test_store(dir.path());

        let handle = store
            .register("metric_a", &[("id".to_string(), "1".to_string())])
            .unwrap();

        let exports = vec![SeriesExport {
            handle,
            points: vec![(1_700_000_000_000_000_000, 99.9)],
        }];

        let bytes = encode(&exports, &store, &[]).unwrap();
        assert!(!bytes.is_empty());

        // Verify decompression and decoding
        let mut decoder = snap::raw::Decoder::new();
        let decompressed = decoder.decompress_vec(&bytes).unwrap();
        let request = proto::WriteRequest::decode(decompressed.as_slice()).unwrap();

        assert_eq!(request.timeseries.len(), 1);
        assert_eq!(request.timeseries[0].samples[0].value, 99.9);
    }

    #[test]
    fn test_push_empty_exports() {
        let config = RemoteWriteConfig::new("http://localhost:9999/api/v1/write");
        let dir = tempfile::tempdir().unwrap();
        let store = create_test_store(dir.path());

        // Should succeed immediately with 0 count, no HTTP call
        let count = push(&config, &[], &store, &[]).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_config_builder() {
        let config = RemoteWriteConfig::new("http://example.com/write")
            .with_header("Authorization", "Bearer token123")
            .with_timeout(Duration::from_secs(10))
            .with_max_retries(5);

        assert_eq!(config.endpoint, "http://example.com/write");
        assert_eq!(config.timeout, Duration::from_secs(10));
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.headers.len(), 1);
        assert_eq!(config.headers[0].0, "Authorization");
        assert_eq!(config.headers[0].1, "Bearer token123");
    }

    #[test]
    fn test_series_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = create_test_store(dir.path());

        // Create a bogus handle that doesn't exist in the store
        let bogus_handle = crate::series::SeriesHandle {
            schema_index: 0,
            series_id: 99,
            column: 99,
        };

        let exports = vec![SeriesExport {
            handle: bogus_handle,
            points: vec![(1_000_000_000_000_000_000, 1.0)],
        }];

        let result = build_write_request(&exports, &store, &[]);
        assert!(result.is_err());
    }
}
