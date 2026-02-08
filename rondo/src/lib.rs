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
//! ## Use Cases
//!
//! - Embedding in custom VMMs (rust-vmm, Cloud Hypervisor, Firecracker agents)
//! - Host-level metrics with local retention and consolidated upstream export
//! - Edge and IoT environments where running a full TSDB is disproportionate
//! - Anywhere you need the SQLite of time-series storage

pub mod error;
pub mod ring;
pub mod schema;
pub mod series;
pub mod slab;
