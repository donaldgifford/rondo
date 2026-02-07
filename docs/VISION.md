# The Observability Tax: Why Modern Infrastructure Needs Embedded Time-Series Storage

## The Problem

Enterprise infrastructure teams are spending between 20-40% of their total cloud compute budget on observability. Not on the insights observability provides — on the *machinery* of collecting, transporting, storing, and querying metrics that most organizations only look at when something is already on fire.

The architecture responsible for this cost looks roughly the same everywhere:

1. An agent or sidecar runs alongside every workload (DaemonSet, sidecar container, host daemon)
2. The agent scrapes or receives metrics over HTTP/gRPC on a polling interval
3. Metrics are buffered in memory, serialized, and shipped over the network to a centralized TSDB
4. The TSDB ingests, indexes, compacts, and stores the data
5. A query layer sits on top for dashboards and alerting

Each of these stages adds compute overhead, memory pressure, network bandwidth, and operational complexity. At scale — thousands of nodes, millions of active series — the observability stack itself becomes one of the largest and most fragile workloads in the fleet.

The numbers tell the story plainly:

- A typical Prometheus instance consuming 200k samples/second requires 8-16 GB of memory and multiple CPU cores just for ingestion and WAL management
- Thanos or Cortex deployments add compactors, store gateways, and object storage costs that routinely exceed the cost of the workloads being monitored
- Most organizations configure retention between 24 hours and 7 days for high-resolution data because the storage and compaction costs of keeping more are prohibitive
- DaemonSet-based agents (Datadog, Grafana Agent, OTEL Collector) consume 100-500 MB of memory and 0.1-0.5 CPU cores *per node*, resources subtracted directly from tenant workloads

The result is a system that is expensive to run, operationally complex, and paradoxically low-resolution where it matters most — at the point of decision. By the time a metric travels from a workload through the collection pipeline to a centralized store and triggers an alert, the window for automated remediation has often closed.

This is not a tooling problem. It is an architectural problem. The entire model of externalizing metrics from the systems that produce them was designed for an era of relatively static, long-lived infrastructure. That era is ending.

## The Shifting Landscape: From Pods to MicroVMs

Kubernetes became the dominant compute platform by providing a common abstraction over heterogeneous infrastructure. But that abstraction comes at a cost — the control plane, the CNI, the CSI, the service mesh, the admission controllers, the operators — all of it is overhead that sits between the workload and the hardware.

A parallel shift is now underway. Driven by the need for stronger isolation, faster cold-start, and more deterministic performance, organizations are moving security-sensitive and performance-critical workloads from container orchestrators to microVM-based environments:

- **AWS Lambda and Fargate** run on Firecracker microVMs, not containers
- **Fly.io** built its entire platform on Firecracker
- **Cloud Hypervisor** and the **rust-vmm** ecosystem are enabling a new generation of purpose-built VMMs
- **Kata Containers** bridges the gap by running OCI containers inside lightweight VMs
- **GPU/accelerator workloads** increasingly demand direct hardware access that container abstractions cannot cleanly provide

In a microVM environment, the Kubernetes observability model breaks down completely:

**There is no DaemonSet.** There is no shared node-level agent that can scrape all workloads. Each microVM is an isolated machine with its own kernel. Running a full metrics agent inside each microVM defeats the purpose of keeping the VM minimal and fast-booting.

**There is no service discovery.** Kubernetes provides a built-in mechanism for Prometheus to discover scrape targets. MicroVM orchestrators (custom or otherwise) do not have an equivalent standard. Every organization builds bespoke discovery.

**The lifecycle is different.** A Firecracker VM can boot in 125ms and die 30 seconds later. The traditional scrape-and-ship model, with its 15-60 second polling intervals, may never even collect a single data point from a short-lived VM. Metrics from ephemeral workloads are systematically lost.

