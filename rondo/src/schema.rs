//! Schema configuration types for Rondo time-series storage.
//!
//! These types define how metrics are stored, including resolution tiers,
//! retention policies, and label-based routing. Schema configuration happens
//! at store creation time and determines the storage layout and behavior.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SchemaError};

/// Maximum number of slots allowed in any single tier.
///
/// This prevents excessive memory usage from misconfigured durations/intervals.
/// With 8 bytes per slot (f64), this allows up to ~8GB per tier.
const MAX_SLOTS_PER_TIER: u64 = 1_000_000_000;

/// Configuration defining how a class of metrics is stored.
///
/// A `SchemaConfig` determines which time series match (via label matching),
/// how they're stored across multiple resolution tiers, and resource limits.
/// Multiple schemas can be configured in a single store to handle different
/// types of metrics with different requirements.
///
/// # Example
///
/// ```rust
/// use std::time::Duration;
/// use rondo::schema::{SchemaConfig, TierConfig, LabelMatcher, ConsolidationFn};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let schema = SchemaConfig {
///     name: "cpu_metrics".to_string(),
///     label_matcher: LabelMatcher::new([
///         ("metric_type", "cpu"),
///     ]),
///     tiers: vec![
///         // High resolution: 1s samples for 1 hour
///         TierConfig::new(
///             Duration::from_secs(1),
///             Duration::from_secs(3600),
///             None, // no consolidation for highest tier
///         )?,
///         // Low resolution: 1m samples for 1 day, averaged
///         TierConfig::new(
///             Duration::from_secs(60),
///             Duration::from_secs(86400),
///             Some(ConsolidationFn::Average),
///         )?,
///     ],
///     max_series: 1000,
/// };
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaConfig {
    /// Human-readable name for this schema.
    pub name: String,

    /// Label matcher that determines which series use this schema.
    ///
    /// A series matches this schema if all labels specified in the matcher
    /// are present in the series labels with matching values.
    pub label_matcher: LabelMatcher,

    /// Resolution tiers, ordered from highest resolution to lowest.
    ///
    /// The first tier receives all writes and has no consolidation function.
    /// Subsequent tiers receive consolidated data from the previous tier.
    /// Each tier can have different sample intervals and retention durations.
    pub tiers: Vec<TierConfig>,

    /// Maximum number of series that can use this schema.
    ///
    /// This determines the size of pre-allocated slabs and affects memory usage.
    /// Choose based on expected cardinality of matching metrics.
    pub max_series: u32,
}

impl SchemaConfig {
    /// Creates a new schema configuration.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for this schema
    /// * `label_matcher` - Matcher to determine which series use this schema
    /// * `tiers` - Resolution tiers, ordered highest to lowest resolution
    /// * `max_series` - Maximum number of series for this schema
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] if the configuration is invalid:
    /// - No tiers specified
    /// - Tiers not properly ordered
    /// - Consolidation function on highest resolution tier
    /// - Invalid max_series count
    /// - Any tier configuration is invalid
    pub fn new(
        name: String,
        label_matcher: LabelMatcher,
        tiers: Vec<TierConfig>,
        max_series: u32,
    ) -> Result<Self> {
        let config = Self {
            name,
            label_matcher,
            tiers,
            max_series,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validates the schema configuration.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] if validation fails.
    pub fn validate(&self) -> Result<()> {
        // Check basic constraints
        if self.tiers.is_empty() {
            return Err(SchemaError::NoTiers.into());
        }

        if self.max_series == 0 {
            return Err(SchemaError::InvalidMaxSeries {
                count: self.max_series,
            }
            .into());
        }

        // Validate each tier individually
        for tier in &self.tiers {
            tier.validate()?;
        }

        // Check tier ordering: intervals should increase (resolution decreases)
        for window in self.tiers.windows(2) {
            let current = &window[0];
            let next = &window[1];

            if current.interval >= next.interval {
                return Err(SchemaError::TiersNotOrdered.into());
            }
        }

        // Check that highest resolution tier has no consolidation function
        if let Some(first_tier) = self.tiers.first()
            && first_tier.consolidation_fn.is_some()
        {
            return Err(SchemaError::ConsolidationOnHighestTier.into());
        }

        Ok(())
    }

    /// Computes a stable hash of this schema configuration.
    ///
    /// This hash is used in slab headers to detect schema changes when
    /// opening an existing store. The hash includes all fields that would
    /// affect the storage layout or data interpretation.
    ///
    /// # Returns
    ///
    /// A 64-bit hash that should remain stable across Rondo versions.
    pub fn stable_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();

        // Hash fields that affect storage layout
        self.label_matcher.hash(&mut hasher);
        self.tiers.hash(&mut hasher);
        self.max_series.hash(&mut hasher);

        // Note: We deliberately exclude `name` from the hash since it's
        // only used for human readability and doesn't affect storage.

        hasher.finish()
    }

    /// Checks if the given labels match this schema's label matcher.
    ///
    /// # Arguments
    ///
    /// * `labels` - The labels to test for matching
    ///
    /// # Returns
    ///
    /// `true` if the labels match this schema, `false` otherwise.
    pub fn matches_labels(&self, labels: &[(String, String)]) -> bool {
        self.label_matcher.matches(labels)
    }
}

