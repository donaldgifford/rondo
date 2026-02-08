# Storage Format Specification

This document describes the byte-level format of rondo slab files.

## Store Directory Layout

```
<store_path>/
  meta.json                    # JSON: schema configs + hashes
  series_index.bin             # JSON: registered series metadata
  consolidation_cursors.json   # JSON: consolidation progress per tier
  schema_0/
    tier_0.slab                # Highest resolution ring buffer
    tier_1.slab                # Consolidated mid-resolution
    tier_2.slab                # Consolidated low-resolution
  schema_1/
    tier_0.slab
    ...
```

## Slab File Format

Each `.slab` file is a fixed-size, memory-mapped file containing a ring buffer of time-series data in columnar layout.

### Header (64 bytes)

| Offset | Size | Type    | Field         | Description                          |
|--------|------|---------|---------------|--------------------------------------|
| 0      | 4    | `[u8;4]`| magic         | `b"RNDO"` — file type identifier    |
| 4      | 4    | `u32`   | version       | Format version (currently `1`)       |
| 8      | 8    | `u64`   | schema_hash   | Stable hash of the schema config     |
| 16     | 4    | `u32`   | slot_count    | Number of time slots in ring buffer  |
| 20     | 4    | `u32`   | max_series    | Maximum number of series columns     |
| 24     | 8    | `u64`   | interval_ns   | Sample interval in nanoseconds       |
| 32     | 4    | `u32`   | write_cursor  | Current write position (slot index)  |
| 36     | 4    | `u32`   | series_count  | Number of registered series          |
| 40     | 24   | `[u8]`  | _reserved     | Zero-filled, reserved for future use |

All multi-byte fields are stored in **native endianness** (the file is not portable across architectures, by design — it's ephemeral per-host storage).

### Series Directory

Immediately after the header, at offset 64:

| Offset         | Size | Type  | Description                          |
|----------------|------|-------|--------------------------------------|
| 64 + i*4       | 4    | `u32` | Column offset for series `i`         |

Size: `max_series * 4` bytes.

Each entry maps a series ID to its column index in the data region. Currently this is an identity mapping (series 0 → column 0, etc.) but the indirection allows future series reordering.

### Data Region

After the series directory, starting at offset `64 + max_series * 4`:

The data region uses **columnar layout**. All timestamps are stored contiguously, followed by all values for series 0, then all values for series 1, etc.

```
┌─────────────────────────────────────────────────────────────┐
│ Timestamp Column (slot_count * 8 bytes)                     │
│ [ts_0] [ts_1] [ts_2] ... [ts_{N-1}]                        │
├─────────────────────────────────────────────────────────────┤
│ Series 0 Value Column (slot_count * 8 bytes)                │
│ [v0_0] [v0_1] [v0_2] ... [v0_{N-1}]                        │
├─────────────────────────────────────────────────────────────┤
│ Series 1 Value Column (slot_count * 8 bytes)                │
│ [v1_0] [v1_1] [v1_2] ... [v1_{N-1}]                        │
├─────────────────────────────────────────────────────────────┤
│ ... (one column per max_series)                             │
└─────────────────────────────────────────────────────────────┘
```

Each slot occupies:
- Timestamp: 8 bytes (`u64`, nanoseconds since Unix epoch)
- Value: 8 bytes (`f64`, IEEE 754 double)

### Total File Size

```
file_size = 64                            # header
          + max_series * 4                 # series directory
          + slot_count * 8                 # timestamp column
          + slot_count * max_series * 8    # value columns
```

For a typical VMM schema (600 slots, 30 series):
```
64 + 30*4 + 600*8 + 600*30*8 = 64 + 120 + 4800 + 144000 = 148,984 bytes (~145 KB)
```

## Slot Computation

Given a timestamp in nanoseconds:

```
slot = (timestamp_ns / interval_ns) % slot_count
```

This maps any timestamp to a fixed slot, enabling O(1) writes. When a new write lands on a slot that already has data, the old data is silently overwritten (round-robin behavior).

## Sentinel Values

- **Unwritten timestamp**: `0` (zero)
- **Missing/unwritten value**: `NaN` (`f64::NAN`)

Slots that have never been written contain `0` for the timestamp and `NaN` for the value. Query operations skip these sentinel values.

## Write Cursor

The `write_cursor` field in the header tracks the most recently written slot. It advances monotonically (modulo `slot_count`) and is used to determine:

- Whether the ring has wrapped (cursor has exceeded `slot_count`)
- Where to start reading for consolidation
- The oldest valid data in the ring

## Consolidation Cursors

Stored in `consolidation_cursors.json`:

```json
{
  "0:1": 1700000000000000000,
  "0:2": 1700000000000000000
}
```

Key format: `"{schema_index}:{tier_index}"`, value: last consolidated timestamp.

## Export Cursors

Stored in a separate JSON file per export destination:

```json
{
  "cursors": {
    "0:0:0": 1700000000000000000,
    "0:0:1": 1700000000000000000
  }
}
```

Key format: `"{schema_index}:{tier_index}:{column}"`, value: last exported timestamp.
