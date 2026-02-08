# rondo — MVP Implementation Guide

Primary path: **Plan A (Demo VMM)** with fallback to Plan B (Firecracker) or Plan C (Cloud Hypervisor) if VMM engineering becomes a bottleneck.

The library (Phases 1-3) is identical across all plans. Only Phase 4 changes if we switch.

---

## Overall Success Criteria

| # | Criterion | Measurement | Target |
|---|-----------|-------------|--------|
| S1 | Write-path performance | `record()` p99 latency over 10M writes | < 50ns |
| S2 | Zero allocations | Custom allocator guard in benchmark | 0 heap allocs on `record()` |
| S3 | Predictable storage | Disk usage for 30 series × 3 tiers | Deterministic, < 2 MB, no growth over time |
| S4 | Data fidelity | 45-second VM lifecycle → query afterward | 45 data points at 1s resolution |
| S5 | Resource efficiency | 100 VMs per host, embedded vs Prometheus+exporter | < 10% of CPU/memory of traditional stack |
| S6 | Integration simplicity | Lines of code in VMM hot path | < 50 lines |

---

## Phase 1: Storage Engine Foundation

**Goal**: Slab file format, mmap lifecycle, ring buffer, and the zero-alloc `record()` hot path.

### Tasks

- [x] **1.1** Define core types in `rondo/src/schema.rs`: `SchemaConfig`, `TierConfig`, `ConsolidationFn`, `LabelMatcher`
- [x] **1.2** Implement slab file format in `rondo/src/slab.rs`
  - 64-byte header: magic bytes, version, schema hash, slot count, series count, interval_ns, write cursor
  - Series directory: fixed-size array mapping series ID → column offset
  - Create + open with `memmap2` (mmap the entire file)
  - Pre-allocate file to exact calculated size on creation
- [x] **1.3** Implement ring buffer logic in `rondo/src/ring.rs`
  - Slot computation from timestamp + interval
  - Write cursor advancement with wraparound
  - Columnar layout: timestamp column + one f64 column per series
  - NaN sentinel for missing/unwritten slots
- [x] **1.4** Implement series registration in `rondo/src/series.rs`
  - `SeriesHandle` — opaque handle containing pre-computed column offset
  - Label set storage and matching against schema matchers
  - Series index persistence (`series_index.bin`)
  - Return error when `max_series` is reached
- [x] **1.5** Implement `Store` in `rondo/src/store.rs`
  - `Store::open(path, schemas)` — create or open a store directory, initialize slabs per schema/tier
  - `Store::register(name, labels)` → `SeriesHandle`
  - `Store::record(handle, value, timestamp)` — **the hot path**: write to mmap'd slot, no alloc, no syscall
  - `Store::record_batch(entries, timestamp)` — multi-series write at same timestamp
  - `meta.json` read/write for schema persistence
- [x] **1.6** Implement basic query in `rondo/src/query.rs`
  - `Store::query(handle, tier, start, end)` → iterator over `(u64, f64)` pairs
  - Handle ring buffer wraparound in read path
  - Skip NaN entries
- [x] **1.7** Wire up `rondo/src/lib.rs` — public API re-exports, crate-level docs
- [x] **1.8** Unit tests
  - Ring buffer: wraparound, slot computation, boundary conditions
  - Slab: create/open/reopen lifecycle, mmap integrity
  - Series: registration, duplicate detection, max_series limit
  - Store: open/record/query round-trip
  - NaN handling in all read paths
- [ ] **1.9** Microbenchmark: `record()` latency
  - Criterion benchmark in `rondo/benches/record.rs`
  - Measure p50/p99/p999 over 10M writes
  - Zero-allocation verification with custom global allocator

### Phase 1 Acceptance

| Check | Criteria |
|-------|----------|
| [ ] | `Store::open` creates store directory with correct slab files |
| [ ] | `Store::record` writes to mmap without allocation (verified by benchmark) |
| [ ] | `Store::query` returns all written data for a time range |
| [ ] | Ring buffer correctly wraps and overwrites oldest data |
| [ ] | NaN sentinel present in unwritten slots |
| [ ] | `record()` benchmark shows < 50ns p99 |
| [ ] | All unit tests pass |

---

## Phase 2: Tiered Consolidation