/// Configuration for a single resolution tier.
///
/// Each tier defines a sample interval, retention duration, and optional
/// consolidation function. Tiers are arranged in decreasing resolution order,
/// with the first tier having the highest resolution (smallest interval).
#[derive(Debug, Clone, PartialEq, Hash, Serialize, Deserialize)]
pub struct TierConfig {
    /// Time interval between samples in this tier.
    ///
    /// For the highest resolution tier, this is the native sample interval.
    /// For lower resolution tiers, this determines how frequently consolidated
    /// values are computed from the previous tier.
    #[serde(with = "duration_serde")]
    pub interval: Duration,

    /// How long to retain data in this tier.
    ///
    /// This determines the size of the ring buffer (retention / interval slots).
    /// Older data is overwritten when the buffer wraps around.
    #[serde(with = "duration_serde")]
    pub retention: Duration,

    /// Function used to consolidate data from the previous tier.
    ///
    /// Must be `None` for the highest resolution tier. For other tiers,
    /// determines how multiple high-resolution samples are aggregated
    /// into a single lower-resolution sample.
    pub consolidation_fn: Option<ConsolidationFn>,
}

impl TierConfig {
    /// Creates a new tier configuration.
    ///
    /// # Arguments
    ///
    /// * `interval` - Time between samples
    /// * `retention` - How long to retain data
    /// * `consolidation_fn` - Optional consolidation function (None for highest res tier)
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] if the configuration is invalid.
    pub fn new(
        interval: Duration,
        retention: Duration,
        consolidation_fn: Option<ConsolidationFn>,
    ) -> Result<Self> {
        let config = Self {
            interval,
            retention,
            consolidation_fn,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validates this tier configuration.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] if validation fails.
    pub fn validate(&self) -> Result<()> {
        // Check for zero durations
        if self.interval.is_zero() {
            return Err(SchemaError::InvalidTierConfig {
                reason: "interval cannot be zero".to_string(),
            }
            .into());
        }

        if self.retention.is_zero() {
            return Err(SchemaError::InvalidTierConfig {
                reason: "retention cannot be zero".to_string(),
            }
            .into());
        }

        // Check that retention is at least one interval
        if self.retention < self.interval {
            return Err(SchemaError::InvalidTierConfig {
                reason: format!(
                    "retention ({:?}) must be >= interval ({:?})",
                    self.retention, self.interval
                ),
            }
            .into());
        }

        // Check that slot count is reasonable
        let slot_count = self.slot_count();
        if slot_count > MAX_SLOTS_PER_TIER {
            return Err(SchemaError::TooManySlots {
                tier: 0, // Will be filled in by caller if needed
                slot_count,
                max_slots: MAX_SLOTS_PER_TIER,
                duration: self.retention,
                interval: self.interval,
            }
            .into());
        }

        Ok(())
    }

    /// Computes the number of slots (samples) in this tier's ring buffer.
    ///
    /// # Returns
    ///
    /// The number of slots, calculated as `retention / interval`.
    /// Returns 0 if the durations are invalid or too large to represent.
    #[allow(clippy::cast_possible_truncation)] // Intentional for performance-critical path
    pub fn slot_count(&self) -> u64 {
        // Use nanoseconds for precise division
        // Note: We accept truncation here as durations > u64::MAX nanos are impractical
        let retention_nanos = self.retention.as_nanos() as u64;
        let interval_nanos = self.interval.as_nanos() as u64;

        if interval_nanos == 0 {
            return 0;
        }

        retention_nanos / interval_nanos
    }
}

