# rondo — Build and development automation
#
# Local targets work on macOS. VMM targets sync source to a remote Linux box
# with KVM and build/test/run there.

# Remote Linux box for VMM development
VMM_HOST     := donald@10.10.11.33
VMM_DIR      := ~/rondo
VMM_CARGO    := . ~/.cargo/env && cd $(VMM_DIR)

# Exclude patterns for rsync
RSYNC_EXCLUDE := --exclude target/ --exclude .git/ --exclude .claude/ \
                 --exclude consolidation_demo_store/

.PHONY: help build test clippy bench \
        vmm-sync vmm-build vmm-test vmm-clippy vmm-run vmm-bench vmm-shell \
        clean

# ─── Local (macOS) ─────────────────────────────────────────────────────

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

build: ## Build all workspace crates (local)
	cargo build --workspace

test: ## Run all tests (local)
	cargo nextest run --workspace

clippy: ## Run clippy on workspace (local)
	cargo clippy --workspace --all-targets -- -D warnings

bench: ## Run criterion benchmarks (local)
	cargo bench -p rondo

# ─── Remote VMM (Linux/KVM) ───────────────────────────────────────────

vmm-sync: ## Sync source to remote Linux box
	rsync -az --delete $(RSYNC_EXCLUDE) ./ $(VMM_HOST):$(VMM_DIR)/

vmm-build: vmm-sync ## Build rondo-demo-vmm on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo build -p rondo-demo-vmm"

vmm-build-release: vmm-sync ## Build rondo-demo-vmm (release) on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo build -p rondo-demo-vmm --release"

vmm-test: vmm-sync ## Run all tests on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo nextest run --workspace"

vmm-test-vmm: vmm-sync ## Run only demo-vmm tests on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo nextest run -p rondo-demo-vmm"

vmm-clippy: vmm-sync ## Run clippy on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo clippy --workspace --all-targets -- -D warnings"

vmm-run: vmm-build ## Run the demo VMM on remote Linux box (pass ARGS=)
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo run -p rondo-demo-vmm -- $(ARGS)"

vmm-bench: vmm-sync ## Run benchmarks on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo run -p rondo --release --example benchmark_comparison"

vmm-shell: ## Open SSH shell on remote Linux box in project dir
	ssh -t $(VMM_HOST) "cd $(VMM_DIR) && exec $$SHELL -l"

vmm-check-kvm: ## Verify KVM is available on remote box
	ssh $(VMM_HOST) "ls -l /dev/kvm && lscpu | grep -i virtual"

vmm-guest: vmm-sync ## Build guest kernel and initramfs on remote box
	ssh $(VMM_HOST) "cd $(VMM_DIR)/rondo-demo-vmm/guest && ./build.sh"

vmm-ssh: ## Run arbitrary command on remote box (pass CMD=)
	ssh $(VMM_HOST) "$(VMM_CARGO) && $(CMD)"

# ─── Cleanup ──────────────────────────────────────────────────────────

clean: ## Remove local build artifacts
	cargo clean

vmm-clean: ## Remove remote build artifacts
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo clean"
