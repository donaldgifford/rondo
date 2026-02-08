# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rondo is an embedded round-robin time-series storage engine written in Rust. It targets VMMs, dataplanes, and performance-critical systems — think rrdtool's storage philosophy with a modern dimensional data model. The project is in early development.

## Build & Development Commands

```bash
# Build
cargo build --workspace

# Test (prefer nextest)
cargo nextest run --workspace
cargo nextest run -p rondo test_name        # single test
cargo test --workspace --doc                 # doctests only

# Lint & format
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check

# Full CI check locally
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check && cargo nextest run --workspace

# Benchmarks
cargo bench -p rondo -- record

# Coverage
cargo llvm-cov --workspace --lcov --output-path lcov.info

# Changelog (generate from conventional commits)
git cliff --output CHANGELOG.md              # full changelog
git cliff --latest --strip header            # latest release notes only

# Watch mode
cargo watch -x "nextest run" -x "clippy --workspace"
```

## Workspace Structure

Cargo workspace (resolver v2) with two active crates:
- **`rondo/`** — Core library crate. All storage engine logic lives here.
- **`rondo-cli/`** — CLI tool for interacting with rondo stores.

Future crates (commented out in workspace): `rondo-fc-agent` (Firecracker agent), `rondo-demo-vmm`.

### Workspace Dependencies

Dependencies are pinned at the workspace level in the root `Cargo.toml` and inherited by crates with `{ workspace = true }`. Active dependencies: memmap2, serde (+derive), serde_json, thiserror, tracing, tracing-subscriber (+env-filter), clap (+derive), tokio (+full), criterion (+html_reports).

## Architecture (Planned)

The storage engine is built around these core abstractions (see `docs/MVP.md` for full spec):

- **Store** — Top-level handle; opens a directory, owns schemas and series
- **Schema** — Defines retention tiers and consolidation functions for a class of metrics
- **Series** — Individual time-series identified by name + dimensional labels (e.g., `vcpu_steal_ns{instance="vm-abc", vcpu="0"}`)
- **Slab** — Memory-mapped file backing a tier; fixed-size ring buffer with columnar layout
- **Tier** — Resolution level within a schema (e.g., 1s/10m, 10s/6h, 5m/7d)

**Key design constraints:**
- Single-writer assumption (no locking on hot path)
- Zero heap allocation on the `record()` write path
- No background threads — caller drives all maintenance via `consolidate()`
- Fixed, bounded storage size determined by schema configuration

## Coding Standards

- **Edition**: Rust 2024, MSRV 1.92
- **Line width**: 100 characters (rustfmt.toml)
- **Unsafe**: All unsafe blocks must have `// SAFETY:` comments (`undocumented_unsafe_blocks = "deny"`). Unsafe ops in unsafe fns are denied.
- **Docs**: `missing_docs` is warned — all public items need doc comments
- **Clippy**: Correctness lints are denied. Performance lints are warned (this is a perf-critical project). Lint groups use `priority = -1` so individual lint overrides take precedence. See `Cargo.toml` `[workspace.lints]` for the full policy.
- **Dependencies**: Must be MIT/Apache-2.0/BSD compatible — no copyleft (cargo-deny enforces this via `deny.toml`). No unknown registries or git sources.

## CI

Three GitHub Actions workflows in `.github/workflows/`:
- **ci.yml** — Format check, TOML format (taplo), clippy, cargo-deny, nextest, doctests, coverage (llvm-cov → codecov), benchmarks (main only), MSRV check (main only)
- **audit.yml** — Weekly security audit + on Cargo.toml/lock changes
- **release.yml** — On `v*` tags: generates release notes via git-cliff, creates GitHub release, publishes to crates.io

## Git Conventions

- Conventional commits: `feat(scope):`, `fix(scope):`, `perf(scope):`, `test(scope):`, `docs:`, `ci:`, `chore:`, `refactor:`
- Scopes: `store`, `slab`, `ring`, `series`, `schema`, `consolidate`, `query`, `export`, `cli`, `fc-agent`, `vmm`
- Trunk-based development on `main`
- Dual license: MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`)

## Remote VMM Development

The demo VMM (`rondo-demo-vmm`) requires Linux/KVM and is developed on a remote Linux box. **Always use Makefile targets for remote operations** — never SSH directly.

```bash
# Remote operations (all via Makefile)
make vmm-sync                     # rsync source to remote
make vmm-build                    # sync + build on remote
make vmm-build-release            # sync + release build
make vmm-test                     # sync + run all tests
make vmm-test-vmm                 # sync + run VMM tests only
make vmm-clippy                   # sync + clippy on remote
make vmm-run ARGS="--kernel ..."  # sync + build + run VMM
make vmm-guest                    # sync + build guest initramfs
make vmm-bench                    # sync + run benchmarks
make vmm-shell                    # open SSH shell on remote
make vmm-ssh CMD="..."            # run arbitrary command on remote
make vmm-check-kvm                # verify KVM availability
```

**Important**: `vmm-run` and `vmm-build` do `rsync --delete` which removes guest build artifacts (`out/`). Rebuild the guest after sync with `make vmm-guest`, or use `make vmm-ssh CMD="..."` to avoid re-syncing.

## Key Design Documents

- `docs/VISION.md` — Project vision and non-goals
- `docs/MVP.md` — Detailed MVP plan with API surface, storage layout, and milestones
- `docs/MVP-ALT.md` — Alternative design exploration
- `docs/REPO_SETUP.md` — Full development guide, CI specs, testing strategy
