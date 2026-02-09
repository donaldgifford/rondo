//! Query interface for reading time-series data from rondo storage.
//!
//! This module provides a high-level query API that handles tier selection,
//! range validation, and metadata reporting. It acts as a routing layer over
//! the ring buffer implementation, selecting the appropriate storage tier
//! based on query requirements and retention policies.
//!
//! # Overview
//!
//! The query system supports two main query modes:
//!
//! - **Direct tier queries** - Query a specific tier with explicit validation
//! - **Automatic tier selection** - Choose the best tier based on retention coverage
//!
//! Both modes return a [`QueryResult`] that wraps the iterator with metadata
//! about the query execution, including which tier was used and whether data
//! may be incomplete.
//!
//! # Example Usage
//!
//! ```rust,no_run
//! # use rondo::store::Store;
//! # use rondo::schema::SchemaConfig;
//! # let mut store = Store::open("./data", vec![])?;
//! # let handle = store.register("cpu.usage", &[])?;
//! # let start_ns = 1_640_000_000_000_000_000u64;
//! # let end_ns = start_ns + 3600 * 1_000_000_000;
//! // Query specific tier
//! let result = store.query(handle, 0, start_ns, end_ns)?;
//! println!("Using tier {}, got {} data points", result.tier_used(), result.count());
//!
//! // Auto-select best tier
//! let result = store.query_auto(handle, start_ns, end_ns)?;
//! if result.may_be_incomplete() {
//!     println!("Warning: some data may be outside retention window");
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use crate::ring::RingIterator;

/// Result of a time-series query operation.
///
/// This struct wraps a ring buffer iterator with metadata about the query
/// execution, including which tier was selected, the actual time range
/// available, and whether the data may be incomplete due to retention limits.
///
/// The result implements `Iterator` to provide direct access to `(timestamp, value)`
/// pairs while preserving query metadata for analysis and monitoring.
#[derive(Debug)]
pub struct QueryResult<'a> {
    /// The underlying iterator over time-series data.
    iterator: RingIterator<'a>,

    /// Which tier index was used for this query.
    tier_used: usize,

    /// The time range actually available in the selected tier.
    available_range: (Option<u64>, Option<u64>),

    /// The requested time range.
    requested_range: (u64, u64),

    /// Whether data may be incomplete due to retention limits.
    may_be_incomplete: bool,
}

impl<'a> QueryResult<'a> {
    /// Creates a new query result.
    ///
    /// # Arguments
    ///
    /// * `iterator` - The ring buffer iterator
    /// * `tier_used` - Which tier index was selected
    /// * `available_range` - The actual time range available (oldest, newest)
    /// * `requested_range` - The time range that was requested
    /// * `may_be_incomplete` - Whether data may be missing due to retention
    pub fn new(
        iterator: RingIterator<'a>,
        tier_used: usize,
        available_range: (Option<u64>, Option<u64>),
        requested_range: (u64, u64),
        may_be_incomplete: bool,
    ) -> Self {
        Self {
            iterator,
            tier_used,
            available_range,
            requested_range,
            may_be_incomplete,
        }
    }

    /// Returns the tier index that was used for this query.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let start_ns = 1_640_000_000_000_000_000u64;
    /// # let end_ns = start_ns + 3600 * 1_000_000_000;
    /// let result = store.query(handle, 1, start_ns, end_ns)?;
    /// assert_eq!(result.tier_used(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn tier_used(&self) -> usize {
        self.tier_used
    }

    /// Returns the time range actually available in the selected tier.
    ///
    /// Returns `(oldest_timestamp, newest_timestamp)` where either value
    /// may be `None` if the tier is empty.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let start_ns = 1_640_000_000_000_000_000u64;
    /// # let end_ns = start_ns + 3600 * 1_000_000_000;
    /// let result = store.query_auto(handle, start_ns, end_ns)?;
    /// if let (Some(oldest), Some(newest)) = result.available_range() {
    ///     println!("Data spans from {} to {}", oldest, newest);
    /// }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn available_range(&self) -> (Option<u64>, Option<u64>) {
        self.available_range
    }

    /// Returns the time range that was originally requested.
    ///
    /// Returns `(start_ns, end_ns)` as provided to the query method.
    pub fn requested_range(&self) -> (u64, u64) {
        self.requested_range
    }

    /// Returns whether the query result may be incomplete.
    ///
    /// This is `true` when the requested time range extends beyond the
    /// retention window of the selected tier, meaning some data points
    /// may have been overwritten or never existed.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let very_old_timestamp = 1_000_000_000_000_000_000u64;
    /// # let end_ns = 1_640_000_000_000_000_000u64;
    /// let result = store.query_auto(handle, very_old_timestamp, end_ns)?;
    /// if result.may_be_incomplete() {
    ///     println!("Warning: some historical data may be missing");
    /// }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn may_be_incomplete(&self) -> bool {
        self.may_be_incomplete
    }

    /// Returns the number of data points that will be returned by this query.
    ///
    /// Note: This consumes the iterator, so the `QueryResult` cannot be used
    /// to iterate after calling this method.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let start_ns = 1_640_000_000_000_000_000u64;
    /// # let end_ns = start_ns + 3600 * 1_000_000_000;
    /// let result = store.query(handle, 0, start_ns, end_ns)?;
    /// println!("Query returned {} data points", result.count());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn count(self) -> usize {
        self.iterator.count()
    }

    /// Collects all data points into a vector.
    ///
    /// This is a convenience method for cases where you need all data points
    /// in memory at once. For large queries, prefer iterating directly.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::store::Store;
    /// # let mut store = Store::open("./data", vec![])?;
    /// # let handle = store.register("cpu.usage", &[])?;
    /// # let start_ns = 1_640_000_000_000_000_000u64;
    /// # let end_ns = start_ns + 3600 * 1_000_000_000;
    /// let result = store.query(handle, 0, start_ns, end_ns)?;
    /// let data_points: Vec<(u64, f64)> = result.collect_all();
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn collect_all(self) -> Vec<(u64, f64)> {
        self.iterator.collect()
    }
}

