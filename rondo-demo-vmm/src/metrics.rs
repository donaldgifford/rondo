//! VMM metrics integration with embedded rondo store.
//!
//! Provides a typed wrapper around rondo's `Store` with pre-registered series
//! handles for all VMM metrics. The `VmMetrics` struct exposes typed `record_*`
//! methods that map directly to `store.record()` — keeping the VMM hot path
//! minimal and allocation-free.

use std::path::Path;
use std::time::Duration;

use rondo::schema::{ConsolidationFn, LabelMatcher, SchemaConfig, TierConfig};
use rondo::series::SeriesHandle;
use rondo::store::Store;

/// Pre-registered series handles for all VMM metrics.
///
/// Created once at VMM startup. Each field is a `SeriesHandle` that can be
/// passed directly to `store.record()` on the hot path with zero allocation.
pub struct VmMetrics {
    /// The underlying rondo store.
    store: Store,

    // --- vCPU metrics (task 4.6) ---
    /// Total vCPU exits by reason: IO.
    pub vcpu_exits_io: SeriesHandle,
    /// Total vCPU exits by reason: MMIO.
    pub vcpu_exits_mmio: SeriesHandle,
    /// Total vCPU exits by reason: HLT.
    pub vcpu_exits_hlt: SeriesHandle,
    /// Total vCPU exits by reason: shutdown.
    pub vcpu_exits_shutdown: SeriesHandle,
    /// Total vCPU exits by reason: other.
    pub vcpu_exits_other: SeriesHandle,
    /// Duration of each vCPU exit in nanoseconds.
    pub vcpu_exit_duration_ns: SeriesHandle,
    /// Duration spent in KVM_RUN in nanoseconds.
    pub vcpu_run_duration_ns: SeriesHandle,

    // --- virtio-blk metrics (task 4.7) ---
    /// Total block requests: read.
    pub blk_requests_read: SeriesHandle,
    /// Total block requests: write.
    pub blk_requests_write: SeriesHandle,
    /// Total block requests: flush.
    pub blk_requests_flush: SeriesHandle,
    /// Duration of block requests in nanoseconds.
    pub blk_request_duration_ns: SeriesHandle,
    /// Total bytes read from block device.
    pub blk_bytes_read: SeriesHandle,
    /// Total bytes written to block device.
    pub blk_bytes_written: SeriesHandle,

    // --- VMM process metrics (task 4.8) ---
    /// Resident set size in bytes.
    pub vmm_rss_bytes: SeriesHandle,
    /// Number of open file descriptors.
    pub vmm_open_fds: SeriesHandle,
    /// VMM uptime in seconds.
    pub vmm_uptime_seconds: SeriesHandle,
}

