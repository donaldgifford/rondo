# Embedded Time-Series Storage Engine — Alternative MVP Plan

## Using an Existing VMM (Firecracker / Cloud Hypervisor)

---

## Why Consider This Alternative

The rust-vmm MVP plan (Plan A) builds a demo VMM from scratch using rust-vmm crates. That approach gives maximum integration depth — the TSDB library lives inside the VMM process and instruments the vCPU exit handler directly. But it also means spending 2-3 weeks building and debugging a VMM that is not the product.

This alternative plan uses an existing, production-quality VMM and integrates the TSDB library at a different layer. The tradeoff is clear: less integration depth in exchange for dramatically less VMM engineering, a faster path to demonstrating the library's value, and the ability to show the library working with VMMs that people actually run in production.

---

## Option Comparison

| Dimension | Plan A: rust-vmm from scratch | Plan B: Firecracker | Plan C: Cloud Hypervisor |
|-----------|-------------------------------|---------------------|--------------------------|
| **VMM engineering effort** | 2-3 weeks | Near zero | Near zero |
| **TSDB integration point** | Inside VMM process (vCPU exit handler) | Host-side agent consuming Firecracker's metrics socket | Inside CH process (fork/patch) or host-side agent |
| **Metrics available** | Everything KVM exposes, directly | What Firecracker chooses to emit (limited but real) | Everything KVM exposes if patching; external otherwise |
| **"Embedded" purity** | True embedding — same process, same address space | Adjacent process — not truly embedded, but demonstrates the architecture | True embedding if forking CH; adjacent otherwise |
| **Production credibility** | "This is a demo VMM" | "This works with Firecracker, the thing AWS Lambda runs on" | "This works with Cloud Hypervisor, used in production by ACRN/Intel" |
| **Path to real adoption** | Users must build their own VMM integration | Users can adopt the host-agent pattern immediately | Users can adopt the pattern or fork CH |
| **Time to first demo** | ~7 weeks | ~3-4 weeks | ~3-5 weeks |

The recommendation: **Firecracker (Plan B) is the fastest path to a compelling demo.** Cloud Hypervisor (Plan C) offers the best middle ground if you want true embedding without building a VMM from scratch. Both are worth understanding — you may end up supporting multiple integration patterns anyway.

---

## Plan B: Firecracker Integration

### How Firecracker Metrics Work Today

Firecracker exposes metrics via a Unix socket. You configure it at boot by specifying a metrics sink:

```bash
curl --unix-socket /tmp/firecracker.socket -X PUT \
  http://localhost/metrics \
  -d '{"metrics_path": "/tmp/fc-metrics.fifo"}'
```

Firecracker then writes a JSON blob to this FIFO at a configured interval (or on flush). The JSON contains flat counters and gauges:

```json
{
  "utc_timestamp_ms": 1706000000000,
  "api_server": { "process_startup_time_us": 1520, "sync_response_fails": 0 },
  "block": { "read_bytes": 4096000, "read_count": 1000, "write_bytes": 0, "write_count": 0 },
  "vcpu": { "exit_io_in": 5432, "exit_io_out": 12301, "exit_mmio_read": 890, "exit_mmio_write": 342 },
  "net": { "rx_bytes_count": 0, "tx_bytes_count": 0, "rx_packets_count": 0, "tx_packets_count": 0 },
  "seccomp": { "num_faults": 0 },
  ...
}
```

This is actually a decent set of metrics — vCPU exit counts by type, block I/O throughput, network counters, and API latency. The problem is what happens next: today, people either ignore this data, pipe it to CloudWatch, or have a custom script that parses the JSON and pushes it to Prometheus. All of these lose the performance and locality advantages.

### The Integration: rondo as a Firecracker Metrics Sink

Instead of piping Firecracker's metrics JSON to a remote system, we build a lightweight host-side agent (a single Rust binary) that:

1. Reads from each Firecracker VM's metrics FIFO
2. Parses the JSON and records values into a per-VM rondo store
3. Runs consolidation ticks
4. Exports consolidated tiers upstream on a configurable interval

