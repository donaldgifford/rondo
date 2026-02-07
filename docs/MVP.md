# Embedded Time-Series Storage Engine — MVP Plan

## Project Overview

This document defines the minimum viable product for an embedded, round-robin time-series storage engine designed for performance-critical systems software. The MVP will consist of two deliverables:

1. **The library** — a Rust crate providing zero-allocation write-path time-series storage with automatic tiered consolidation
2. **The demo VMM** — a minimal microVM monitor built on rust-vmm that embeds the library and demonstrates the value proposition against a traditional externalized monitoring stack

The VMM is not the product. The library is the product. The VMM exists to provide a credible, real-world integration that makes the performance and architectural advantages undeniable.

---

## Part 1: The Storage Engine (the library)

### Core Abstractions

The library has four foundational concepts:

**Schema** — A declaration of how a class of metrics should be stored. Defines resolution tiers (intervals), retention durations, and consolidation functions. Schemas are matched to incoming series by label patterns.

**Series** — A unique time-series identified by a name and a set of key-value labels. Every series is bound to exactly one schema. Example: `vcpu_steal_ns{instance="vm-abc", vcpu="0"}`.

**Slab** — The physical storage unit. A memory-mapped file containing a fixed-size columnar ring buffer for all series sharing a schema. Slabs are pre-allocated at creation time and never grow.

**Tier** — A resolution level within a schema. Each tier has an interval (e.g., 1s, 10s, 5m) and a duration (e.g., 10m, 6h, 7d). When a high-resolution tier's ring buffer wraps, evicted data is consolidated into the next tier using the configured aggregation functions.

### Storage Layout

```
Store Directory
├── meta.json                     # store metadata, schema definitions
├── series_index.bin              # label set → series ID mapping
├── schema_0/                     # one directory per schema
│   ├── tier_0.slab               # highest resolution (e.g., 1s × 10m = 600 slots)
│   ├── tier_1.slab               # mid resolution (e.g., 10s × 6h = 2,160 slots)
│   └── tier_2.slab               # low resolution (e.g., 5m × 7d = 2,016 slots)
└── schema_1/
    ├── tier_0.slab
    └── tier_1.slab
```

Each slab file is a flat, memory-mapped structure:

```
┌────────────────────────────────────────────────┐
│ Slab Header (64 bytes)                         │
│   magic, version, schema_hash, slot_count,     │
│   series_count, interval_ns, write_cursor      │
├────────────────────────────────────────────────┤
│ Series Directory (fixed-size array)            │
│   [series_id → column_offset] × max_series     │
├────────────────────────────────────────────────┤
│ Ring Buffer Data                               │
│   Column 0: timestamps (u64 nanos)             │
│   Column 1: series 0 values (f64)              │
│   Column 2: series 1 values (f64)              │
│   ...                                          │
│   Column N: series N-1 values (f64)            │
└────────────────────────────────────────────────┘
```

The columnar layout means reading a time range for a single series touches contiguous memory. The ring buffer means writes never move data — they overwrite the oldest slot. The fixed size means the file can be pre-allocated and fallocate'd at creation, eliminating filesystem fragmentation.

### MVP API Surface