**The density is different.** A single host might run hundreds or thousands of microVMs. The per-workload overhead of an agent, an HTTP endpoint, and a scrape cycle that was tolerable at "50 pods per node" becomes untenable at "500 microVMs per host."

The organizations that are building custom VMMs — whether from scratch using rust-vmm components, wrapping Firecracker, or assembling Cloud Hypervisor configurations — are doing so precisely because they need control over performance characteristics that off-the-shelf platforms cannot guarantee. These are the teams that can least afford to bolt on a heavyweight observability stack that introduces the very unpredictability they are trying to eliminate.

## Why Existing Solutions Do Not Fit

### Prometheus and Its Ecosystem (Thanos, Cortex, Mimir, VictoriaMetrics)

Prometheus is the de facto standard for cloud-native monitoring, and for good reason. It defined the dimensional data model (metric name + label set) that everyone now uses, and PromQL is the closest thing the industry has to a standard query language for metrics.

But Prometheus is a *server*. It assumes it is a long-running process with significant memory, a local disk for its TSDB, and network access to scrape targets. It cannot be embedded. Its TSDB, while well-engineered, is designed for the general case — variable retention, dynamic schema, background compaction — not for the constrained, predictable case that embedded systems need.

The scale-out solutions (Thanos, Mimir, VictoriaMetrics) add yet more infrastructure. They are solving the right problem — global query over distributed Prometheus instances — but they do so by adding compactors, ingesters, store gateways, and object storage dependencies. For organizations already struggling with observability costs, the answer is not more components.

### OpenTelemetry

OTEL is winning the instrumentation and transport standard wars, and rightly so. But OTEL is a *protocol and SDK*, not a storage engine. The OTEL Collector is another agent that must run somewhere, consume resources, and ship data to a backend. OTEL defines how metrics move between systems; it does not address where they live or how they are retained.

A modern embedded TSDB should *speak* OTEL for interoperability but should not *depend* on OTEL infrastructure for core functionality.

### Commercial Platforms (Datadog, New Relic, Dynatrace, Chronosphere)

Commercial observability platforms trade operational complexity for financial cost. They remove the burden of running Prometheus and Thanos yourself, replacing it with per-host, per-metric, or per-GB pricing that scales linearly (or worse) with infrastructure growth.

For large fleets, the costs are staggering. It is common for enterprise Datadog bills to reach seven or eight figures annually. These platforms also introduce a hard external dependency — metrics leave your network, are stored on infrastructure you do not control, and are queried through APIs with rate limits and retention policies you cannot modify.

For organizations building custom VMMs and performance-critical infrastructure, sending every metric to a SaaS platform is both prohibitively expensive and architecturally inappropriate. The data is most valuable where it is produced, not in a vendor's multi-tenant store.

### rrdtool and Its Legacy

rrdtool got the core abstraction right 25 years ago: fixed-size, round-robin archives with pre-defined resolution tiers and automatic consolidation. The storage is bounded and predictable. Writes are O(1). There is no compaction, no garbage collection, no operational surprise.

But rrdtool was designed for a different era:

- **One file per metric database.** At cloud scale, this means millions of files and filesystem-level bottlenecks.
- **No dimensional data model.** No labels, no tags. Series are identified positionally, not semantically.
- **C codebase with decades of accumulated complexity.** Extending or embedding rrdtool in modern systems software is painful.
- **Built-in graphing tied to the storage engine.** Rendering concerns are mixed with storage concerns.
- **No standard protocol support.** No Prometheus remote-write, no OTLP, no modern query interface.

The philosophy is sound. The implementation is a product of its time.

## Our Approach: An Embedded Round-Robin Time-Series Store

We propose building a new time-series storage engine — implemented as a **Rust library with C FFI** — that combines rrdtool's storage philosophy with a modern dimensional data model and integration interfaces. The design principles are:

### 1. Library First, Server Optional

