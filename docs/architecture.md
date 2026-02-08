# Architecture

rondo is an embedded round-robin time-series storage engine designed for VMMs and performance-critical systems.

## Core Concepts

### Store

The `Store` is the top-level entry point. It manages a directory containing slab files, schema metadata, and series registrations.

```
my_metrics/
  meta.json                  # Schema definitions + hashes
  series_index.bin           # Registered series (name, labels, column)
  consolidation_cursors.json # Tier consolidation progress
  schema_0/
    tier_0.slab              # High-resolution ring buffer (mmap'd)
    tier_1.slab              # Mid-resolution consolidated data
    tier_2.slab              # Low-resolution long-term data
```

### Schemas

A schema defines a class of metrics with shared retention policy. Each schema contains one or more tiers, where each tier has:

- **Interval**: How often data points are recorded (e.g., 1s)
- **Retention**: How long data is kept (e.g., 10 minutes)
- **Consolidation function**: How higher-res data is downsampled (avg, min, max, sum, count, last)

Example: a VMM metrics schema with 3 tiers:
- Tier 0: 1s interval, 10min retention (600 slots) — raw data
- Tier 1: 10s interval, 6h retention (2160 slots) — averaged
- Tier 2: 5min interval, 7d retention (2016 slots) — averaged

### Series

A series is a single time-series identified by a name and a set of key-value labels:

```
vcpu_exits_total{reason="io"}
blk_bytes_total{direction="read"}
vmm_rss_bytes{}
```

Series are registered once at startup. Registration returns a `SeriesHandle` — a small, `Copy` struct containing pre-computed column offsets for zero-allocation writes.

### Ring Buffers

Each (schema, tier) pair has a ring buffer backed by a memory-mapped file (slab). The ring buffer stores data in columnar layout:

```
| Timestamp column | Series 0 values | Series 1 values | ... |
| slot_0 ts        | slot_0 val      | slot_0 val      |     |
| slot_1 ts        | slot_1 val      | slot_1 val      |     |
| ...              | ...             | ...             |     |
```

When the ring wraps, oldest data is overwritten. This guarantees bounded, predictable storage.

## Data Flow

### Write Path (Hot)

```
store.record(handle, value, timestamp)
  └─> ring_buffer.write(column, value, timestamp)
        └─> compute slot = (timestamp / interval) % slot_count
            write timestamp to mmap[ts_offset + slot * 8]
            write value to mmap[val_offset + slot * 8]
```

No heap allocation. No syscall. Just pointer arithmetic and mmap writes. Measured at ~4ns per write.

### Consolidation

```
store.consolidate()
  └─> for each schema:
        for each tier pair (source → dest):
          scan source ring for data since last consolidation cursor
          group source points into destination-interval windows
          apply consolidation function (avg, min, max, etc.)
          write consolidated values to destination ring
          update cursor
```

Consolidation is called explicitly (typically on a 1s timer). It cascades: tier 0 → tier 1, tier 1 → tier 2, etc.

### Query Path

```
store.query(handle, tier, start, end)
  └─> ring_buffer.read(column, start, end)
        └─> compute start_slot, end_slot from timestamps
            iterate slots, skip NaN values
            return (timestamp, value) pairs

store.query_auto(handle, start, end)
  └─> find highest-resolution tier covering the requested range
      fall back to lower tiers for longer ranges
```

### Export Path

```
store.drain(&mut cursor, tier)
  └─> for each series in schema:
        read all points since cursor position
        advance cursor to newest timestamp
        return SeriesExport { handle, points }

// Optional: push to Prometheus
remote_write::push(&config, &exports, &store)
  └─> build WriteRequest protobuf
      compress with snappy
      POST to endpoint with retry
```

## Crate Structure

```
rondo/                  # Core library
  src/
    store.rs            # Store: open, record, query, consolidate, drain
    schema.rs           # SchemaConfig, TierConfig, ConsolidationFn
    series.rs           # SeriesHandle, SeriesRegistry
    ring.rs             # RingBuffer: read, write, wraparound
    slab.rs             # Slab: mmap file format, header, data access
    query.rs            # QueryResult, tier selection
    consolidate.rs      # ConsolidationEngine, cursor management
    export.rs           # ExportCursor, drain_series, drain_tier
    remote_write.rs     # Prometheus remote-write (feature-gated)
    error.rs            # Error types
    lib.rs              # Public API re-exports

rondo-cli/              # CLI tool
  src/main.rs           # info, query, bench subcommands

rondo-demo-vmm/         # Demo VMM (Linux-only for KVM)
  src/
    main.rs             # CLI + VMM entry point
    metrics.rs          # VmMetrics wrapper with typed record methods
    vmm.rs              # KVM VM creation + boot (Linux)
    vcpu.rs             # vCPU thread + exit handling (Linux)
    devices/block.rs    # virtio-blk device (Linux)
    api.rs              # HTTP metrics endpoint (Linux)
```
