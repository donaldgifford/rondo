# rondo — Repository Setup & Development Guide

## Repository Structure

```
rondo/
├── .github/
│   ├── workflows/
│   │   ├── ci.yml                    # lint, test, bench on every PR
│   │   ├── release.yml               # cargo publish on tag push
│   │   └── audit.yml                 # weekly cargo-audit for vulnerabilities
│   ├── CODEOWNERS
│   └── PULL_REQUEST_TEMPLATE.md
│
├── Cargo.toml                        # workspace root
├── rust-toolchain.toml               # pin stable toolchain
├── clippy.toml                       # clippy configuration
├── deny.toml                         # cargo-deny configuration
├── .gitignore
├── LICENSE-MIT
├── LICENSE-APACHE
├── README.md
├── CHANGELOG.md
│
├── rondo/                            # core library crate
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs
│   │   ├── store.rs
│   │   ├── slab.rs
│   │   ├── ring.rs
│   │   ├── series.rs
│   │   ├── schema.rs
│   │   ├── consolidate.rs
│   │   ├── query.rs
│   │   └── export.rs
│   ├── benches/
│   │   ├── record.rs                # write-path microbenchmarks
│   │   └── query.rs                 # read-path benchmarks
│   └── tests/
│       ├── integration.rs
│       ├── consolidation.rs
│       └── wraparound.rs
│
├── rondo-cli/                        # CLI tool
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
│
├── rondo-fc-agent/                   # Firecracker agent (Plan B)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── fifo.rs
│       ├── parser.rs
│       ├── vm_store.rs
│       ├── discovery.rs
│       ├── api.rs
│       └── export.rs
│
├── rondo-demo-vmm/                   # demo VMM (Plan A, optional)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs
│   │   ├── vmm.rs
│   │   ├── vcpu.rs
│   │   ├── devices/
│   │   │   ├── serial.rs
│   │   │   └── block.rs
│   │   ├── metrics.rs
│   │   └── api.rs
│   └── guest/
│       ├── build.sh
│       ├── kernel.config
│       └── init.sh
│
├── benchmarks/                       # comparative benchmarks (not Rust benches)
│   ├── write_overhead/
│   │   ├── README.md
│   │   └── run.sh
│   ├── scale_test/
│   │   ├── README.md
│   │   └── run.sh
│   └── ephemeral_vm/
│       ├── README.md
│       └── run.sh
│
└── docs/
    ├── architecture.md
    ├── storage-format.md
    ├── firecracker-integration.md
    └── benchmarks.md
```

---

## Workspace Cargo.toml

```toml
[workspace]
resolver = "2"
members = [
    "rondo",
    "rondo-cli",
    # Uncomment as these are built:
    # "rondo-fc-agent",
    # "rondo-demo-vmm",
]

[workspace.package]
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/YOURUSER/rondo"
rust-version = "1.75"              # MSRV — pin to a reasonable stable version

[workspace.dependencies]
# Shared dependencies pinned at workspace level.
# Individual crates inherit these with { workspace = true }.
memmap2 = "0.9"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
criterion = { version = "0.5", features = ["html_reports"] }

# rust-vmm crates (for demo VMM, when needed)
# kvm-ioctls = "0.19"
# kvm-bindings = "0.10"
# vm-memory = "0.16"
# linux-loader = "0.12"
# virtio-queue = "0.13"
# event-manager = "0.4"
```

---

## Toolchain & Rust Version

```toml
# rust-toolchain.toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
```

Pin to stable. No nightly unless a specific feature demands it (unlikely for this project). `llvm-tools-preview` is for coverage instrumentation via `cargo-llvm-cov`.

---

## Development Tools

### Required (install once)

| Tool | Install | Purpose |
|------|---------|---------|
| `cargo-deny` | `cargo install cargo-deny` | License auditing, vulnerability scanning, dependency policy |
| `cargo-audit` | `cargo install cargo-audit` | CVE scanning against RustSec advisory DB |
| `cargo-llvm-cov` | `cargo install cargo-llvm-cov` | Code coverage via LLVM instrumentation |
| `cargo-nextest` | `cargo install cargo-nextest` | Faster test runner with better output |
| `cargo-watch` | `cargo install cargo-watch` | Auto-rebuild on file changes during development |
| `cargo-release` | `cargo install cargo-release` | Automates version bumps, tagging, and publishing |
| `taplo` | `cargo install taplo-cli` | TOML formatter and linter |
| `git-cliff` | `cargo install git-cliff` | Auto-generate CHANGELOG from conventional commits |

