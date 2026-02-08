# Benchmarks

## Write-Path Latency (Benchmark A)

Comparison of rondo `record()` against common alternatives, measuring p50/p99/p999 latency over 10M iterations.

| Method                 | p50 (ns) | p99 (ns) | p999 (ns) | Throughput     |
|------------------------|----------|----------|-----------|----------------|
| rondo `record()`       | ~0       | ~42      | ~84       | ~51M writes/s  |
| atomic `counter.inc()` | ~41      | ~42      | ~84       | ~45M writes/s  |
| `write()` syscall      | ~1500    | ~3291    | ~9041     | ~549K writes/s |
| UDP send (StatsD-like) | ~4583    | ~11917   | ~30334    | ~195K writes/s |

### Analysis

rondo's `record()` is an mmap pointer write — no heap allocation, no syscall. At p99 it matches raw atomic operations and is **78x faster than file write()** and **284x faster than UDP send**.

The p50 of ~0ns means the write often completes below the timer's resolution (~41ns on this hardware). The actual instruction count is minimal: compute slot offset, write 16 bytes (8 timestamp + 8 value) to mmap'd memory.

### Methodology

- Hardware: Apple Silicon (M-series)
- Release build with LTO
- 100K warmup iterations before measurement
- Latency measured per-operation using `Instant::now()`
- Statistics computed from sorted latency array

Run with: `cargo run -p rondo --release --example benchmark_comparison`

## Ephemeral VM Data Capture (Benchmark C)

Comparison of data points captured for short-lived VM workloads.

| VM Lifetime | Embedded (1s) | Scrape (15s) | Scrape (30s) |
|-------------|---------------|--------------|--------------|
| 5 seconds   | 5 (100%)      | 0 (0%)       | 0 (0%)       |
| 10 seconds  | 10 (100%)     | 0 (0%)       | 0 (0%)       |
| 30 seconds  | 30 (100%)     | 2 (6.7%)     | 1 (3.3%)     |
| 45 seconds  | 45 (100%)     | 3 (6.7%)     | 1 (2.2%)     |

### Analysis

For VMs that live less than the scrape interval (5s, 10s), traditional scrape-based monitoring captures **zero data points**. The VM starts, runs, and shuts down between scrapes, losing all telemetry.

Embedded rondo captures **every second** of the VM's lifecycle — startup spike, steady state, and shutdown — because recording happens inline with the VMM's event loop, not via an external poller.

Even for VMs that live longer than the scrape interval (30s, 45s), scrape captures only 3-7% of the data that embedded recording captures.

### Methodology

- Simulated VM lifecycles with configurable duration
- Embedded: 1s recording interval, records every tick
- Scrape: average of best/worst case timing alignment
- No actual VMs — focuses on the mathematical data capture difference

Run with: `cargo run -p rondo --release --example ephemeral_vm_benchmark`

## Resource Overhead at Scale (Benchmark B)

Measured resource usage of 10/50/100 concurrent KVM VMs with embedded rondo metrics on a single host, compared against estimated Prometheus + node-exporter stack overhead.

### Results

| VMs | Success | Peak RSS | Store Disk | Extra Processes | Prom Stack (est.) |
|-----|---------|----------|------------|-----------------|-------------------|
| 10  | 10/10   | 1,072 MB | 11 MB      | 0               | 380 MB            |
| 50  | 50/50   | 5,129 MB | 57 MB      | 0               | 1,500 MB          |
| 100 | 100/100 | 7,123 MB | 114 MB     | 0               | 2,900 MB          |

Rondo store disk usage is consistent at **1.1 MB per VM** (16 series × 3 tiers).

### Prometheus Stack Estimates

Per-VM overhead for traditional monitoring:
- **node-exporter**: ~25 MB RSS per instance
- **Prometheus server**: ~100 MB base + ~3 MB per scrape target
- **Network**: ~50 kB per scrape × N targets / 15s interval

| VMs | node-exporters | Prometheus | Total est. |
|-----|---------------|------------|------------|
| 10  | 250 MB        | 130 MB     | 380 MB     |
| 50  | 1,250 MB      | 250 MB     | 1,500 MB   |
| 100 | 2,500 MB      | 400 MB     | 2,900 MB   |

### Analysis

Embedded rondo eliminates the per-VM node-exporter process entirely. Each VMM writes metrics directly to mmap'd slab files — the monitoring overhead is the store's mmap region (~1.1 MB disk per VM for 16 series × 3 tiers). No separate exporter process, no network scrape traffic, no Prometheus server scaling with target count.

At 100 VMs, the traditional Prometheus stack would add **101 extra processes** and an estimated **2.9 GB of additional RSS** purely for monitoring. Rondo adds **zero processes** and **zero network overhead** — metrics recording is embedded in the VMM's existing event loop.

Peak RSS includes KVM guest memory pages faulted in by the hypervisor (128 MiB allocated per VM, ~71-107 MB faulted in depending on workload). This memory is inherent to running the VM regardless of monitoring approach.

### Methodology

- Hardware: 8 vCPU, 15.6 GB RAM, Linux 6.12 (Debian 13)
- `scripts/benchmark_scale.sh` spawns N concurrent `rondo-demo-vmm` instances
- Staggered launch: 100ms per VM + 500ms every 10th to avoid KVM I/O storms
- Resource sampling via `/proc/PID/status` (VmRSS) every 2 seconds
- Each VMM runs a 15-second guest workload (4-phase: CPU burst, idle, I/O, mixed)
- All 100 VMs booted, ran workload, and shut down within 44 seconds
- Prometheus estimates based on published node-exporter and Prometheus resource profiles

Run with: `make vmm-bench-scale`

## Criterion Microbenchmarks

Detailed criterion benchmarks for the `record()` hot path:

```
cargo bench -p rondo -- record
```

Benchmark groups:
- `record/single_series` — Single series write latency
- `record/series_count/{1,10,30,100}` — Scaling with series count
- `record_batch/series_count/{1,10,30,100}` — Batch write performance
- `record/30_series_throughput` — Realistic VMM workload throughput