```rust
/// Open or create a store at the given path with the provided schemas.
/// Schemas are immutable after creation — changing them requires migration.
pub fn Store::open(path: &Path, schemas: &[SchemaConfig]) -> Result<Store>;

/// Register a new series. Returns a SeriesHandle for fast subsequent writes.
/// This is NOT on the hot path — it's called once per series at startup or
/// on first encounter. It may allocate and take a lock.
pub fn Store::register(
    &self,
    name: &str,
    labels: &[(&str, &str)],
) -> Result<SeriesHandle>;

/// Record a value for a previously registered series.
/// THIS IS THE HOT PATH.
/// Contract: no heap allocation, no syscall, no contested lock.
/// Writes to a pre-computed slot in the mmap'd ring buffer.
/// Timestamp is typically Instant::now() but can be caller-provided
/// for replay or testing.
pub fn Store::record(
    &self,
    handle: &SeriesHandle,
    value: f64,
    timestamp: u64,
) -> Result<()>;

/// Record multiple values atomically (same timestamp, multiple series).
/// Useful for VMM exit handlers that update several counters at once.
pub fn Store::record_batch(
    &self,
    entries: &[(&SeriesHandle, f64)],
    timestamp: u64,
) -> Result<()>;

/// Run one consolidation pass. Checks all tiers for wrapped data and
/// consolidates into the next tier. Designed to be called from the
/// application's own event loop or maintenance tick — not a background thread.
/// Returns the number of consolidations performed.
pub fn Store::consolidate(&self) -> Result<usize>;

/// Query a series over a time range at a specific tier.
/// Returns an iterator over (timestamp, value) pairs.
pub fn Store::query(
    &self,
    handle: &SeriesHandle,
    tier: usize,
    start: u64,
    end: u64,
) -> Result<impl Iterator<Item = (u64, f64)>>;

/// Query with automatic tier selection — picks the highest-resolution
/// tier that covers the requested time range.
pub fn Store::query_auto(
    &self,
    handle: &SeriesHandle,
    start: u64,
    end: u64,
) -> Result<impl Iterator<Item = (u64, f64)>>;

/// Drain consolidated data from a tier for upstream export.
/// Returns all points since the given cursor and advances the cursor.
/// Designed for periodic push to a remote TSDB.
pub fn Store::drain(
    &self,
    tier: usize,
    cursor: &mut ExportCursor,
) -> Result<Vec<(SeriesHandle, Vec<(u64, f64)>)>>;
```

### Schema Configuration

```rust
pub struct SchemaConfig {
    /// Label matcher — series matching this pattern use this schema.
    /// Supports exact match and regex on label values.
    pub matcher: LabelMatcher,

    /// Resolution tiers, ordered from highest to lowest resolution.
    /// Each tier defines an interval and a retention duration.
    pub tiers: Vec<TierConfig>,

    /// Maximum number of series this schema can hold.
    /// Determines the pre-allocated slab size. Pick a power of 2.
    pub max_series: u32,
}

pub struct TierConfig {
    /// Sample interval for this tier.
    pub interval: Duration,

    /// How long data is retained at this resolution.
    pub duration: Duration,

    /// Consolidation functions applied when rolling data from the
    /// previous (higher-resolution) tier into this one.
    /// Not applicable to the highest-resolution tier.
    pub consolidation: Vec<ConsolidationFn>,
}

pub enum ConsolidationFn {
    Average,
    Min,
    Max,
    Last,
    Sum,
    Count,
    // P50, P90, P99 are stretch goals — they require more storage
    // per slot (a digest rather than a scalar).
}
```

### MVP Scope — What's In, What's Out

**In scope for MVP:**

- Store creation with schema definitions
- Series registration with label sets
- Zero-alloc record() on the hot path via mmap'd ring buffers
- Automatic tiered consolidation (avg, min, max, last)
- Time-range queries at any tier
- Drain/export interface for upstream push
- Prometheus remote-write export (push consolidated tiers to an existing Prometheus/Victoria)
- Basic CLI tool for inspection (`rondo info`, `rondo query`, `rondo dump`)
- Benchmarks proving the write-path performance claims (ns-scale latency, no allocations)

**Out of scope for MVP (future work):**

- PromQL query engine (use the export-to-Prometheus path for querying in v1)
- OTLP ingest/export (Prometheus remote-write is sufficient for v1)
- Server mode / networked query interface
- C FFI and language bindings (Rust-only for v1 since the VMM is also Rust)
- Percentile consolidation functions (require t-digest or DDSketch storage)
- Dynamic schema migration
- Clustering or replication

---

## Part 2: The Demo VMM

