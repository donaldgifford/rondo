# rondo

Embedded round-robin time-series storage engine for VMMs and performance-critical systems.

## What is rondo?

rondo is a Rust library for high-performance, fixed-size time-series storage designed to be embedded directly in VMMs, dataplanes, and other systems software. Think rrdtool's storage model with a modern dimensional data model.

**Key properties:**

- **Zero-allocation write path** via memory-mapped ring buffers (~4ns per write)
- **Automatic tiered consolidation** — downsampling from 1s to 10s to 5min resolution
- **Bounded, predictable storage** — size is determined by configuration, not data volume
- **Dimensional labels** (key-value pairs) on every series
- **No background threads, no GC, no compaction surprises**

## Why embedded metrics?

Traditional monitoring stacks (Prometheus + exporters) use a pull/scrape model that fails for short-lived workloads:

| VM Lifetime | Embedded (1s) | Scrape (15s) | Scrape (30s) |
|-------------|---------------|--------------|--------------|
| 5 seconds   | 5 points (100%) | 0 points (0%) | 0 points (0%) |
| 30 seconds  | 30 points (100%) | 2 points (7%) | 1 point (3%) |
| 45 seconds  | 45 points (100%) | 3 points (7%) | 1 point (2%) |

Embedding the time-series store directly in the VMM captures every second of the VM lifecycle.

## Quick Start

```rust
use rondo::{Store, SchemaConfig, TierConfig, LabelMatcher};
use std::time::Duration;

// Define a schema: 1s resolution kept for 10 minutes
let schemas = vec![SchemaConfig {
    name: "vm_metrics".to_string(),
    label_matcher: LabelMatcher::any(),
    tiers: vec![TierConfig {
        interval: Duration::from_secs(1),
        retention: Duration::from_secs(600),
        consolidation_fn: None,
    }],
    max_series: 100,
}];

// Open or create a store
let mut store = Store::open("./my_metrics", schemas)?;

// Register a series
let cpu = store.register("cpu.usage", &[
    ("host".to_string(), "web1".to_string()),
])?;

// Record a value (zero-allocation hot path)
store.record(cpu, 85.5, 1_640_000_000_000_000_000)?;

// Query data back
let result = store.query(cpu, 0, 0, u64::MAX)?;
for (timestamp, value) in result {
    println!("{}: {}", timestamp, value);
}
```

## CLI

The `rondo-cli` crate provides a command-line tool for inspecting and querying stores:

```bash
# Show store metadata and series
rondo info ./my_metrics

# Query a series (CSV output)
rondo query ./my_metrics cpu.usage --range 1h --tier auto

# Query with JSON output
rondo query ./my_metrics cpu.usage --range 30m --format json

# Run write-path benchmark
rondo bench --points 10000000 --series 30
```

## Performance

| Method | p99 Latency | Throughput |
|--------|-------------|------------|
| rondo `record()` | ~42 ns | ~51M writes/s |
| `write()` syscall | ~3,291 ns | ~549K writes/s |
| UDP send (StatsD) | ~11,917 ns | ~195K writes/s |

rondo is **78x faster than file I/O** and **284x faster than network I/O** at p99 because `record()` is just a pointer write to mmap'd memory.

## Storage Model

rondo uses fixed-size ring buffers with columnar layout:

```
Store directory:
  meta.json              # Schema definitions
  schema_0/
    tier_0.slab          # 1s resolution, 10min retention (mmap'd)
    tier_1.slab          # 10s resolution, 6h retention
    tier_2.slab          # 5min resolution, 7d retention
```

Each slab file has a deterministic size based on `slot_count * (1 + max_series) * 8 + header`. A typical VMM schema with 30 series and 3 tiers uses ~1.5 MB total.

See [docs/storage-format.md](docs/storage-format.md) for the byte-level specification.

## Optional Features

- **`prometheus-remote-write`**: Adds a Prometheus remote-write client for pushing drained data to a remote TSDB. Requires `prost`, `reqwest`, and `snap` dependencies.

```toml
[dependencies]
rondo = { version = "0.0.1", features = ["prometheus-remote-write"] }
```

## Architecture

See [docs/architecture.md](docs/architecture.md) for the full architecture overview.

## Workspace

| Crate | Description |
|-------|-------------|
| `rondo` | Core library — store, ring buffers, consolidation, export |
| `rondo-cli` | CLI tool for inspection, queries, and benchmarks |
| `rondo-demo-vmm` | Minimal demo VMM with embedded metrics (Linux/KVM only) |

## License

MIT OR Apache-2.0