/// Aggregation function for consolidating high-resolution data into lower-resolution tiers.
///
/// Each function defines how multiple samples from a higher resolution tier
/// are combined into a single sample in a lower resolution tier. NaN values
/// are filtered out before aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConsolidationFn {
    /// Arithmetic mean of all non-NaN values.
    Average,

    /// Minimum of all non-NaN values.
    Min,

    /// Maximum of all non-NaN values.
    Max,

    /// Most recent (last) non-NaN value.
    Last,

    /// Sum of all non-NaN values.
    Sum,

    /// Count of non-NaN values.
    Count,
}

impl ConsolidationFn {
    /// Applies this consolidation function to a slice of values.
    ///
    /// NaN values are filtered out before aggregation. If all values are NaN
    /// or the slice is empty, returns NaN.
    ///
    /// # Arguments
    ///
    /// * `values` - The values to consolidate
    ///
    /// # Returns
    ///
    /// The consolidated value, or NaN if no valid values are present.
    ///
    /// # Panics
    ///
    /// May panic if there are precision issues with very large counts (>2^52),
    /// though this is extremely unlikely in practice.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rondo::schema::ConsolidationFn;
    ///
    /// let values = [1.0, 2.0, f64::NAN, 4.0];
    ///
    /// let avg = ConsolidationFn::Average.apply(&values);
    /// assert!((avg - (7.0 / 3.0)).abs() < 1e-10);
    /// assert_eq!(ConsolidationFn::Min.apply(&values), 1.0);
    /// assert_eq!(ConsolidationFn::Max.apply(&values), 4.0);
    /// assert_eq!(ConsolidationFn::Last.apply(&values), 4.0);
    /// assert_eq!(ConsolidationFn::Sum.apply(&values), 7.0);
    /// assert_eq!(ConsolidationFn::Count.apply(&values), 3.0);
    /// ```
    #[allow(clippy::cast_precision_loss)] // Acceptable for consolidation operations
    pub fn apply(self, values: &[f64]) -> f64 {
        // Filter out NaN values
        let valid_values: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();

        if valid_values.is_empty() {
            return f64::NAN;
        }

        match self {
            Self::Average => {
                let sum: f64 = valid_values.iter().sum();
                sum / valid_values.len() as f64
            }
            Self::Min => valid_values
                .iter()
                .fold(f64::INFINITY, |acc, &v| acc.min(v)),
            Self::Max => valid_values
                .iter()
                .fold(f64::NEG_INFINITY, |acc, &v| acc.max(v)),
            Self::Last => *valid_values.last().unwrap(), // Safe because we checked non-empty
            Self::Sum => valid_values.iter().sum(),
            Self::Count => valid_values.len() as f64,
        }
    }
}

/// Label-based matcher for routing series to schemas.
///
/// A label matcher defines a set of required label key-value pairs. A time
/// series matches if all the matcher's labels are present in the series labels
/// with exactly matching values.
///
/// For the MVP, only exact string matching is supported. Regex matching may
/// be added as a future enhancement.
#[derive(Debug, Clone, PartialEq, Hash, Serialize, Deserialize)]
pub struct LabelMatcher {
    /// Required labels as key-value pairs.
    ///
    /// All labels in this map must be present in a series's labels with
    /// exactly matching values for the series to match this matcher.
    /// Uses BTreeMap for deterministic ordering in hash computation.
    labels: BTreeMap<String, String>,
}