### Why rust-vmm

The VMM is a demonstration vehicle, not a product. We need it to be:

1. Real enough to produce genuine metrics (actual vCPU exits, actual virtio I/O)
2. Simple enough that the TSDB integration is the interesting part, not the VMM itself
3. Built in Rust so the library integration is native with no FFI overhead

Building a VMM from scratch gives maximum control but means reimplementing well-understood components (KVM ioctl wrappers, virtio device models, memory management) that rust-vmm already provides as tested crates. For an MVP where the VMM is the demo, not the product, rust-vmm is the right call.

If the project evolves to the point where the VMM itself becomes a product (like the Nexus work), we can replace the demo VMM with the real one. The library's API is VMM-agnostic by design.

### rust-vmm Crates We'll Use

| Crate | Purpose |
|-------|---------|
| `kvm-ioctls` | KVM API wrappers for VM/vCPU creation and control |
| `kvm-bindings` | KVM struct definitions |
| `vm-memory` | Guest memory management (GuestMemoryMmap) |
| `vm-superio` | Legacy i8042/serial device emulation |
| `linux-loader` | Loading bzImage/ELF kernels into guest memory |
| `virtio-queue` | Virtio queue implementation |
| `virtio-blk` | Virtio block device (for disk I/O metrics) |
| `event-manager` | Epoll-based event loop |

### VMM Architecture

The demo VMM boots a minimal Linux guest (custom initramfs with BusyBox) and runs a synthetic workload that generates measurable metrics across CPU, memory, and I/O dimensions.

```
┌──────────────────────────────────────────────────────┐
│                    Demo VMM Process                   │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │              Event Loop (epoll)                  │ │
│  │                                                   │ │
│  │  vCPU Thread(s) ──→ KVM_RUN ──→ exit handler    │ │
│  │       │                              │            │ │
│  │       │    on each exit:             │            │ │
│  │       │    store.record(             │            │ │
│  │       │      exit_type,              │            │ │
│  │       │      duration_ns             │            │ │
│  │       │    )                          │            │ │
│  │       │                              │            │ │
│  │  virtio-blk handler ──→ on I/O complete:         │ │
│  │       │                  store.record(            │ │
│  │       │                    blk_latency_ns,        │ │
│  │       │                    queue_depth             │ │
│  │       │                  )                         │ │
│  │       │                                           │ │
│  │  Maintenance Tick (1s) ──→ store.consolidate()   │ │
│  │                          ──→ export_if_due()      │ │
│  └─────────────────────────────────────────────────┘ │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │         Embedded TSDB (rondo)                   │ │
│  │                                                   │ │
│  │  Schema: vmm_metrics                              │ │
│  │  Tiers: 1s/10m, 10s/6h, 5m/7d                   │ │
│  │  Series: ~20-30 per VM                            │ │
│  │  Total storage: ~2 MB per VM (deterministic)      │ │
│  └─────────────────────────────────────────────────┘ │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │         HTTP API (minimal)                        │ │
│  │                                                   │ │
│  │  GET /metrics/query?series=...&range=...          │ │
│  │  GET /metrics/health                              │ │
│  │  GET /metrics/info (store stats)                  │ │
│  └─────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────┘
```

### Metrics the VMM Will Produce

These are real metrics from KVM/virtio, not synthetic data:

**vCPU metrics (per vCPU):**

- `vcpu_exits_total` — counter of total KVM exits, labeled by exit reason (MMIO, PIO, HLT, etc.)
- `vcpu_exit_duration_ns` — histogram/last of time spent handling each exit
- `vcpu_run_duration_ns` — time spent in KVM_RUN before each exit (guest execution time)
- `vcpu_steal_ns` — approximation of time the vCPU thread was not scheduled by the host

**Memory metrics:**

- `guest_memory_bytes` — total guest memory allocated
- `memory_slot_count` — number of KVM memory slots