**Goal**: Automatic downsampling when high-res tiers wrap, with configurable aggregation functions.

### Tasks

- [ ] **2.1** Implement consolidation functions in `rondo/src/consolidate.rs`
  - `Average`, `Min`, `Max`, `Last`, `Sum`, `Count`
  - Each operates on a slice of `f64` values (with NaN filtering)
- [ ] **2.2** Implement `Store::consolidate()` in `store.rs`
  - Scan tier 0 for newly wrapped regions since last consolidation
  - Apply configured consolidation functions to produce tier 1 values
  - Cascade: tier 1 wraps → consolidate into tier 2, etc.
  - Track consolidation cursors per tier (persisted in slab header or meta.json)
  - Return count of consolidations performed
- [ ] **2.3** Implement `Store::query_auto(handle, start, end)`
  - Select highest-resolution tier that covers the requested time range
  - Fall back to lower tiers for ranges that exceed higher-tier retention
- [ ] **2.4** Integration tests
  - Write at 1s for 15 simulated minutes → verify tier 1 (10s) has correct consolidated values
  - Write past tier 1 capacity → verify tier 2 (5m) cascade
  - Verify consolidation functions produce mathematically correct results
  - Verify `query_auto` selects the right tier for various ranges

### Phase 2 Acceptance

| Check | Criteria |
|-------|----------|
| [ ] | `consolidate()` correctly downsamples tier 0 → tier 1 when tier 0 wraps |
| [ ] | Cascade works: tier 0 → tier 1 → tier 2 |
| [ ] | All consolidation functions (avg, min, max, last, sum, count) produce correct output |
| [ ] | NaN values in source tier are excluded from consolidation |
| [ ] | `query_auto` returns highest-resolution data available for the requested range |
| [ ] | Integration tests pass with 15 minutes of simulated 1s writes |

---

## Phase 3: Export and CLI

**Goal**: Data export for upstream push and a CLI for inspection and debugging.

### Tasks

- [ ] **3.1** Implement `Store::drain()` in `rondo/src/export.rs`
  - `drain(tier, cursor)` → returns all points since cursor, advances cursor
  - `ExportCursor` type with persistence support
  - Designed for periodic push to remote TSDB
- [ ] **3.2** Implement Prometheus remote-write client in `rondo/src/export.rs`
  - Serialize drain output to Prometheus remote-write protobuf format
  - HTTP POST to configurable endpoint
  - Basic retry logic
- [ ] **3.3** CLI: `rondo info <store_path>` in `rondo-cli/`
  - Print schemas, series count, tier slot usage, total disk size
  - Show consolidation cursor positions
- [ ] **3.4** CLI: `rondo query <store_path> <series> --range 1h --tier auto`
  - Output as CSV or JSON (flag-controlled)
  - Support both explicit tier and auto tier selection
- [ ] **3.5** CLI: `rondo bench`
  - Standalone write-path microbenchmark
  - Creates a temp store, writes 10M points, reports latency percentiles
  - Verifies zero allocations
- [ ] **3.6** Wire CLI argument parsing with clap derive

### Phase 3 Acceptance

| Check | Criteria |
|-------|----------|
| [ ] | `drain()` returns correct data and advances cursor |
| [ ] | Repeated `drain()` calls return only new data since last call |
| [ ] | Prometheus remote-write successfully pushes to a test Prometheus instance |
| [ ] | `rondo info` displays accurate store metadata |
| [ ] | `rondo query` returns correct data in CSV and JSON formats |
| [ ] | `rondo bench` completes 10M writes and reports latency |

---

## Phase 4: Demo VMM (Plan A)

**Goal**: Minimal rust-vmm VMM that boots a Linux guest and produces real KVM/virtio metrics via embedded rondo.

> **Fallback**: If VMM engineering is taking too long, switch to Plan B (Firecracker agent) or Plan C (Cloud Hypervisor fork). The library from Phases 1-3 is unchanged.

### Tasks

- [ ] **4.1** Set up `rondo-demo-vmm` crate with rust-vmm dependencies
  - `kvm-ioctls`, `kvm-bindings`, `vm-memory`, `linux-loader`, `vm-superio`, `event-manager`
  - Add crate to workspace members
