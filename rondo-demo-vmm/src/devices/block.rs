//! Virtio-blk device implementation.
//!
//! Provides a minimal virtio block device backed by a file, with
//! rondo metrics instrumentation for I/O operations.
//!
//! This module is only compiled on Linux (requires KVM).

// TODO(phase4): Implement task 4.4 (virtio-blk):
// - Backing file for guest disk I/O
// - Wire into event loop for async I/O completion
// - Task 4.7 (virtio-blk instrumentation):
//   - Record blk_requests_total by operation type
//   - Record blk_request_duration_ns per request
//   - Record blk_bytes_total by direction