**Block I/O metrics (if virtio-blk is included):**

- `blk_requests_total` — counter by operation type (read, write, flush)
- `blk_request_duration_ns` — latency per request
- `blk_queue_depth` — current virtio queue depth
- `blk_bytes_total` — throughput counter by direction

**VMM process metrics:**

- `vmm_rss_bytes` — resident set size of the VMM process
- `vmm_open_fds` — file descriptor count
- `vmm_uptime_seconds` — time since boot

### Guest Workload

The guest runs a minimal Linux with a BusyBox initramfs. The init script runs a simple workload that generates varied I/O and CPU patterns:

```sh
#!/bin/sh
# Generate CPU load with varying intensity
while true; do
    # Burst: heavy computation for 5 seconds
    dd if=/dev/urandom of=/dev/null bs=1M count=50 2>/dev/null
    # Idle: sleep for 2 seconds
    sleep 2
    # I/O: write to virtio-blk device
    dd if=/dev/zero of=/dev/vda bs=4k count=1000 2>/dev/null
    sleep 1
done
```

This creates a repeating pattern of CPU bursts, idle periods, and I/O activity that produces visually interesting time-series data when graphed — it is obvious at a glance that the embedded store is capturing real workload dynamics.

### Building the Guest

```bash
# Kernel: minimal config, KVM guest support, virtio drivers
make defconfig
scripts/config --enable VIRTIO_BLK
scripts/config --enable VIRTIO_NET
scripts/config --enable VIRTIO_PCI
scripts/config --enable KVM_GUEST
scripts/config --disable MODULES
make bzImage -j$(nproc)

# Initramfs: BusyBox static build
mkdir -p initramfs/{bin,dev,proc,sys}
cp busybox initramfs/bin/
ln -s busybox initramfs/bin/sh
# ... create init script ...
cd initramfs && find . | cpio -o -H newc | gzip > ../initramfs.gz
```

---

## Part 3: The Benchmark — Proving the Point

The MVP must include a head-to-head comparison that quantifies the advantage of the embedded approach versus traditional externalized monitoring. This is what makes the project compelling beyond "cool tech."

### Benchmark A: Write-Path Overhead

Measure the cost of recording a metric value inside the VMM's vCPU exit handler.

| Approach | What We Measure |
|----------|----------------|
| **Embedded (rondo)** | Time for `store.record()` — single mmap'd write |
| **Prometheus client (in-process)** | Time for `counter.inc()` / `histogram.observe()` — in-memory with allocator |
| **StatsD (UDP push)** | Time for serializing and sending a UDP packet to localhost |
| **Direct file write** | Time for `write()` syscall to a log file |

Expected outcome: rondo is 10-100x faster than alternatives because it avoids allocation, serialization, and syscalls.

Metric: p50/p99 latency in nanoseconds, measured over 10 million writes.

### Benchmark B: Resource Overhead at Scale

Run N microVMs on a single host, each producing 30 metrics at 1-second intervals.

**Scenario 1: Embedded monitoring**

- Each VMM has an embedded rondo store
- Host agent reads 10s tier from each store, exports 5m rollups to remote Prometheus
- Measure: total CPU%, memory, disk I/O consumed by monitoring

**Scenario 2: Traditional monitoring**

- Prometheus node-exporter or custom exporter per VM (or DaemonSet equivalent)
- Central Prometheus scraping all exporters at 15s intervals
- Thanos sidecar for longer retention
- Measure: total CPU%, memory, disk I/O, network bandwidth consumed by monitoring

Expected outcome: embedded approach uses an order of magnitude less CPU and memory, eliminates network overhead for high-resolution data, and provides higher resolution at the point of decision.

### Benchmark C: Data Availability for Ephemeral VMs

Boot a microVM, run a workload for 30 seconds, shut it down.

- **Embedded**: query the local store after shutdown — all 30 seconds of 1s data is available
- **Traditional (15s scrape)**: at most 2 data points captured, possibly zero if scrape timing misaligns with VM lifecycle

