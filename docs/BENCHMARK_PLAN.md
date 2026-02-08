# Benchmark Readiness Plan

## Context

The VMM demo runs end-to-end but `rondo-cli query` returns 0 points (the `--range` always computes relative to wall-clock time, missing the store data). The workload is hardcoded at ~18s but we need 15/30/45s variants for Benchmark C (ephemeral VM data capture). Scale benchmarking (Benchmark B) needs investigation and the remote box has Prometheus + Grafana available for integration testing.

## Changes

### 1. Fix CLI `--range all` support ✅

**File:** `rondo-cli/src/main.rs`

- Added `"all"` as special value in `parse_duration()` — returns `u64::MAX`
- When range is `u64::MAX`, set `start_ns = 0` and `end_ns = u64::MAX`
- Also added label-filtered queries: `name{key=value,...}` syntax
- Also fixed `reconstruct_schemas()` to handle `#[serde(flatten)]` metadata format

### 2. Make guest workload duration tunable via kernel cmdline ✅

**Files:**
- `rondo-demo-vmm/guest/workload.sh` — accepts `$WORKLOAD_DURATION` env var, default 18s, distributes phases proportionally
- `rondo-demo-vmm/guest/init` — parses `/proc/cmdline` for `workload_duration=N`, exports as env var

**Duration distribution** (proportional to current 18s = 5+3+5+5):
- Phase 1 (CPU burst): 28% of total
- Phase 2 (idle): 17% of total
- Phase 3 (I/O sim): 28% of total
- Phase 4 (mixed): 28% of total

### 3. Add Makefile benchmark targets ✅

**File:** `Makefile`

- `vmm-bench-15` — 15-second VM lifecycle benchmark
- `vmm-bench-30` — 30-second VM lifecycle benchmark
- `vmm-bench-45` — 45-second VM lifecycle benchmark
- `vmm-bench-capture` — runs all three and reports capture rates

### 4. Update `vmm-demo` target ✅

**File:** `Makefile`

Updated to use `--range all` and label-filtered queries.

### 5. Scale benchmark investigation (Benchmark B) — PLANNED

**Remote environment:** The remote box (10.10.11.33) has Prometheus and Grafana available.

**Open questions for Benchmark B (resource overhead at scale):**
- **Orchestration**: Is a shell script sufficient for spawning 10-100 VMM instances, or do we need dedicated tooling (e.g., a Rust harness)?
- **Resource measurement**: How to reliably measure per-VMM CPU%, memory, disk I/O? `/proc/[pid]/stat` + `/proc/[pid]/io` vs external monitoring?
- **Storage characterization**: HDD vs SSD on remote box — need to measure baseline I/O latency to contextualize disk overhead numbers
- **Comparison baseline**: For "traditional monitoring" comparison, need Prometheus node-exporter per VM + central scrape config

**Grafana dashboard (task 5.4):** Integration path is to wire `remote_write::push()` into VMM maintenance loop → push to remote Prometheus → visualize in existing Grafana. The `rondo::remote_write` module is fully implemented with protobuf serialization, snappy compression, and retry logic but not yet wired into the VMM.

## Verified Results

| Benchmark | Duration | Data Points | Notes |
|-----------|----------|-------------|-------|
| vmm-bench-15 | 15s workload | 19 points | ~2s boot + 15s workload + 2s post-boot overhead |
| vmm-bench-45 | 45s workload | 26 points | ~2s boot + 45s workload, maintenance thread at 1Hz |

**Key finding**: The `vmm_uptime_seconds` series (recorded by the 1Hz maintenance thread) is the most reliable metric for data capture counting. The `vcpu_exits_total` counter overwrites the same slot value (1.0) rather than accumulating, yielding fewer distinct data points. A future improvement would be to accumulate exit counts per second-slot rather than overwriting.

## Files modified
- `rondo-cli/src/main.rs` — `--range all` support, label-filtered queries, metadata fix
- `rondo-demo-vmm/guest/workload.sh` — tunable duration
- `rondo-demo-vmm/guest/init` — parse cmdline for workload_duration
- `Makefile` — benchmark targets, fix vmm-demo query
- `docs/IMPLEMENTATION.md` — scale benchmark notes + reference this doc
- `CLAUDE.md` — new targets + Prometheus/Grafana note

## Verification ✅
1. `make vmm-demo` — shows data points in post-run query ✅
2. `make vmm-bench-45` — guest runs 45s workload, store captures 26 data points ✅
3. `rondo-cli query vmm_metrics 'vcpu_exits_total{reason=io}' --range all` — returns recorded points ✅
4. `make vmm-clippy` and `make vmm-test` — all clean (138 tests pass) ✅