- [ ] **4.2** Implement minimal VMM boot in `vmm.rs`
  - Create KVM VM, configure memory regions via `vm-memory`
  - Set up CPU ID, MSRs, special registers for x86_64 boot
  - Load bzImage kernel via `linux-loader`
  - Load initramfs into guest memory
- [ ] **4.3** Implement vCPU thread in `vcpu.rs`
  - KVM_RUN loop with exit handling (IO, MMIO, HLT, shutdown)
  - Serial console output via `vm-superio`
- [ ] **4.4** Add virtio-blk device in `devices/block.rs`
  - Backing file for guest disk I/O
  - Wire into event loop for async I/O completion
- [ ] **4.5** Integrate rondo in `metrics.rs`
  - Initialize `Store` with VMM metrics schema (1s/10m, 10s/6h, 5m/7d tiers)
  - Register series for all VMM metrics (~20-30 series)
  - Provide `VmMetrics` wrapper with typed `record_*` methods
- [ ] **4.6** Instrument vCPU exit handler
  - Record `vcpu_exits_total` by exit reason
  - Record `vcpu_exit_duration_ns` per exit
  - Record `vcpu_run_duration_ns` (time in KVM_RUN)
- [ ] **4.7** Instrument virtio-blk handler
  - Record `blk_requests_total` by operation type
  - Record `blk_request_duration_ns` per request
  - Record `blk_bytes_total` by direction
- [ ] **4.8** Add VMM process metrics
  - `vmm_rss_bytes` via `/proc/self/status`
  - `vmm_open_fds` via `/proc/self/fd`
  - `vmm_uptime_seconds`
- [ ] **4.9** Add maintenance tick to event loop
  - 1-second timer calling `store.consolidate()`
  - Optional export on configurable interval
- [ ] **4.10** Add HTTP query endpoint in `api.rs`
  - `GET /metrics/query?series=...&start=...&end=...`
  - `GET /metrics/health`
  - `GET /metrics/info`
- [ ] **4.11** Build guest kernel and initramfs
  - Minimal kernel config: KVM guest, virtio drivers, no modules
  - BusyBox initramfs with synthetic workload (CPU bursts + disk I/O + idle periods)
  - Build script in `rondo-demo-vmm/guest/build.sh`

### Phase 4 Acceptance

| Check | Criteria |
|-------|----------|
| [ ] | Demo VMM boots a Linux guest to serial console |
| [ ] | vCPU exit metrics are recorded into rondo store |
| [ ] | virtio-blk I/O metrics are recorded |
| [ ] | `consolidate()` runs on 1s tick without blocking the event loop |
| [ ] | HTTP API returns queryable metric data |
| [ ] | Guest workload produces visually distinct patterns (bursts, idle, I/O) |
| [ ] | Total rondo overhead in VMM is < 50 lines in hot path |

---

## Phase 5: Benchmarks and Documentation

**Goal**: Head-to-head comparisons proving the embedded advantage, plus documentation.

### Tasks

- [ ] **5.1** Benchmark A: Write-path overhead comparison
  - rondo `record()` vs Prometheus client `counter.inc()` vs StatsD UDP push vs direct `write()` syscall
  - p50/p99 latency over 10M iterations
- [ ] **5.2** Benchmark B: Resource overhead at scale
  - 10, 50, 100 VMs per host
  - Embedded rondo vs Prometheus + exporter stack
  - Measure CPU%, memory, disk I/O, network bandwidth
- [ ] **5.3** Benchmark C: Ephemeral VM data capture
  - 30-second and 45-second VM lifecycles
  - Compare data points captured: embedded (100%) vs 15s scrape (0-3 points)
- [ ] **5.4** Grafana dashboard
  - Showing data exported from embedded stores via remote-write
  - Side-by-side with traditional scrape for visual comparison
- [ ] **5.5** Documentation
  - README update: architecture overview, quickstart, usage examples
  - `docs/architecture.md`: storage model, data flow diagrams
  - `docs/storage-format.md`: byte-level slab format specification
  - `docs/benchmarks.md`: methodology and results

### Phase 5 Acceptance

| Check | Criteria |
|-------|----------|
| [ ] | Benchmark A shows rondo 10-100x faster than alternatives |
| [ ] | Benchmark B shows < 10% CPU/memory of Prometheus stack at 100 VMs |
| [ ] | Benchmark C shows 100% data capture for 45s VM vs < 10% for scrape model |
| [ ] | Grafana dashboard renders exported data correctly |
| [ ] | All overall success criteria (S1-S6) are met |