```
┌──────────────────────────────────────────────────────────────┐
│                         Host                                  │
│                                                               │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐          │
│  │ Firecracker  │  │ Firecracker  │  │ Firecracker  │          │
│  │   VM 0       │  │   VM 1       │  │   VM 2       │          │
│  │              │  │              │  │              │          │
│  │  metrics ──→ FIFO  metrics ──→ FIFO  metrics ──→ FIFO     │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘         │
│         │                 │                 │                  │
│  ┌──────┴─────────────────┴─────────────────┴───────────┐     │
│  │                rondo-agent                           │     │
│  │                                                       │     │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐            │     │
│  │  │ Store:   │  │ Store:   │  │ Store:   │            │     │
│  │  │ VM 0     │  │ VM 1     │  │ VM 2     │            │     │
│  │  │ 1s/10m   │  │ 1s/10m   │  │ 1s/10m   │            │     │
│  │  │ 10s/6h   │  │ 10s/6h   │  │ 10s/6h   │            │     │
│  │  │ 5m/7d    │  │ 5m/7d    │  │ 5m/7d    │            │     │
│  │  └──────────┘  └──────────┘  └──────────┘            │     │
│  │                                                       │     │
│  │  ┌─────────────────────────────────────────────────┐  │     │
│  │  │  Decision Engine (reads local tiers)            │  │     │
│  │  │  - Host saturation detection                    │  │     │
│  │  │  - Per-VM anomaly detection                     │  │     │
│  │  │  - Export 5m rollups to fleet TSDB              │  │     │
│  │  └─────────────────────────────────────────────────┘  │     │
│  │                                                       │     │
│  │  HTTP API: /query, /health, /vms                      │     │
│  └───────────────────────────┬───────────────────────────┘     │
│                              │                                 │
└──────────────────────────────┼─────────────────────────────────┘
                               │ 5m rollups
                               ▼
                     Fleet TSDB (Prometheus / Victoria)
```

### What the Agent Looks Like

The agent is a single Rust binary that manages multiple Firecracker VMs on a host. It's not a general-purpose metrics collector — it specifically understands Firecracker's metrics format and the lifecycle of microVMs.

```rust
// rondo-fc-agent: main loop (simplified)

struct VmState {
    vm_id: String,
    store: rondo::Store,
    fifo: File,
    handles: MetricHandles, // pre-registered SeriesHandles for each FC metric
}

struct MetricHandles {
    vcpu_exit_io_in: SeriesHandle,
    vcpu_exit_io_out: SeriesHandle,
    vcpu_exit_mmio_read: SeriesHandle,
    vcpu_exit_mmio_write: SeriesHandle,
    blk_read_bytes: SeriesHandle,
    blk_read_count: SeriesHandle,
    blk_write_bytes: SeriesHandle,
    blk_write_count: SeriesHandle,
    net_rx_bytes: SeriesHandle,
    net_tx_bytes: SeriesHandle,
    // ... ~20-30 series per VM
}

fn main() {
    let mut vms: HashMap<String, VmState> = HashMap::new();

    // Watch for new Firecracker instances (inotify on metrics dir)
    // or accept registrations via a Unix socket from the orchestrator.

    loop {
        // Poll all FIFOs (epoll)
        for vm in vms.values_mut() {
            if let Some(json) = read_metrics_fifo(&vm.fifo) {
                let ts = timestamp_now();
                let metrics: FirecrackerMetrics = parse(&json);

                // These are the hot-path calls — they hit rondo's
                // zero-alloc mmap write path
                vm.store.record(&vm.handles.vcpu_exit_io_in,
                    metrics.vcpu.exit_io_in as f64, ts);
                vm.store.record(&vm.handles.vcpu_exit_io_out,
                    metrics.vcpu.exit_io_out as f64, ts);
                vm.store.record(&vm.handles.blk_read_bytes,
                    metrics.block.read_bytes as f64, ts);
                // ... etc
            }
        }

        // Consolidation tick (1s)
        if tick_due() {
            for vm in vms.values() {
                vm.store.consolidate();
            }
        }

        // Export tick (5m)
        if export_due() {
            let batch = collect_drain_all(&vms, Tier::FiveMinute);
            remote_write(batch, &fleet_tsdb_endpoint);
        }

        // Cleanup: when a VM exits, drain remaining data and archive store
        cleanup_exited_vms(&mut vms);
    }
}
```

### Advantages of This Approach

**Zero VMM engineering.** Firecracker is battle-tested, well-documented, and trivial to run. You download a binary, boot a VM in one API call, and start getting metrics immediately. All engineering effort goes into the library and the agent.