impl<'a> Iterator for QueryResult<'a> {
    type Item = (u64, f64);

    fn next(&mut self) -> Option<Self::Item> {
        self.iterator.next()
    }
}

/// Determines if a time range is covered by a tier's retention window.
///
/// # Arguments
///
/// * `oldest` - The oldest timestamp available in the tier (None if empty)
/// * `newest` - The newest timestamp available in the tier (None if empty)
/// * `start_ns` - Start of the requested range
/// * `end_ns` - End of the requested range
///
/// # Returns
///
/// `(fully_covered, may_be_incomplete)` where:
/// - `fully_covered` - True if the entire range is within retention
/// - `may_be_incomplete` - True if some data might be missing
pub fn analyze_coverage(
    oldest: Option<u64>,
    newest: Option<u64>,
    start_ns: u64,
    end_ns: u64,
) -> (bool, bool) {
    match (oldest, newest) {
        (Some(oldest_ts), Some(newest_ts)) => {
            // Check if requested range is fully within available range
            let fully_covered = start_ns >= oldest_ts && end_ns <= newest_ts;

            // Data may be incomplete if request starts before oldest available
            // or ends after newest available (though ending after newest is
            // expected for real-time queries)
            let may_be_incomplete = start_ns < oldest_ts;

            (fully_covered, may_be_incomplete)
        }
        _ => {
            // No data available in tier
            (false, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_coverage_full_coverage() {
        // Available: 1000..5000, Requested: 2000..4000
        let (fully_covered, incomplete) = analyze_coverage(Some(1000), Some(5000), 2000, 4000);
        assert!(fully_covered);
        assert!(!incomplete);
    }

    #[test]
    fn test_analyze_coverage_starts_too_early() {
        // Available: 2000..5000, Requested: 1000..4000
        let (fully_covered, incomplete) = analyze_coverage(Some(2000), Some(5000), 1000, 4000);
        assert!(!fully_covered);
        assert!(incomplete);
    }

    #[test]
    fn test_analyze_coverage_ends_too_late() {
        // Available: 1000..3000, Requested: 2000..5000
        let (fully_covered, incomplete) = analyze_coverage(Some(1000), Some(3000), 2000, 5000);
        assert!(!fully_covered);
        assert!(!incomplete); // Ending after newest is normal for real-time queries
    }

    #[test]
    fn test_analyze_coverage_no_data() {
        let (fully_covered, incomplete) = analyze_coverage(None, None, 1000, 2000);
        assert!(!fully_covered);
        assert!(incomplete);
    }

    #[test]
    fn test_analyze_coverage_exact_match() {
        // Available: 1000..2000, Requested: 1000..2000
        let (fully_covered, incomplete) = analyze_coverage(Some(1000), Some(2000), 1000, 2000);
        assert!(fully_covered);
        assert!(!incomplete);
    }

    #[test]
    fn test_analyze_coverage_request_before_data() {
        // Available: 2000..3000, Requested: 500..1000
        let (fully_covered, incomplete) = analyze_coverage(Some(2000), Some(3000), 500, 1000);
        assert!(!fully_covered);
        assert!(incomplete);
    }
}