---

## Phase Dependency Graph

```
Phase 1 (Foundation)
    │
    ├──→ Phase 2 (Consolidation)
    │        │
    │        ├──→ Phase 3 (Export + CLI)
    │        │        │
    │        │        └──→ Phase 5 (Benchmarks + Docs)
    │        │
    │        └──→ Phase 4 (Demo VMM)
    │                 │
    │                 └──→ Phase 5 (Benchmarks + Docs)
    │
    └──→ Phase 1.9 (Microbenchmark — can validate S1/S2 early)
```

Phase 1 is the critical path. Phase 2 and early Phase 3 work (drain, CLI info/query) can overlap with Phase 4 setup. Phase 5 depends on both Phase 3 and Phase 4.

---

## Fallback Decision Points

**After Phase 1**: If `record()` benchmark does not meet < 50ns target, investigate before proceeding. The slab/ring design may need revision.

**During Phase 4**: If VMM engineering is consuming disproportionate effort:
- **Switch to Plan B (Firecracker agent)**: Replace tasks 4.1-4.4 and 4.11 with: Firecracker FIFO parser, per-VM store lifecycle, epoll loop, VM discovery. Keep tasks 4.5-4.10 (metrics integration, HTTP API) largely unchanged.
- **Switch to Plan C (Cloud Hypervisor fork)**: Fork CH, add rondo dependency, instrument `vcpu_run()` and virtio handlers. ~300-500 lines of diff against upstream.

The library is the product. The VMM integration proves it works. If one integration path is too hard, switch to another — the library code doesn't change.

---

## Code Quality Gates

Use the `rust-development` skills throughout implementation to enforce idiomatic Rust and catch issues early.

### Architecture and Design (use `rust-development:rust-architect`)

Invoke **before writing code** for each new module or significant type:
- Validate ownership model for `Store`, `Slab`, `SeriesHandle` — ensure borrows are minimal on the hot path
- Review trait design: whether `ConsolidationFn` should be an enum vs trait object vs function pointer (enum is preferred for this project — no dynamic dispatch on hot path)
- Evaluate error type hierarchy: `thiserror` enum per module vs single crate-level error type
- Review API surface before stabilizing: lifetime elision, builder patterns where appropriate, `&self` vs `&mut self` for `record()` (should be `&self` with interior write to mmap)

### Performance Review (use `rust-development:rust-performance`)

Invoke at key performance checkpoints:
- **After task 1.5** (`record()` implementation): Profile for hidden allocations, unnecessary bounds checks, suboptimal mmap access patterns. Verify the write path compiles to minimal instructions.
- **After task 1.9** (microbenchmark): If < 50ns target is missed, use this to identify bottlenecks — cache line contention, branch misprediction, unnecessary pointer indirection.
- **After task 2.2** (`consolidate()`): Review consolidation loop for vectorization opportunities, unnecessary copies, and allocation in the scan path.
- **During Phase 5** (benchmarks): Analyze flamegraphs and criterion output for regression or optimization opportunities.

### Code Review (use `rust-development:review`)

Invoke **after completing each task** before moving to the next:
- Idiomatic patterns: proper use of `Result`, `Option`, iterator chains, slice operations
- Safety: all `unsafe` blocks have `// SAFETY:` comments, minimal unsafe surface area, sound abstractions over raw mmap pointers
- Error handling: no `.unwrap()` in library code (only in tests/benches), `?` propagation, descriptive error variants
- API ergonomics: consistent naming, sensible defaults, no surprising behavior

### Test Generation (use `rust-development:test`)

Invoke **when writing tests** for each module:
- Unit tests co-located in each module (`#[cfg(test)] mod tests`)
- Integration tests in `rondo/tests/` for cross-module flows
- Property-based tests with `proptest` for storage invariants (ring buffer correctness, consolidation math, no stale reads after wraparound)
- Doc tests on all public API functions showing typical usage

### Scaffolding (use `rust-development:scaffold`)

Invoke **once at Phase 4 start** to scaffold the `rondo-demo-vmm` crate with proper structure, ensuring it follows the same workspace patterns (edition, lints, dependency inheritance) as the existing crates.