**Production credibility.** "This works with Firecracker" is a stronger statement than "this works with our demo VMM." People evaluating the library can try it with their existing Firecracker deployments.

**Demonstrates the architectural pattern, not just the embedding.** Even though the library isn't inside Firecracker's process, the agent demonstrates the core value proposition: local storage with tiered consolidation, high-resolution data available for local decisions, and consolidated exports upstream. The architecture diagram is the same — only the boundary between "VMM" and "TSDB" shifts from a function call to a FIFO read.

**Immediate path to real use.** Anyone running Firecracker in production can deploy the agent today. They don't need to modify their VMM or change their deployment model.

### Limitations

**Not truly embedded.** The TSDB is in a separate process from the VMM. The write path includes a FIFO read and JSON parse, which is microseconds, not nanoseconds. This is still vastly better than shipping metrics over HTTP to a remote Prometheus, but it doesn't prove the "nanosecond write latency" claim that the library is designed for.

**Limited to what Firecracker emits.** Firecracker's metrics JSON is useful but not exhaustive. You can't instrument individual vCPU exit handlers or get per-request block I/O latency — Firecracker aggregates before emitting. You see `exit_io_in: 5432` as a total count, not per-exit timing.

**JSON parsing in the hot-ish path.** Firecracker emits JSON. Parsing it isn't expensive, but it's not zero-cost either. This is a minor concern but worth noting for the benchmarks — the write-path benchmark should separate "time to parse Firecracker JSON" from "time for rondo.record()."

### Mitigations

To still prove the nanosecond write-path claim, include a **standalone microbenchmark** that calls `store.record()` directly in a tight loop — independent of the Firecracker integration. This lets you make both claims: "the library itself is nanosecond-fast" and "the Firecracker agent integration is microsecond-fast."

To show what's possible with deeper integration, include a **section in the documentation** showing how a Firecracker fork or a custom rust-vmm VMM could embed the library directly for even lower overhead. The agent is the easy path; embedding is the optimal path. Both are valid.

---

## Plan C: Cloud Hypervisor Integration

### Why Cloud Hypervisor Is Different

Cloud Hypervisor (CH) is an open-source VMM written in Rust, built on rust-vmm crates, and maintained by a community that includes Intel, Microsoft, and others. Unlike Firecracker, which is tightly controlled by AWS, CH is designed to be forked and extended. Its architecture is modular, and its codebase is structured to support exactly the kind of instrumentation we want.

This makes CH a middle ground between "build a VMM from scratch" and "use an opaque binary."

### Integration Options

**Option C1: External agent (same as Firecracker approach)**

Cloud Hypervisor supports `--event-monitor` which writes VM lifecycle events to a socket, and can expose a REST API. You can build an agent similar to the Firecracker plan that polls CH's API for metrics and records them into rondo stores.

Pros: No VMM modification needed. Cons: Same limitations as the Firecracker approach — not truly embedded, limited to what CH exposes externally.

**Option C2: Fork and instrument (recommended)**

Fork Cloud Hypervisor and add rondo instrumentation directly in the VMM's hot paths. Since CH is Rust and uses the same rust-vmm crates we'd use in Plan A, the integration looks nearly identical to the "from scratch" approach but with a fully functional VMM as the starting point.

The key instrumentation points in CH's codebase:

```
cloud-hypervisor/
├── vmm/src/
│   ├── cpu.rs              # vCPU thread — KVM_RUN loop
│   │   └── vcpu_run()      # ← instrument exit handling here
│   │
│   ├── device_manager.rs   # device setup
│   │
│   ├── devices/
│   │   ├── virtio/
│   │   │   ├── block.rs    # ← instrument I/O completion here
│   │   │   └── net.rs      # ← instrument TX/RX here
│   │   └── ...
│   │
│   └── vm.rs               # VM lifecycle
│       └── boot()          # ← initialize rondo store here
```

The instrumentation is surgical — you add a `rondo::Store` to the `Vm` struct, register series during device setup, and add `store.record()` calls in the exit handler and device completion paths. The total diff against upstream CH would be a few hundred lines.

