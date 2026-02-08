//! # rondo
//!
//! Embedded round-robin time-series storage engine.
//!
//! rondo is a Rust library for high-performance, fixed-size time-series storage
//! designed to be embedded directly in VMMs, dataplanes, and other
//! performance-critical systems software. Think rrdtool's storage philosophy
//! with a modern dimensional data model.
//!
//! **Status**: This crate is in early development. The API is not yet stable.
//!
//! ## Key Properties
//!
//! - Zero-allocation write path via memory-mapped ring buffers
//! - Automatic tiered consolidation (downsampling) at write time
//! - Bounded, predictable storage — size is determined by configuration, not data volume
//! - Dimensional labels (key-value pairs) on every series
//! - No background threads, no GC, no compaction surprises
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use rondo::{Store, SchemaConfig, TierConfig, LabelMatcher, SeriesHandle};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Define a schema: 1s resolution kept for 10 minutes
//! let schemas = vec![SchemaConfig {
//!     name: "vm_metrics".to_string(),
//!     label_matcher: LabelMatcher::any(),
//!     tiers: vec![TierConfig {
//!         interval: Duration::from_secs(1),
//!         retention: Duration::from_secs(600),
//!         consolidation_fn: None,
//!     }],
//!     max_series: 100,
//! }];
//!
//! // Open or create a store
//! let mut store = Store::open("./my_metrics", schemas)?;
//!
//! // Register a series
//! let cpu = store.register("cpu.usage", &[
//!     ("host".to_string(), "web1".to_string()),
//! ])?;
//!
//! // Record a value (zero-allocation hot path)
//! store.record(cpu, 85.5, 1_640_000_000_000_000_000)?;
//!
//! // Query data back
//! let result = store.query(cpu, 0, 0, u64::MAX)?;
//! for (timestamp, value) in result {
//!     println!("{}: {}", timestamp, value);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Architecture
//!
//! - [`Store`] — Top-level handle; opens a directory, owns schemas and series
//! - [`SchemaConfig`] — Defines retention tiers and consolidation for a class of metrics
//! - [`SeriesHandle`] — Opaque, `Copy` handle for zero-alloc writes
//! - [`QueryResult`] — Lazy iterator with tier metadata
//!
//! ## Modules
//!
//! For lower-level access, the individual modules are also public:
//!
//! - [`store`] — Store lifecycle, record, query
//! - [`schema`] — Schema, tier, and consolidation configuration
//! - [`series`] — Series registration and handle management
//! - [`ring`] — Ring buffer implementation over memory-mapped slabs
//! - [`slab`] — Raw memory-mapped file format
//! - [`query`] — Query result types and tier selection
//! - [`error`] — Error types

pub mod error;
pub mod query;
pub mod ring;
pub mod schema;
pub mod series;
pub mod slab;
pub mod store;

// Re-export primary API types at crate root for convenience.
pub use error::{Result, RondoError};
pub use query::QueryResult;
pub use schema::{ConsolidationFn, LabelMatcher, SchemaConfig, TierConfig};
pub use series::SeriesHandle;
pub use store::Store;