This one is less about performance and more about correctness — the traditional model systematically loses data from short-lived workloads.

---

## Part 4: Deliverables and Milestones

### Milestone 1: Storage Engine Core (Weeks 1-3)

- [ ] Slab file format implementation (create, mmap, read/write slots)
- [ ] Series registration with label indexing
- [ ] `record()` hot path — zero-alloc mmap write with ring buffer semantics
- [ ] `record_batch()` for multi-series atomic writes
- [ ] Time-range query over a single tier
- [ ] Unit tests for ring buffer wraparound, slot computation, data integrity
- [ ] Microbenchmark: `record()` latency (target: < 50ns p99)

### Milestone 2: Tiered Consolidation (Weeks 3-4)

- [ ] Consolidation engine: avg, min, max, last, sum, count
- [ ] `consolidate()` function driven by caller's event loop
- [ ] Automatic tier cascade (tier 0 wraps → consolidate into tier 1 → ...)
- [ ] Query with automatic tier selection
- [ ] Integration tests: write at 1s for 15 minutes, verify 10s and 5m tiers are populated correctly

### Milestone 3: Export and CLI (Weeks 4-5)

- [ ] `drain()` interface for pulling consolidated data for upstream push
- [ ] Prometheus remote-write client (push 5m rollups to a remote Prometheus)
- [ ] CLI tool: `rondo info <store_path>` — print schema, series count, storage usage
- [ ] CLI tool: `rondo query <store_path> <series> --range 1h` — dump data as CSV or JSON
- [ ] CLI tool: `rondo bench` — run the write-path microbenchmark standalone

### Milestone 4: Demo VMM (Weeks 5-7)

- [ ] Minimal rust-vmm VMM: boot bzImage + initramfs, single vCPU, serial console
- [ ] Add virtio-blk device with a backing file
- [ ] Instrument vCPU exit handler with `store.record()`
- [ ] Instrument virtio-blk handler with latency and throughput metrics
- [ ] Add VMM process metrics (RSS, FDs, uptime)
- [ ] 1-second maintenance tick for `consolidate()` and export
- [ ] HTTP endpoint for querying local store (`/metrics/query`)
- [ ] Build guest kernel and initramfs with synthetic workload

### Milestone 5: Benchmarks and Documentation (Weeks 7-8)

- [ ] Benchmark A: write-path overhead comparison
- [ ] Benchmark B: resource overhead at scale (10, 50, 100 VMs)
- [ ] Benchmark C: ephemeral VM data availability
- [ ] README with architecture overview and getting started
- [ ] Blog post / technical write-up with benchmark results
- [ ] Grafana dashboard showing data exported from embedded stores

---

## Part 5: Project Structure

```
rondo/
├── Cargo.toml                    # workspace root
├── README.md
│
├── rondo/                       # the library crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                # public API
│       ├── store.rs              # Store implementation
│       ├── slab.rs               # mmap'd slab file management
│       ├── ring.rs               # ring buffer logic
│       ├── series.rs             # series registration and label index
│       ├── schema.rs             # schema definition and matching
│       ├── consolidate.rs        # consolidation engine
│       ├── query.rs              # time-range query
│       ├── export.rs             # drain and remote-write client
│       └── bench.rs              # internal benchmarks
│
├── rondo-cli/                   # CLI tool
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
│
├── rondo-demo-vmm/              # demo VMM
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs               # VMM entry point
│   │   ├── vmm.rs                # VM setup (memory, vCPU, devices)
│   │   ├── vcpu.rs               # vCPU thread and exit handler
│   │   ├── devices/
│   │   │   ├── serial.rs         # serial console
│   │   │   └── block.rs          # virtio-blk with instrumentation
│   │   ├── metrics.rs            # rondo integration and schema setup
│   │   └── api.rs                # minimal HTTP query endpoint
│   └── guest/
│       ├── build.sh              # kernel + initramfs build script
│       ├── kernel.config         # minimal kernel config
│       └── init.sh               # guest init script with workload
│
├── benchmarks/                   # comparative benchmarks
│   ├── write_overhead/
│   ├── scale_test/
│   └── ephemeral_vm/
│
└── docs/
    ├── architecture.md
    ├── storage-format.md
    └── benchmarks.md
```