```rust
// In cpu.rs, inside the vCPU run loop:
loop {
    match vcpu_fd.run() {
        Ok(exit_reason) => {
            let exit_start = Instant::now();

            match exit_reason {
                VcpuExit::IoIn(port, data) => {
                    handle_io_in(port, data);
                    // ← ADD: one line
                    self.metrics.record_exit("io_in", exit_start.elapsed());
                }
                VcpuExit::MmioRead(addr, data) => {
                    handle_mmio_read(addr, data);
                    self.metrics.record_exit("mmio_read", exit_start.elapsed());
                }
                // ... other exit types
            }
        }
        Err(e) => { /* ... */ }
    }
}

// metrics.rs — thin wrapper around rondo
impl VmMetrics {
    pub fn record_exit(&self, reason: &str, duration: Duration) {
        let ts = timestamp_now();
        if let Some(handle) = self.exit_handles.get(reason) {
            self.store.record(handle, duration.as_nanos() as f64, ts).ok();
        }
        self.store.record(&self.exit_total_handle, 1.0, ts).ok();
    }
}
```

### Advantages of the CH Fork Approach

**True embedding with minimal VMM engineering.** You get the "same process, same address space, nanosecond writes" story without building a VMM from scratch. Cloud Hypervisor already handles vCPU management, device emulation, memory management, and all the operational complexity.

**Full access to internal metrics.** Unlike the Firecracker approach, you can instrument anything — per-exit latency, per-request block I/O timing, memory balloon pressure, dirty page rates. You're in the VMM's source code.

**Upstream potential.** Cloud Hypervisor is community-governed. A well-designed instrumentation layer could potentially be proposed upstream as an optional feature, giving the project visibility and adoption through CH's existing user base.

**Production-ready VMM.** CH runs real workloads. It supports VFIO passthrough for GPUs, vhost-user for network and storage, live migration, and memory hotplug. A demo built on CH is immediately relevant to people running real infrastructure, not just a toy.

### Cloud Hypervisor Fork — What to Modify

| File | Change | Effort |
|------|--------|--------|
| `Cargo.toml` | Add `rondo` dependency | Trivial |
| `vmm/src/vm.rs` | Add `Store` to `Vm` struct, initialize at boot | Small |
| `vmm/src/cpu.rs` | Add `record()` calls in vCPU exit handler | Small |
| `vmm/src/devices/virtio/block.rs` | Add `record()` calls in I/O completion | Small |
| `vmm/src/devices/virtio/net.rs` | Add `record()` calls in TX/RX paths | Small |
| `vmm/src/api/mod.rs` | Add `/metrics/query` endpoint to CH's existing HTTP API | Medium |
| New: `vmm/src/metrics.rs` | rondo integration wrapper, schema config, series registration | Medium |

Total estimated diff: 300-500 lines against upstream CH. Maintainable as a patch set or thin fork.

### Architecture with CH Fork

```
┌──────────────────────────────────────────────────────────────┐
│                         Host                                  │
│                                                               │
│  ┌───────────────────────────────────────────────────────┐   │
│  │          Cloud Hypervisor (forked) — VM 0              │   │
│  │                                                         │   │
│  │  vCPU loop ──→ store.record(exit_type, latency)        │   │
│  │  virtio-blk ──→ store.record(io_latency, throughput)   │   │
│  │  virtio-net ──→ store.record(rx_bytes, tx_bytes)       │   │
│  │                                                         │   │
│  │  ┌──────────────────────────────────────────────────┐  │   │
│  │  │ rondo store (mmap'd, ~2MB)                      │  │   │
│  │  │ Tier 0: 1s × 10m = 600 slots                     │  │   │
│  │  │ Tier 1: 10s × 6h = 2,160 slots                   │  │   │
│  │  │ Tier 2: 5m × 7d = 2,016 slots                    │  │   │
│  │  └──────────────────────────────────────────────────┘  │   │
│  │                                                         │   │
│  │  HTTP API: /api/v1/metrics/query (extends CH's API)    │   │
│  └────────────────────────────┬────────────────────────────┘   │
│                               │                                │
│  ┌────────────────────────────┴────────────────────────────┐   │
│  │              Host Orchestrator / Agent                    │   │
│  │  - Reads 10s tier from each CH instance via HTTP API     │   │
│  │  - Or reads mmap'd store files directly (same host)      │   │
│  │  - Exports 5m rollups to fleet TSDB                      │   │
│  └──────────────────────────┬───────────────────────────────┘   │
│                             │                                   │
└─────────────────────────────┼───────────────────────────────────┘
                              ▼
                    Fleet TSDB (5m rollups)
```