The primary artifact is a Rust crate that can be linked into any application. No daemon, no sidecar, no network dependency. A thin server binary can wrap the library for standalone use cases, but the library is the product.

This follows the SQLite model: the most widely deployed database in the world is not a server. It is a library embedded in applications that need structured storage without operational overhead. The time-series space has no equivalent.

### 2. Fixed-Size, Predictable Storage

Storage is bounded by configuration, not by data volume. When you create a schema, you know exactly how much disk (or memory) it will consume, forever. There is no unbounded WAL, no compaction backlog, no storage surprise.

This is the rrdtool contract, preserved intact.

### 3. Write Path: Zero Allocation, Zero Coordination

The `record()` function — the one called millions of times per second in a VMM's hot path — must not allocate heap memory, must not acquire a contested lock (single-writer is the expected case), and must not perform a syscall. It writes to a pre-computed slot in a memory-mapped ring buffer. The kernel handles page writeback asynchronously.

Target write latency: single-digit nanoseconds. This is non-negotiable for embedding in VMMs and dataplanes.

### 4. Tiered Consolidation at Write Time

When data ages out of a high-resolution tier, it is consolidated (downsampled) into the next tier as part of the write path or a lightweight maintenance tick. There is no background compaction job. Consolidation functions (average, min, max, last, percentiles) are defined per-schema.

### 5. Dimensional Labels, Schema-Bound

Series are identified by a label set (`{__name__="vcpu_steal_ns", instance="vm-abc", vcpu="0"}`). But unlike Prometheus, where any label combination creates a new series dynamically, series are bound to **schemas** that define their retention tiers and consolidation rules. This gives operators explicit control over cardinality and storage cost.

### 6. Standard Protocols for Interoperability

The library and optional server support:

- **Prometheus remote-write** for push-based export to existing infrastructure
- **OTLP** for integration with OpenTelemetry pipelines
- **PromQL-compatible query** for read-side compatibility with Grafana and existing tooling

This is not an island. It is a building block that integrates with what teams already have.

## Integration: The VMM Use Case

The most immediate and compelling integration point is inside custom VMMs — whether built from scratch using the rust-vmm crate ecosystem, wrapping Firecracker, or based on Cloud Hypervisor.

### Metrics at the Source

A VMM has direct access to the most valuable performance signals:

- **vCPU scheduling**: steal time, exit counts and reasons (MMIO, PIO, MSR, halt), context switch frequency
- **Memory**: balloon inflation/deflation pressure, page fault rates, dirty page tracking throughput (critical for live migration decisions)
- **Block I/O**: virtio-blk/virtio-scsi queue depths, request latency distributions, throughput per device
- **Network**: virtio-net TX/RX rates, packet drops, tap/macvtap queue backpressure
- **Snapshot/restore**: checkpoint timing, memory serialization throughput

Today, these metrics either go uncollected (because there is no standard way to export them from a minimal VMM) or are exposed via a metrics endpoint that an external agent must discover and scrape. The embedded model eliminates this gap entirely.

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Host                                  │
│                                                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐     │
│  │   microVM    │  │   microVM    │  │   microVM    │     │
│  │             │  │             │  │             │     │
│  │  ┌───────┐  │  │  ┌───────┐  │  │  ┌───────┐  │     │
│  │  │ VMM   │  │  │  │ VMM   │  │  │  │ VMM   │  │     │
│  │  │  +    │  │  │  │  +    │  │  │  │  +    │  │     │
│  │  │ TSDB  │  │  │  │ TSDB  │  │  │  │ TSDB  │  │     │
│  │  │(embed)│  │  │  │(embed)│  │  │  │(embed)│  │     │
│  │  └───┬───┘  │  │  └───┬───┘  │  │  └───┬───┘  │     │
│  └──────┼──────┘  └──────┼──────┘  └──────┼──────┘     │
│         │                │                │             │
│  ┌──────┴────────────────┴────────────────┴──────┐      │
│  │           Host Orchestrator / Agent            │      │
│  │                                                │      │
│  │  - Reads 10s tier from each VMM store          │      │
│  │  - Makes local placement/migration decisions   │      │
│  │  - Exports 5m rollups to fleet TSDB            │      │
│  └────────────────────┬───────────────────────────┘      │
│                       │                                  │
└───────────────────────┼──────────────────────────────────┘
                        │  5m consolidated pushes
                        ▼
              ┌──────────────────┐
              │   Fleet TSDB     │
              │  (Prometheus /   │
              │   Victoria /     │
              │   hosted)        │
              └──────────────────┘