---

## Part 6: Naming

The project is named **rondo**. In music, a rondo is a compositional form built on a principal theme that recurs — a theme that keeps coming back around. This maps directly to the round-robin storage model where data cycles through fixed-size ring buffers.

- **Crate name**: `rondo` (available on crates.io)
- **CLI**: `rondo-cli`
- **Demo VMM**: `rondo-demo-vmm`
- **Repository**: `rondo`

The name is short, memorable, and unlikely to collide with other projects in the systems or observability space.

---

## Part 7: Open Questions

These need decisions before or during implementation:

**1. Single-writer or multi-writer?**
The MVP assumes single-writer (one thread calls `record()`). This is correct for the VMM use case where the vCPU thread is the writer. Multi-writer (multiple threads recording concurrently) requires either per-thread ring buffers or atomic operations on the write cursor. Decision: **single-writer for MVP**, multi-writer as a future enhancement.

**2. What happens when max_series is reached?**
Options: (a) return an error from `register()`, (b) evict the least-recently-written series, (c) dynamically grow the slab. Decision: **(a) return an error** for MVP. Fixed size is a feature, not a limitation. The operator should size their schema appropriately.

**3. Should consolidation be synchronous or async?**
The `consolidate()` call could block (scan all tiers, consolidate in-place) or return a future. For the VMM use case, calling it from a 1-second maintenance tick in the event loop is fine — the work per tick is minimal (at most a few hundred slot reads and writes). Decision: **synchronous for MVP**.

**4. NaN/missing data handling?**
rrdtool uses NaN to represent unknown values. This is important for the round-robin model — if no write occurs in a slot's time window, what value does it hold? Options: (a) NaN sentinel, (b) separate validity bitmap, (c) require the caller to always write. Decision: **(a) NaN sentinel** — it's what rrdtool did and it works well with f64 storage.

**5. Endianness and portability?**
The slab format uses native endianness for maximum write-path performance (no byte swapping). This means slab files are not portable across architectures. Decision: **native endian for MVP**. Cross-architecture portability is not a use case for embedded stores — the store lives and dies with the process that created it.

**6. Should the HTTP endpoint serve Prometheus exposition format?**
Having the demo VMM expose `/metrics` in Prometheus format would let people point an existing Prometheus at it for familiarity. But this somewhat undermines the "you don't need to scrape" message. Decision: **yes, include it** — it's a small addition and it makes the demo more accessible. The benchmarks will show that you *can* scrape it but you *don't need to*.

---

## Success Criteria

The MVP is successful if it demonstrates the following:

1. **Write-path performance**: `record()` completes in under 50ns at p99 with zero heap allocations, measured by benchmarks and verified by profiling.

2. **Predictable storage**: Total disk usage for a VMM running 30 series across 3 tiers is deterministic and under 2 MB, with no growth over time regardless of how long the VMM runs.

3. **Data fidelity**: A microVM that runs for 45 seconds has 45 data points at 1-second resolution available for query after shutdown. The traditional scrape model captures 0-3 points for the same VM.

4. **Resource efficiency**: At 100 VMs per host, the embedded approach uses less than 10% of the CPU and memory consumed by an equivalent Prometheus + exporter stack.

5. **Integration simplicity**: Adding monitoring to the demo VMM requires fewer than 50 lines of code in the VMM's hot path, with no external dependencies, no sidecar processes, and no network configuration.

If these five things are proven, the project has a compelling story to tell.