### Optional (nice to have)

| Tool | Install | Purpose |
|------|---------|---------|
| `cargo-flamegraph` | `cargo install flamegraph` | CPU flamegraphs for profiling the write path |
| `cargo-bloat` | `cargo install cargo-bloat` | Track binary size, find what's pulling in weight |
| `cargo-expand` | `cargo install cargo-expand` | Macro expansion debugging |
| `hyperfine` | `cargo install hyperfine` | CLI benchmarking (for comparative benchmarks) |

---

## Linting & Formatting

### Clippy Configuration

```toml
# clippy.toml
too-many-arguments-threshold = 8
cognitive-complexity-threshold = 30
```

Workspace-level clippy lints in `Cargo.toml`:

```toml
[workspace.lints.clippy]
# Correctness
correctness = { level = "deny" }

# Performance — critical for this project
perf = { level = "warn" }
large_enum_variant = { level = "warn" }
inefficient_to_string = { level = "warn" }

# Style
style = { level = "warn" }
needless_return = { level = "allow" }      # sometimes explicit returns are clearer
module_name_repetitions = { level = "allow" }

# Pedantic (opt-in, useful ones)
cast_possible_truncation = { level = "warn" }
cast_sign_loss = { level = "warn" }
cast_precision_loss = { level = "warn" }
missing_errors_doc = { level = "warn" }
missing_panics_doc = { level = "warn" }

# Restriction (opt-in, safety-relevant)
undocumented_unsafe_blocks = { level = "deny" }

[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_docs = "warn"
```

Each crate inherits with:

```toml
[lints]
workspace = true
```

### Rustfmt

```toml
# rustfmt.toml
max_width = 100
use_field_init_shorthand = true
use_try_shorthand = true
edition = "2021"
```

---

## Dependency Policy (cargo-deny)

```toml
# deny.toml
[advisories]
vulnerability = "deny"
unmaintained = "warn"
yanked = "deny"

[licenses]
allow = [
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unicode-3.0",
    "Zlib",
]
copyleft = "deny"
unlicensed = "deny"

[bans]
multiple-versions = "warn"     # flag duplicate versions in the tree
wildcards = "deny"             # no wildcard version specs
highlight = "all"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
```

This enforces: no copyleft dependencies (important for embedding in proprietary VMMs), no known vulnerabilities, no pulling from random git repos or registries.

---

## CI Pipeline

```yaml
# .github/workflows/ci.yml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: "-D warnings"

jobs:
  check:
    name: Check & Lint
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - name: Format check
        run: cargo fmt --all -- --check
      - name: TOML format check
        run: |
          cargo install taplo-cli --locked
          taplo fmt --check
      - name: Clippy
        run: cargo clippy --workspace --all-targets -- -D warnings
      - name: Deny check
        run: |
          cargo install cargo-deny --locked
          cargo deny check

  test:
    name: Test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install nextest
        run: cargo install cargo-nextest --locked
      - name: Run tests
        run: cargo nextest run --workspace
      - name: Run doctests
        run: cargo test --workspace --doc

  coverage:
    name: Coverage
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: llvm-tools-preview
      - uses: Swatinem/rust-cache@v2
      - name: Install llvm-cov
        run: cargo install cargo-llvm-cov --locked
      - name: Generate coverage
        run: cargo llvm-cov --workspace --lcov --output-path lcov.info
      - name: Upload coverage
        uses: codecov/codecov-action@v4
        with:
          files: lcov.info
          fail_ci_if_error: false

  bench:
    name: Benchmarks
    runs-on: ubuntu-latest
    if: github.event_name == 'push' && github.ref == 'refs/heads/main'
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Run benchmarks
        run: cargo bench --workspace -- --output-format bencher | tee bench_output.txt
      # Optional: track benchmark regressions with github-action-benchmark
      # - uses: benchmark-action/github-action-benchmark@v1
      #   with:
      #     tool: cargo
      #     output-file-path: bench_output.txt

  # Runs on main only — validates MSRV
  msrv:
    name: MSRV Check
    runs-on: ubuntu-latest
    if: github.event_name == 'push' && github.ref == 'refs/heads/main'
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.75.0
      - uses: Swatinem/rust-cache@v2
      - name: Check MSRV
        run: cargo check --workspace
```