impl VmMetrics {
    /// Creates a new `VmMetrics` instance, opening the rondo store and
    /// registering all VMM metric series.
    ///
    /// # Schema Design
    ///
    /// - Tier 0: 1s interval, 10min retention (raw high-res data)
    /// - Tier 1: 10s interval, 6h retention (consolidated average)
    /// - Tier 2: 5min interval, 7d retention (consolidated average)
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened or series registration fails.
    pub fn open<P: AsRef<Path>>(store_path: P) -> rondo::Result<Self> {
        let schemas = vec![SchemaConfig {
            name: "vmm".to_string(),
            label_matcher: LabelMatcher::any(),
            tiers: vec![
                TierConfig::new(Duration::from_secs(1), Duration::from_secs(600), None)?,
                TierConfig::new(
                    Duration::from_secs(10),
                    Duration::from_secs(21600),
                    Some(ConsolidationFn::Average),
                )?,
                TierConfig::new(
                    Duration::from_secs(300),
                    Duration::from_secs(604800),
                    Some(ConsolidationFn::Average),
                )?,
            ],
            max_series: 30,
        }];

        let mut store = Store::open(store_path, schemas)?;

        // Register vCPU exit metrics
        let vcpu_exits_io = store.register(
            "vcpu_exits_total",
            &[("reason".to_string(), "io".to_string())],
        )?;
        let vcpu_exits_mmio = store.register(
            "vcpu_exits_total",
            &[("reason".to_string(), "mmio".to_string())],
        )?;
        let vcpu_exits_hlt = store.register(
            "vcpu_exits_total",
            &[("reason".to_string(), "hlt".to_string())],
        )?;
        let vcpu_exits_shutdown = store.register(
            "vcpu_exits_total",
            &[("reason".to_string(), "shutdown".to_string())],
        )?;
        let vcpu_exits_other = store.register(
            "vcpu_exits_total",
            &[("reason".to_string(), "other".to_string())],
        )?;
        let vcpu_exit_duration_ns = store.register("vcpu_exit_duration_ns", &[])?;
        let vcpu_run_duration_ns = store.register("vcpu_run_duration_ns", &[])?;

        // Register virtio-blk metrics
        let blk_requests_read = store.register(
            "blk_requests_total",
            &[("op".to_string(), "read".to_string())],
        )?;
        let blk_requests_write = store.register(
            "blk_requests_total",
            &[("op".to_string(), "write".to_string())],
        )?;
        let blk_requests_flush = store.register(
            "blk_requests_total",
            &[("op".to_string(), "flush".to_string())],
        )?;
        let blk_request_duration_ns = store.register("blk_request_duration_ns", &[])?;
        let blk_bytes_read = store.register(
            "blk_bytes_total",
            &[("direction".to_string(), "read".to_string())],
        )?;
        let blk_bytes_written = store.register(
            "blk_bytes_total",
            &[("direction".to_string(), "write".to_string())],
        )?;

        // Register VMM process metrics
        let vmm_rss_bytes = store.register("vmm_rss_bytes", &[])?;
        let vmm_open_fds = store.register("vmm_open_fds", &[])?;
        let vmm_uptime_seconds = store.register("vmm_uptime_seconds", &[])?;

        Ok(Self {
            store,
            vcpu_exits_io,
            vcpu_exits_mmio,
            vcpu_exits_hlt,
            vcpu_exits_shutdown,
            vcpu_exits_other,
            vcpu_exit_duration_ns,
            vcpu_run_duration_ns,
            blk_requests_read,
            blk_requests_write,
            blk_requests_flush,
            blk_request_duration_ns,
            blk_bytes_read,
            blk_bytes_written,
            vmm_rss_bytes,
            vmm_open_fds,
            vmm_uptime_seconds,
        })
    }

    /// Records a vCPU exit event.
    ///
    /// # Errors
    ///
    /// Returns an error if the write to the ring buffer fails.
    pub fn record_vcpu_exit(
        &mut self,
        reason: VcpuExitReason,
        exit_duration_ns: f64,
        run_duration_ns: f64,
        timestamp_ns: u64,
    ) -> rondo::Result<()> {
        let handle = match reason {
            VcpuExitReason::Io => self.vcpu_exits_io,
            VcpuExitReason::Mmio => self.vcpu_exits_mmio,
            VcpuExitReason::Hlt => self.vcpu_exits_hlt,
            VcpuExitReason::Shutdown => self.vcpu_exits_shutdown,
            VcpuExitReason::Other => self.vcpu_exits_other,
        };

        self.store.record(handle, 1.0, timestamp_ns)?;
        self.store
            .record(self.vcpu_exit_duration_ns, exit_duration_ns, timestamp_ns)?;
        self.store
            .record(self.vcpu_run_duration_ns, run_duration_ns, timestamp_ns)?;

        Ok(())
    }

    /// Records a virtio-blk request.
    ///
    /// # Errors
    ///
    /// Returns an error if the write to the ring buffer fails.
    pub fn record_blk_request(
        &mut self,
        op: BlkOp,
        duration_ns: f64,
        bytes: f64,
        timestamp_ns: u64,
    ) -> rondo::Result<()> {
        let (req_handle, bytes_handle) = match op {
            BlkOp::Read => (self.blk_requests_read, self.blk_bytes_read),
            BlkOp::Write => (self.blk_requests_write, self.blk_bytes_written),
            BlkOp::Flush => (self.blk_requests_flush, self.blk_bytes_read), // flush has no bytes
        };

        self.store.record(req_handle, 1.0, timestamp_ns)?;
        self.store
            .record(self.blk_request_duration_ns, duration_ns, timestamp_ns)?;
        if bytes > 0.0 {
            self.store.record(bytes_handle, bytes, timestamp_ns)?;
        }

        Ok(())
    }

    /// Records VMM process-level metrics.
    ///
    /// # Errors
    ///
    /// Returns an error if the write to the ring buffer fails.
    pub fn record_process_stats(
        &mut self,
        rss_bytes: f64,
        open_fds: f64,
        uptime_seconds: f64,
        timestamp_ns: u64,
    ) -> rondo::Result<()> {
        self.store
            .record(self.vmm_rss_bytes, rss_bytes, timestamp_ns)?;
        self.store
            .record(self.vmm_open_fds, open_fds, timestamp_ns)?;
        self.store
            .record(self.vmm_uptime_seconds, uptime_seconds, timestamp_ns)?;

        Ok(())
    }