---

## Revised Milestones — Plan B (Firecracker)

### Milestone 1: Storage Engine Core (Weeks 1-3)

Same as Plan A — the library is identical regardless of integration target.

- [ ] Slab file format (create, mmap, read/write slots)
- [ ] Series registration with label indexing
- [ ] `record()` hot path — zero-alloc mmap write
- [ ] `record_batch()` for multi-series atomic writes
- [ ] Time-range query over a single tier
- [ ] Unit tests, microbenchmarks (target: < 50ns p99 for record())

### Milestone 2: Tiered Consolidation (Weeks 3-4)

Same as Plan A.

- [ ] Consolidation engine: avg, min, max, last, sum, count
- [ ] `consolidate()` driven by caller
- [ ] Tier cascade on wraparound
- [ ] Auto tier selection for queries

### Milestone 3: Export and CLI (Weeks 4-5)

Same as Plan A.

- [ ] `drain()` interface
- [ ] Prometheus remote-write client
- [ ] CLI: info, query, bench commands

### Milestone 4: Firecracker Agent (Weeks 5-6)

This replaces the "build a VMM" milestone. Dramatically less effort.

- [ ] Firecracker metrics FIFO parser (JSON → typed struct)
- [ ] Per-VM store lifecycle (create on VM boot, drain on VM exit)
- [ ] Epoll-based main loop watching multiple FIFOs
- [ ] VM discovery (inotify on a directory, or registration socket)
- [ ] HTTP API for querying any VM's local store
- [ ] Integration test: boot 5 Firecracker VMs, verify all metrics flowing

### Milestone 5: Benchmarks and Documentation (Weeks 6-7)

- [ ] Benchmark A: standalone `record()` latency (proves library performance)
- [ ] Benchmark B: agent overhead at scale (50, 100, 200 Firecracker VMs)
- [ ] Benchmark C: ephemeral VM data capture (30-second VM lifecycle)
- [ ] Benchmark D: comparison against Prometheus + node-exporter scraping Firecracker's `/metrics`
- [ ] README, architecture docs, getting started guide
- [ ] Grafana dashboard showing fleet view from exported rollups

**Total time: ~7 weeks** (vs ~8 weeks for Plan A), with significantly less risk because the VMM engineering is eliminated.

---

## Revised Milestones — Plan C (Cloud Hypervisor Fork)

### Milestones 1-3: Storage Engine (Weeks 1-5)

Identical to Plans A and B.

### Milestone 4: Cloud Hypervisor Instrumentation (Weeks 5-7)

- [ ] Fork Cloud Hypervisor at latest stable tag
- [ ] Add rondo as a dependency, create `metrics.rs` integration module
- [ ] Initialize store in `Vm::boot()` with configurable schema
- [ ] Instrument `vcpu_run()` exit handler — record exit reason and handling duration
- [ ] Instrument virtio-blk completion path — record per-request latency and throughput
- [ ] Instrument virtio-net TX/RX paths
- [ ] Add VMM process metrics (RSS, FDs, uptime via /proc/self)
- [ ] Add 1-second consolidation tick to CH's event loop
- [ ] Extend CH's HTTP API with `/api/v1/metrics/query` endpoint
- [ ] Integration test: boot a VM, run workload, query metrics via API

### Milestone 5: Benchmarks and Documentation (Weeks 7-8)

- [ ] Benchmark A: `record()` latency inside CH's vCPU exit handler (proves true embedded performance)
- [ ] Benchmark B: CH with rondo vs CH + external Prometheus scraping (resource overhead)
- [ ] Benchmark C: ephemeral VM data capture
- [ ] Benchmark D: vCPU exit handler overhead with and without instrumentation (proves near-zero impact)
- [ ] README, architecture docs, patch guide for CH
- [ ] Grafana dashboard

**Total time: ~8 weeks**, same as Plan A but with a production-quality VMM instead of a demo.

---

## Recommendation Matrix