```yaml
# .github/workflows/audit.yml
name: Security Audit

on:
  schedule:
    - cron: '0 6 * * 1'    # every Monday at 6am UTC
  push:
    paths:
      - '**/Cargo.toml'
      - '**/Cargo.lock'

jobs:
  audit:
    name: Audit
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: rustsec/audit-check@v2
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
```

```yaml
# .github/workflows/release.yml
name: Release

on:
  push:
    tags:
      - 'v*'

jobs:
  publish:
    name: Publish to crates.io
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Publish rondo (library)
        run: cargo publish -p rondo
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      # CLI and agent published separately if/when ready
```

---

## Git Conventions

### Branch Strategy

Simple trunk-based development:

- `main` — always releasable, protected, requires PR review
- Feature branches: `feature/slab-format`, `feature/consolidation-engine`
- Fix branches: `fix/wraparound-off-by-one`
- No long-lived develop branch. No gitflow. Keep it simple.

### Commit Messages

Use conventional commits for clean changelog generation with `git-cliff`:

```
feat(store): implement zero-alloc record() write path
fix(ring): handle wraparound at slot boundary correctly
perf(slab): switch to direct mmap pointer arithmetic
test(consolidation): add property tests for tier cascade
docs: add storage format specification
ci: add benchmark tracking to CI pipeline
chore: update memmap2 to 0.9.5
```

Prefixes: `feat`, `fix`, `perf`, `test`, `docs`, `ci`, `chore`, `refactor`

Scopes (when applicable): `store`, `slab`, `ring`, `series`, `schema`, `consolidate`, `query`, `export`, `cli`, `fc-agent`, `vmm`

### Tagging & Releases

```bash
# Version bump, tag, and publish
cargo release patch  # or minor, major
# This will:
#   1. Bump version in Cargo.toml
#   2. Update Cargo.lock
#   3. Commit with message "chore: release v0.1.1"
#   4. Tag v0.1.1
#   5. Push commit and tag
#   6. CI release workflow publishes to crates.io
```

Generate changelog before release:

```bash
git cliff --output CHANGELOG.md
```

---

## Testing Strategy

### Unit Tests

Live alongside the code in each module. Focus on:

- Ring buffer slot computation and wraparound edge cases
- Slab file creation, mmap lifecycle, recovery after crash
- Series registration, label matching, hash collisions
- Consolidation function correctness (avg, min, max over known inputs)
- NaN handling in all code paths

### Integration Tests

In `rondo/tests/`. End-to-end flows:

- Create store → register series → write 10,000 points → query → verify
- Write past tier 0 capacity → verify tier 1 consolidation is correct
- Simulate crash (kill process) → reopen store → verify data integrity
- Multiple schemas with different retention tiers in one store

### Property Tests

Use `proptest` for the storage engine internals. Key properties:

- For any sequence of writes, `query()` returns exactly what was written (modulo consolidation)
- Consolidation of N high-res points produces exactly ceil(N / ratio) low-res points
- Ring buffer never reads stale data after wraparound
- Store size on disk never exceeds the calculated maximum

```toml
# In rondo/Cargo.toml dev-dependencies
[dev-dependencies]
proptest = "1"
tempfile = "3"
```

### Benchmarks

Use `criterion` in `rondo/benches/`. Critical benchmarks:

- `record()` latency: p50, p99, p999 over 10M writes
- `record()` with allocation tracking (assert zero allocations via custom allocator)
- `query()` latency for various range sizes
- `consolidate()` cost per tier
- Series registration throughput

