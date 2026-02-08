# Benchmark Readiness Plan

## Context

The VMM demo runs end-to-end but `rondo-cli query` returns 0 points (the `--range` always computes relative to wall-clock time, missing the store data). The workload is hardcoded at ~18s but we need 15/30/45s variants for Benchmark C (ephemeral VM data capture). Scale benchmarking (Benchmark B) needs investigation and the remote box has Prometheus + Grafana available for integration testing.

## Changes

### 1. Fix CLI `--range all` support

**File:** `rondo-cli/src/main.rs`

- Add `"all"` as a special value in `parse_duration()` — returns `u64::MAX`
- When range is `u64::MAX`, set `start_ns = 0` and `end_ns = u64::MAX` instead of computing from wall-clock time
- This lets `rondo query vmm_metrics vcpu_exits_total --range all` dump all data regardless of timestamps

### 2. Make guest workload duration tunable via kernel cmdline

**Files:**
- `rondo-demo-vmm/guest/workload.sh` — accept `$WORKLOAD_DURATION` env var, default to 18s if not set, distribute phases proportionally
- `rondo-demo-vmm/guest/init` — parse `/proc/cmdline` for `workload_duration=N` parameter, export as env var before calling workload.sh

**Duration distribution** (proportional to current 18s = 5+3+5+5):
- Phase 1 (CPU burst): 28% of total
- Phase 2 (idle): 17% of total
- Phase 3 (I/O sim): 28% of total
- Phase 4 (mixed): 28% of total

For 45s that gives: 13s CPU + 8s idle + 12s I/O + 12s mixed = 45s

The VMM already supports `--cmdline` override, so no VMM code changes needed — just pass `workload_duration=45` in the cmdline string.

### 3. Add Makefile benchmark targets

**File:** `Makefile`

Add targets that combine guest build + VMM run + post-run query for specific durations:
- `vmm-bench-15` — 15-second VM lifecycle
- `vmm-bench-30` — 30-second VM lifecycle
- `vmm-bench-45` — 45-second VM lifecycle
- `vmm-bench-capture` — runs all three and reports capture rates

Each target passes the appropriate `workload_duration=N` via the `--cmdline` flag, then queries the store with `--range all` to count data points.

### 4. Update `vmm-demo` target

**File:** `Makefile`

Update the existing `vmm-demo` target to use `--range all` in the post-run query so it actually shows data.

### 5. Scale benchmark investigation (Benchmark B)

**Remote environment:** The remote box (10.10.11.33) has Prometheus and Grafana available.

**Open questions for Benchmark B (resource overhead at scale):**
- **Orchestration**: Is a shell script sufficient for spawning 10-100 VMM instances, or do we need dedicated tooling (e.g., a Rust harness)?
- **Resource measurement**: How to reliably measure per-VMM CPU%, memory, disk I/O? `/proc/[pid]/stat` + `/proc/[pid]/io` vs external monitoring?
- **Storage characterization**: HDD vs SSD on remote box — need to measure baseline I/O latency to contextualize disk overhead numbers
- **Comparison baseline**: For "traditional monitoring" comparison, need Prometheus node-exporter per VM + central scrape config

**Grafana dashboard (task 5.4):** Integration path is to wire `remote_write::push()` into VMM maintenance loop → push to remote Prometheus → visualize in existing Grafana. The `rondo::remote_write` module is fully implemented with protobuf serialization, snappy compression, and retry logic but not yet wired into the VMM.

## Files to modify
- `rondo-cli/src/main.rs` — `--range all` support
- `rondo-demo-vmm/guest/workload.sh` — tunable duration
- `rondo-demo-vmm/guest/init` — parse cmdline for workload_duration
- `Makefile` — benchmark targets, fix vmm-demo query
- `docs/IMPLEMENTATION.md` — scale benchmark notes + reference this doc
- `CLAUDE.md` — new targets + Prometheus/Grafana note

## Verification
1. `make vmm-demo` — should show actual data points in the post-run query
2. `make vmm-bench-45` — guest should run ~45s workload, store should have ~45 data points at 1s resolution
3. `rondo-cli query vmm_metrics vcpu_exits_total --range all --format csv` — should return all recorded points
4. `make vmm-clippy` and `make vmm-test` — all clean