```

### What This Enables

**Real-time, local decision-making.** The host orchestrator can read the 1-second and 10-second tiers from every VMM's embedded store to make placement, throttling, and migration decisions without a network round-trip to a central TSDB. "Is this host memory-pressured?" is answered by reading local data, not querying Thanos.

**Massive reduction in central ingestion volume.** If each VMM records at 1-second resolution locally but only exports 5-minute consolidations upstream, the central TSDB ingests 300x fewer samples. For a fleet of 10,000 VMs producing 100 series each at 1s resolution, that is the difference between 1,000,000 samples/second centrally and 3,333 samples/second. The cost implications are enormous.

**No lost metrics from ephemeral workloads.** A microVM that lives for 45 seconds still has its full metric history in its local store. The host agent can drain that data after the VM exits. Nothing is lost to missed scrape intervals.

**Debug without escalation.** When investigating a performance issue on a specific VM, an operator can query the local high-resolution data directly rather than hoping the central store retained enough granularity. The 1-second tier gives sub-second visibility into exactly what happened.

## Deployment Options for MicroVM Environments

Organizations moving to microVM-based infrastructure face a spectrum of build-vs-adopt decisions. The embedded TSDB integrates across these options:

### Option A: Custom VMM from Scratch (rust-vmm)

For teams building VMMs using the rust-vmm crate ecosystem (KVM ioctls, virtio device backends, memory management), the TSDB integrates as another crate dependency. The VMM author instruments their vCPU loop, device handlers, and management API directly. This offers maximum control and minimum overhead.

**Best for:** Organizations with specialized isolation, performance, or compliance requirements (defense/IC, HPC, custom cloud providers) that justify the investment in a bespoke VMM.

### Option B: Firecracker Wrapper

Firecracker already exposes a metrics endpoint via a Unix socket. A wrapping orchestrator can consume these metrics and record them into an embedded TSDB instance rather than shipping them externally. For custom forks of Firecracker, the TSDB can be integrated directly into the VMM process.

**Best for:** Organizations using Firecracker for serverless or container-like workloads that want better observability without modifying Firecracker's core.

### Option C: Cloud Hypervisor / QEMU Integration

Cloud Hypervisor (Rust-based) is a natural integration target, similar to Option A. For QEMU-based environments, the C FFI allows the TSDB to be linked into QEMU or a co-located lightweight process that reads QEMU's metrics via QMP.

**Best for:** Organizations with existing VM infrastructure looking to improve observability without a full VMM rewrite.

### Option D: Host-Level Agent (Non-VMM)

Even without embedding in a VMM, the TSDB can serve as the storage backend for a host-level agent that collects system metrics (CPU, memory, disk, network) with a small, predictable resource footprint. This is the "better node-exporter" use case — same data, but stored locally with automatic tiered retention instead of requiring an external Prometheus to scrape and store it.

**Best for:** Any environment — VMs, bare metal, edge, homelab — where running a full Prometheus stack is disproportionate to the monitoring need.

## Why Now: The Economics of Scarcity

Three converging pressures make this approach urgent rather than merely interesting:

### Compute Scarcity Is Real

GPU availability is constrained globally, driving organizations to maximize utilization of every allocated resource. CPU and memory costs have risen as cloud providers adjust pricing. In this environment, dedicating 5-10% of a node's resources to observability agents is no longer an acceptable overhead — it is a material cost that displaces revenue-generating workloads.

In GPU/accelerator environments specifically, the monitoring overhead problem is acute. A DGX or HGX node running ML training workloads cannot afford to have monitoring agents competing for PCIe bandwidth, memory bus cycles, or CPU cores that should be feeding the accelerators. An embedded TSDB with near-zero overhead is not a luxury; it is a requirement for maintaining the utilization rates that justify the hardware investment.

### Stack Complexity Is a Multiplier

Every component in the observability stack has its own failure modes, upgrade cycles, security patches, and operational runbooks. For platform engineering teams already managing Kubernetes clusters, service meshes, CI/CD pipelines, and security compliance frameworks, the observability stack is often the single largest source of operational toil.

The embedded model eliminates entire categories of operational concern:

- No agent DaemonSets to manage, upgrade, and debug
- No ingestion pipeline capacity planning
- No compaction job tuning and monitoring (monitoring the monitoring)
- No network-level concerns about metrics traffic congestion
- No single point of failure in the collection path

Simplification is not just an engineering preference. It is a risk reduction strategy. Every removed component is a component that cannot fail, cannot be misconfigured, and cannot be exploited.

### The Infrastructure Substrate Is Changing

The Kubernetes ecosystem assumed that the pod is the atomic unit of compute and built everything — scheduling, networking, storage, observability — around that abstraction. As the industry moves toward microVMs, unikernels, WebAssembly runtimes, and bare-metal serverless, the pod-centric tooling does not translate.

Organizations building on these next-generation substrates face a choice: shoehorn their workloads back into Kubernetes-shaped abstractions to reuse existing tooling, or build tooling that fits the new model natively. The first option reintroduces the overhead they were trying to escape. The second option requires components — like an embeddable TSDB — that do not yet exist in production-quality form.

## Design Constraints and Non-Goals

To be explicit about scope:

**This is a storage engine and library, not a monitoring platform.** It does not include dashboarding, alerting, incident management, or log aggregation. It stores time-series data efficiently and exposes it for query. Other tools handle everything else.

**This is not a distributed database.** There is no clustering, no consensus, no replication in the core. If you need HA, you run two instances and dual-write — the fixed-size storage makes this cheap and deterministic. Fleet-level aggregation is handled by exporting consolidated data to an existing distributed TSDB.

**This targets bounded, predictable workloads.** If you need unbounded retention, dynamic schema, and the ability to ingest arbitrary cardinality, Prometheus and its ecosystem remain the right choice. This tool is for the case where you know your metrics shape upfront, want deterministic resource usage, and value write-path performance above all else.

**This is Rust-native with C FFI for cross-language embedding.** First-class bindings for Go, Python, and C are in scope. JVM and other runtimes are possible but not a priority.

## What We Believe

We believe the observability industry took a wrong turn when it decided that all metrics must flow through a centralized pipeline before they become useful. This architecture was a reasonable response to the infrastructure of 2015 — relatively static, long-lived, coarse-grained. It is a poor fit for the infrastructure of 2025 and beyond — ephemeral, fine-grained, and performance-critical.

We believe the right model is **metrics at the edge, summaries at the center**. High-resolution data belongs where it is produced and where it informs real-time decisions. Consolidated rollups travel to central stores for fleet-wide visibility and long-term trending. This is not a new idea — rrdtool had it right in 1999. It just needs a modern implementation that fits how infrastructure is actually built today.

We believe there is a large and underserved need for an embedded, high-performance time-series storage engine. Not another server. Not another platform. A library that systems software authors can link into their VMMs, dataplanes, storage controllers, and agents — the same way they link in a memory allocator or a logging framework. Something so small and fast that there is no reason *not* to include it.

We believe this is the foundation of a fundamentally better observability architecture — one that costs less, fails less, and actually has the data you need when you need it.
