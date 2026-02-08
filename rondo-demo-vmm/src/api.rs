//! HTTP API for querying rondo metrics from the running VMM.
//!
//! Provides simple HTTP endpoints for health checks, store info,
//! and metric queries.
//!
//! This module is only compiled on Linux for the full VMM binary,
//! but the handler logic is platform-independent.

// TODO(phase4): Implement task 4.10 (HTTP query endpoint):
// - GET /metrics/query?series=...&start=...&end=...
// - GET /metrics/health
// - GET /metrics/info