| If your priority is... | Choose... | Because... |
|------------------------|-----------|------------|
| **Fastest path to demo** | Plan B (Firecracker) | No VMM engineering. Library + agent in 7 weeks. |
| **Proving true embedded performance** | Plan C (Cloud Hypervisor fork) | Real embedding in a real VMM, nanosecond writes proven in production code. |
| **Maximum learning / control** | Plan A (rust-vmm from scratch) | You own every line. Best for evolving into Nexus. |
| **Broadest initial audience** | Plan B (Firecracker) | More people run Firecracker than CH. The agent pattern requires no VMM modification. |
| **Upstream contribution potential** | Plan C (Cloud Hypervisor fork) | CH is community-governed. Instrumentation could be proposed as an optional feature. |
| **Doing both demos** | Plan B first, then Plan C | Ship the Firecracker agent in 7 weeks. Then do the CH fork to prove true embedding. Two integration patterns, one library. |

### The "Do Both" Path

There is a compelling argument for building **Plan B first, then Plan C**:

1. **Weeks 1-5**: Build the library (shared across all plans)
2. **Weeks 5-7**: Build the Firecracker agent (Plan B). Ship it. Get feedback.
3. **Weeks 7-9**: Fork Cloud Hypervisor and add direct instrumentation (Plan C). This proves the deeper integration story.

This gives you two reference implementations — one showing "easy adoption with any VMM" and one showing "what's possible with true embedding." The library is the same in both cases. The benchmarks from Plan C prove the nanosecond claims. The Firecracker agent proves the "drop this into your existing fleet" story.

Combined timeline: ~9 weeks for both, with a shippable artifact at week 7.

---

## Project Structure (Plan B — Firecracker Agent)

```
rondo/
├── Cargo.toml                       # workspace root
├── README.md
│
├── rondo/                          # the library crate (identical across plans)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── store.rs
│       ├── slab.rs
│       ├── ring.rs
│       ├── series.rs
│       ├── schema.rs
│       ├── consolidate.rs
│       ├── query.rs
│       └── export.rs
│
├── rondo-cli/                      # CLI tool (identical across plans)
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
│
├── rondo-fc-agent/                 # Firecracker agent (Plan B)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                  # epoll loop, VM lifecycle
│       ├── fifo.rs                  # Firecracker metrics FIFO reader
│       ├── parser.rs               # JSON → typed metrics struct
│       ├── vm_store.rs             # per-VM store management
│       ├── discovery.rs            # VM discovery (inotify / socket)
│       ├── api.rs                  # HTTP query endpoint
│       └── export.rs               # remote-write push to fleet TSDB
│
├── benchmarks/
│   ├── write_overhead/              # standalone record() latency
│   ├── agent_overhead/              # CPU/mem at 50/100/200 VMs
│   ├── ephemeral_vm/                # 30-second VM data capture
│   └── vs_prometheus/               # head-to-head with scrape model
│
└── docs/
    ├── architecture.md
    ├── storage-format.md
    ├── firecracker-integration.md
    └── benchmarks.md
```

## Project Structure (Plan C — Cloud Hypervisor Fork)

```
rondo/
├── (same library, CLI, and benchmark structure as above)
│
├── cloud-hypervisor/                # git submodule or fork
│   └── (upstream CH with patches applied)
│
├── patches/                         # maintainable patch series against CH
│   ├── 0001-add-rondo-dependency.patch
│   ├── 0002-add-metrics-module.patch
│   ├── 0003-instrument-vcpu-exit-handler.patch
│   ├── 0004-instrument-virtio-blk.patch
│   ├── 0005-instrument-virtio-net.patch
│   ├── 0006-add-metrics-api-endpoint.patch
│   └── 0007-add-consolidation-tick.patch
│
└── docs/
    ├── architecture.md
    ├── storage-format.md
    ├── cloud-hypervisor-integration.md
    └── benchmarks.md
```

---

## Success Criteria (Shared Across All Plans)

Regardless of which integration path is chosen, the MVP is successful if:

1. **Library performance**: `record()` < 50ns p99, zero heap allocations — proven by standalone microbenchmark.

2. **Predictable storage**: Deterministic disk usage, no growth over time, calculable at provisioning time.

3. **Ephemeral data capture**: 100% of data points captured from a 45-second VM lifecycle. Traditional scrape model captures < 10%.

4. **Resource efficiency**: Order of magnitude less CPU and memory than equivalent Prometheus + agent stack at 100 VMs.

5. **Integration simplicity**: The Firecracker agent is a single binary with a single config file. The CH integration is < 500 lines of diff.

6. **Interoperability**: Consolidated data flows to existing Prometheus/Grafana infrastructure via remote-write with no special configuration on the receiving end.