    /// Runs consolidation on all tiers.
    ///
    /// Should be called on a 1-second timer tick from the event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if consolidation fails.
    pub fn consolidate(&mut self) -> rondo::Result<usize> {
        self.store.consolidate()
    }

    /// Returns a reference to the underlying store for queries and export.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Returns a mutable reference to the underlying store.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }
}

/// vCPU exit reasons.
#[derive(Debug, Clone, Copy)]
pub enum VcpuExitReason {
    /// I/O port access.
    Io,
    /// Memory-mapped I/O access.
    Mmio,
    /// HLT instruction.
    Hlt,
    /// Shutdown request.
    Shutdown,
    /// Any other exit reason.
    Other,
}

/// Block device operation types.
#[derive(Debug, Clone, Copy)]
pub enum BlkOp {
    /// Read operation.
    Read,
    /// Write operation.
    Write,
    /// Flush/sync operation.
    Flush,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_vm_metrics_open_and_register() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("metrics");

        let metrics = VmMetrics::open(&store_path).unwrap();

        // Verify all 16 series were registered
        let handles = metrics.store().handles();
        assert_eq!(handles.len(), 16);
    }

    #[test]
    fn test_record_vcpu_exit() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("metrics");

        let mut metrics = VmMetrics::open(&store_path).unwrap();
        let ts = 1_700_000_000_000_000_000u64;

        metrics
            .record_vcpu_exit(VcpuExitReason::Io, 500.0, 10_000.0, ts)
            .unwrap();
        metrics
            .record_vcpu_exit(VcpuExitReason::Hlt, 200.0, 5_000.0, ts + 1_000_000_000)
            .unwrap();

        // Query back the IO exit counter
        let result = metrics.store().query(metrics.vcpu_exits_io, 0, ts, ts + 1).unwrap();
        let points: Vec<_> = result.collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].1, 1.0);
    }

    #[test]
    fn test_record_blk_request() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("metrics");

        let mut metrics = VmMetrics::open(&store_path).unwrap();
        let ts = 1_700_000_000_000_000_000u64;

        metrics
            .record_blk_request(BlkOp::Read, 1000.0, 4096.0, ts)
            .unwrap();
        metrics
            .record_blk_request(BlkOp::Write, 2000.0, 8192.0, ts + 1_000_000_000)
            .unwrap();

        // Query back the read bytes
        let result = metrics.store().query(metrics.blk_bytes_read, 0, ts, ts + 1).unwrap();
        let points: Vec<_> = result.collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].1, 4096.0);
    }

    #[test]
    fn test_record_process_stats() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("metrics");

        let mut metrics = VmMetrics::open(&store_path).unwrap();
        let ts = 1_700_000_000_000_000_000u64;

        metrics
            .record_process_stats(50_000_000.0, 42.0, 3600.0, ts)
            .unwrap();

        let result = metrics.store().query(metrics.vmm_rss_bytes, 0, ts, ts + 1).unwrap();
        let points: Vec<_> = result.collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].1, 50_000_000.0);
    }

    #[test]
    fn test_consolidation_tick() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("metrics");

        let mut metrics = VmMetrics::open(&store_path).unwrap();

        // Write some data
        let base_ts = 1_700_000_000_000_000_000u64;
        for i in 0u32..20 {
            let ts = base_ts + u64::from(i) * 1_000_000_000;
            metrics
                .record_vcpu_exit(VcpuExitReason::Io, f64::from(i * 100), f64::from(i * 1000), ts)
                .unwrap();
        }

        // Consolidation should succeed (may or may not downsample yet depending on tier capacity)
        let count = metrics.consolidate().unwrap();
        // Consolidation succeeds (count may be 0 if tier 0 hasn't wrapped)
        let _ = count;
    }

    #[test]
    fn test_reopen_store_preserves_series() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("metrics");

        let ts = 1_700_000_000_000_000_000u64;

        // Open and record
        {
            let mut metrics = VmMetrics::open(&store_path).unwrap();
            metrics
                .record_process_stats(100_000.0, 10.0, 1.0, ts)
                .unwrap();
        }

        // Reopen and query
        {
            let metrics = VmMetrics::open(&store_path).unwrap();
            let result = metrics.store().query(metrics.vmm_rss_bytes, 0, ts, ts + 1).unwrap();
            let points: Vec<_> = result.collect();
            assert_eq!(points.len(), 1);
            assert_eq!(points[0].1, 100_000.0);
        }
    }
}