impl LabelMatcher {
    /// Creates a new label matcher from key-value pairs.
    ///
    /// # Arguments
    ///
    /// * `labels` - Iterator of (key, value) pairs to match
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rondo::schema::LabelMatcher;
    ///
    /// let matcher = LabelMatcher::new([
    ///     ("service", "web"),
    ///     ("env", "prod"),
    /// ]);
    /// ```
    pub fn new<I, K, V>(labels: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            labels: labels
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// Creates an empty matcher that matches any series.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rondo::schema::LabelMatcher;
    ///
    /// let matcher = LabelMatcher::any();
    /// assert!(matcher.matches(&[("any".to_string(), "labels".to_string())]));
    /// ```
    pub fn any() -> Self {
        Self {
            labels: BTreeMap::new(),
        }
    }

    /// Checks if the given labels match this matcher.
    ///
    /// Returns `true` if all labels in this matcher are present in the
    /// provided labels with exactly matching values.
    ///
    /// # Arguments
    ///
    /// * `labels` - The labels to test for matching
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rondo::schema::LabelMatcher;
    ///
    /// let matcher = LabelMatcher::new([("service", "web")]);
    ///
    /// assert!(matcher.matches(&[("service".to_string(), "web".to_string()), ("env".to_string(), "prod".to_string())]));
    /// assert!(!matcher.matches(&[("service".to_string(), "api".to_string())]));
    /// assert!(!matcher.matches(&[("env".to_string(), "prod".to_string())]));
    /// ```
    pub fn matches(&self, labels: &[(String, String)]) -> bool {
        // Convert slice to HashMap for efficient lookups
        let label_map: HashMap<&str, &str> = labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Check that all required labels are present with matching values
        self.labels.iter().all(|(required_key, required_value)| {
            label_map
                .get(required_key.as_str())
                .map(|&value| value == required_value)
                .unwrap_or(false)
        })
    }

    /// Returns an iterator over the required labels.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rondo::schema::LabelMatcher;
    ///
    /// let matcher = LabelMatcher::new([("service", "web")]);
    /// for (key, value) in matcher.labels() {
    ///     println!("{key}={value}");
    /// }
    /// ```
    pub fn labels(&self) -> impl Iterator<Item = (&str, &str)> {
        self.labels.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Returns the number of required labels.
    pub fn label_count(&self) -> usize {
        self.labels.len()
    }

    /// Checks if this matcher requires no specific labels (matches anything).
    pub fn is_any(&self) -> bool {
        self.labels.is_empty()
    }
}

/// Serde support for Duration fields.
///
/// Durations are serialized as total seconds (f64) for human readability
/// in JSON configuration files.
mod duration_serde {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_secs_f64().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds = f64::deserialize(deserializer)?;
        Ok(Duration::from_secs_f64(seconds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consolidation_functions() {
        let values = [1.0, 2.0, f64::NAN, 4.0, 3.0];

        assert!((ConsolidationFn::Average.apply(&values) - 2.5).abs() < f64::EPSILON);
        assert_eq!(ConsolidationFn::Min.apply(&values), 1.0);
        assert_eq!(ConsolidationFn::Max.apply(&values), 4.0);
        assert_eq!(ConsolidationFn::Last.apply(&values), 3.0);
        assert_eq!(ConsolidationFn::Sum.apply(&values), 10.0);
        assert_eq!(ConsolidationFn::Count.apply(&values), 4.0);

        // Test all NaN
        let nan_values = [f64::NAN, f64::NAN];
        assert!(ConsolidationFn::Average.apply(&nan_values).is_nan());
        assert!(ConsolidationFn::Min.apply(&nan_values).is_nan());

        // Test empty
        assert!(ConsolidationFn::Average.apply(&[]).is_nan());
    }

    #[test]
    fn test_label_matcher() {
        let matcher = LabelMatcher::new([("service", "web"), ("env", "prod")]);

        // Should match when all labels are present
        assert!(matcher.matches(&[
            ("service".to_string(), "web".to_string()),
            ("env".to_string(), "prod".to_string()),
            ("extra".to_string(), "label".to_string()),
        ]));

        // Should not match when a label is missing
        assert!(!matcher.matches(&[("service".to_string(), "web".to_string())]));

        // Should not match when a label value differs
        assert!(!matcher.matches(&[
            ("service".to_string(), "api".to_string()),
            ("env".to_string(), "prod".to_string()),
        ]));

        // Any matcher should match anything
        let any_matcher = LabelMatcher::any();
        assert!(any_matcher.matches(&[]));
        assert!(any_matcher.matches(&[("any".to_string(), "label".to_string())]));
    }

    #[test]
    fn test_tier_config_validation() {
        // Valid config
        let config = TierConfig::new(
            Duration::from_secs(60),
            Duration::from_secs(3600),
            Some(ConsolidationFn::Average),
        );
        assert!(config.is_ok());

        // Invalid: zero interval
        let config = TierConfig::new(
            Duration::ZERO,
            Duration::from_secs(3600),
            Some(ConsolidationFn::Average),
        );
        assert!(config.is_err());

        // Invalid: retention < interval
        let config = TierConfig::new(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Some(ConsolidationFn::Average),
        );
        assert!(config.is_err());
    }

    #[test]
    fn test_schema_config_validation() {
        let valid_tiers = vec![
            TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(3600),
                consolidation_fn: None, // Highest res tier
            },
            TierConfig {
                interval: Duration::from_secs(60),
                retention: Duration::from_secs(86400),
                consolidation_fn: Some(ConsolidationFn::Average),
            },
        ];

        let config = SchemaConfig::new(
            "test".to_string(),
            LabelMatcher::any(),
            valid_tiers.clone(),
            1000,
        );
        assert!(config.is_ok());

        // Invalid: no tiers
        let config = SchemaConfig::new("test".to_string(), LabelMatcher::any(), vec![], 1000);
        assert!(config.is_err());

        // Invalid: consolidation on highest res tier
        let mut invalid_tiers = valid_tiers.clone();
        invalid_tiers[0].consolidation_fn = Some(ConsolidationFn::Average);
        let config =
            SchemaConfig::new("test".to_string(), LabelMatcher::any(), invalid_tiers, 1000);
        assert!(config.is_err());

        // Invalid: tiers not ordered
        let mut unordered_tiers = valid_tiers;
        unordered_tiers.reverse();
        let config = SchemaConfig::new(
            "test".to_string(),
            LabelMatcher::any(),
            unordered_tiers,
            1000,
        );
        assert!(config.is_err());
    }

    #[test]
    fn test_stable_hash() {
        let schema1 = SchemaConfig {
            name: "test1".to_string(),
            label_matcher: LabelMatcher::new([("service", "web")]),
            tiers: vec![TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(3600),
                consolidation_fn: None,
            }],
            max_series: 1000,
        };

        let schema2 = SchemaConfig {
            name: "test2".to_string(), // Different name
            label_matcher: LabelMatcher::new([("service", "web")]),
            tiers: vec![TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(3600),
                consolidation_fn: None,
            }],
            max_series: 1000,
        };

        // Names should not affect hash
        assert_eq!(schema1.stable_hash(), schema2.stable_hash());

        let schema3 = SchemaConfig {
            name: "test1".to_string(),
            label_matcher: LabelMatcher::new([("service", "api")]), // Different matcher
            tiers: vec![TierConfig {
                interval: Duration::from_secs(1),
                retention: Duration::from_secs(3600),
                consolidation_fn: None,
            }],
            max_series: 1000,
        };

        // Different matcher should affect hash
        assert_ne!(schema1.stable_hash(), schema3.stable_hash());
    }
}