```rust
// rondo/benches/record.rs
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_record(c: &mut Criterion) {
    // Setup store, register series...
    c.bench_function("record_single", |b| {
        b.iter(|| {
            store.record(&handle, 42.0, timestamp);
        })
    });
}

criterion_group!(benches, bench_record);
criterion_main!(benches);
```

---

## Local Development Workflow

### Day-to-day

```bash
# Watch mode — recompile and test on save
cargo watch -x "nextest run" -x "clippy --workspace"

# Run specific test
cargo nextest run -p rondo test_ring_wraparound

# Run benchmarks locally
cargo bench -p rondo -- record

# Check everything CI will check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
cargo nextest run --workspace
```

### Profiling the write path

```bash
# CPU flamegraph of the benchmark
cargo flamegraph --bench record -- --bench

# Or with perf directly
cargo bench -p rondo -- record --profile-time 10
perf record -g target/release/deps/record-*
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg
```

### Verifying zero allocations

Use a custom global allocator in tests that panics on allocation during the hot path:

```rust
#[cfg(test)]
mod alloc_tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, Ordering};

    static DENY_ALLOC: AtomicBool = AtomicBool::new(false);

    struct CheckedAllocator;
    unsafe impl GlobalAlloc for CheckedAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if DENY_ALLOC.load(Ordering::SeqCst) {
                panic!("allocation detected in hot path!");
            }
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static A: CheckedAllocator = CheckedAllocator;

    #[test]
    fn record_does_not_allocate() {
        let store = /* setup */;
        let handle = store.register("test", &[]).unwrap();

        DENY_ALLOC.store(true, Ordering::SeqCst);
        store.record(&handle, 42.0, 1_000_000).unwrap();
        DENY_ALLOC.store(false, Ordering::SeqCst);
    }
}
```

---

## Documentation

### In-code docs

Every public type, function, and module gets a doc comment. Use `#[doc = "..."]` or `///` style. Include examples for key APIs:

```rust
/// Record a value for a previously registered series.
///
/// This is the hot path. It writes directly to a pre-computed slot in the
/// memory-mapped ring buffer. No heap allocation, no syscall, no lock
/// contention in the single-writer case.
///
/// # Errors
///
/// Returns [`Error::InvalidHandle`] if the handle refers to a dropped series.
///
/// # Examples
///
///
/// # use rondo::{Store, SchemaConfig};
/// # let store = Store::open(path, &schemas)?;
/// let cpu = store.register("cpu_usage", &[("core", "0")])?;
/// store.record(&cpu, 73.5, timestamp)?;
///
pub fn record(&self, handle: &SeriesHandle, value: f64, timestamp: u64) -> Result<()> {
    // ...
}
```

### docs/ directory

- `architecture.md` — high-level design, storage model, data flow diagrams
- `storage-format.md` — byte-level slab format specification (enables third-party readers)
- `firecracker-integration.md` — how to deploy the FC agent
- `benchmarks.md` — methodology and results from comparative benchmarks

### README.md

Should cover: what it is (one paragraph), install, minimal usage example, link to docs, license. Keep it tight — detailed docs go in `docs/`.

---

## License

Dual MIT / Apache-2.0. This is the Rust ecosystem standard and matches rust-vmm's licensing. Include both license files at the repo root:

- `LICENSE-MIT`
- `LICENSE-APACHE`

Each crate's `Cargo.toml` references both:

```toml
license = "MIT OR Apache-2.0"
```

---

## Initial Repo Bootstrap

Quick start from zero:

```bash
# Create the repo
mkdir rondo && cd rondo
git init

# Create workspace
cat > Cargo.toml << 'EOF'
[workspace]
resolver = "2"
members = ["rondo", "rondo-cli"]

[workspace.package]
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/YOURUSER/rondo"
rust-version = "1.75"
EOF

# Create the library crate
cargo init rondo --lib
# Create the CLI crate
cargo init rondo-cli

# Toolchain
cat > rust-toolchain.toml << 'EOF'
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
EOF

# Git ignore
cat > .gitignore << 'EOF'
/target
*.swp
*.swo
.idea/
.vscode/
*.prof
*.svg
EOF

# First commit
git add -A
git commit -m "feat: initial workspace scaffold"
```

From there, start building the slab format and `record()` hot path — the foundation everything else depends on.
