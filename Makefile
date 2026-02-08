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

# Base kernel command line for the demo VMM guest
VMM_CMDLINE_BASE := console=ttyS0 earlyprintk=ttyS0 reboot=k panic=1 noapic notsc clocksource=jiffies lpj=1000000 rdinit=/init
VMM_KERNEL       := rondo-demo-vmm/guest/out/bzImage
VMM_INITRAMFS    := rondo-demo-vmm/guest/out/initramfs.cpio

# Default Prometheus remote-write endpoint on the remote box
VMM_REMOTE_WRITE := https://prometheus.fartlab.dev/api/v1/write

.PHONY: help build test clippy bench \
        vmm-sync vmm-build vmm-test vmm-clippy vmm-run vmm-bench vmm-shell \
        vmm-demo vmm-demo-disk vmm-demo-query vmm-demo-remote-write \
        vmm-bench-15 vmm-bench-30 vmm-bench-45 \
        vmm-bench-capture vmm-bench-scale clean

# ─── Local (macOS) ─────────────────────────────────────────────────────

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

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

vmm-bench: vmm-sync ## Run write-path benchmarks on remote Linux box
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo run -p rondo --release --example benchmark_comparison"

vmm-shell: ## Open SSH shell on remote Linux box in project dir
	ssh -t $(VMM_HOST) "cd $(VMM_DIR) && exec $$SHELL -l"

vmm-check-kvm: ## Verify KVM is available on remote box
	ssh $(VMM_HOST) "ls -l /dev/kvm && lscpu | grep -i virtual"

vmm-guest: vmm-sync ## Build guest kernel and initramfs on remote box
	ssh $(VMM_HOST) "cd $(VMM_DIR)/rondo-demo-vmm/guest && ./build.sh"

vmm-demo: vmm-build ## Build guest, run VMM demo end-to-end, query metrics afterward
	ssh $(VMM_HOST) "$(VMM_CARGO) && \
		cd rondo-demo-vmm/guest && ./build.sh && cd $(VMM_DIR) && \
		rm -rf vmm_metrics && \
		cargo run -p rondo-demo-vmm -- \
			--kernel $(VMM_KERNEL) \
			--initramfs $(VMM_INITRAMFS) && \
		echo '' && echo '=== Post-run metrics store ===' && \
		cargo run -p rondo-cli -- info vmm_metrics && \
		echo '' && echo '=== vCPU IO exits (tier 0) ===' && \
		cargo run -p rondo-cli -- query vmm_metrics 'vcpu_exits_total{reason=io}' --range all --tier 0 --format csv | tail -20"

vmm-demo-disk: vmm-build ## Run VMM demo with virtio-blk disk device
	ssh $(VMM_HOST) "$(VMM_CARGO) && \
		cd rondo-demo-vmm/guest && ./build.sh && cd $(VMM_DIR) && \
		rm -rf vmm_metrics && \
		cargo run -p rondo-demo-vmm -- \
			--kernel $(VMM_KERNEL) \
			--initramfs $(VMM_INITRAMFS) \
			--disk vmm_disk.img && \
		echo '' && echo '=== Post-run metrics store ===' && \
		cargo run -p rondo-cli -- info vmm_metrics && \
		echo '' && echo '=== Block I/O reads (tier 0) ===' && \
		cargo run -q -p rondo-cli -- query vmm_metrics 'blk_requests_total{op=read}' --range all --tier 0 --format csv 2>/dev/null | tail -20"

vmm-demo-query: ## Query metrics store on remote box (after vmm-demo)
	ssh $(VMM_HOST) "$(VMM_CARGO) && \
		cargo run -p rondo-cli -- info vmm_metrics && \
		echo '' && echo '=== Series list ===' && \
		cargo run -p rondo-cli -- info vmm_metrics 2>&1 | grep -E '^\s+-' || true"

vmm-demo-remote-write: vmm-build ## Run VMM demo with Prometheus remote-write export
	ssh $(VMM_HOST) "$(VMM_CARGO) && \
		cd rondo-demo-vmm/guest && ./build.sh && cd $(VMM_DIR) && \
		rm -rf vmm_metrics && \
		cargo run -p rondo-demo-vmm -- \
			--kernel $(VMM_KERNEL) \
			--initramfs $(VMM_INITRAMFS) \
			--cmdline '$(VMM_CMDLINE_BASE) workload_duration=45' \
			--disk vmm_disk.img \
			--remote-write $(VMM_REMOTE_WRITE) && \
		echo '' && echo '=== Post-run metrics store ===' && \
		cargo run -p rondo-cli -- info vmm_metrics && \
		echo '' && echo '=== Export cursor ===' && \
		cat vmm_metrics/cursor_prometheus.json 2>/dev/null || echo '(no cursor — remote-write may not have pushed yet)'"

vmm-ssh: ## Run arbitrary command on remote box (pass CMD=)
	ssh $(VMM_HOST) "$(VMM_CARGO) && $(CMD)"

# ─── VMM Lifecycle Benchmarks ────────────────────────────────────────

# Helper: run VMM with a specific workload duration, then query and count data points.
# Usage: $(call run-vmm-bench,DURATION_SECONDS)
define run-vmm-bench
	ssh $(VMM_HOST) "$(VMM_CARGO) && \
		cd rondo-demo-vmm/guest && ./build.sh && cd $(VMM_DIR) && \
		rm -rf vmm_metrics && \
		cargo run -p rondo-demo-vmm -- \
			--kernel $(VMM_KERNEL) \
			--initramfs $(VMM_INITRAMFS) \
			--cmdline '$(VMM_CMDLINE_BASE) workload_duration=$(1)' && \
		echo '' && echo '=== $(1)s VM lifecycle: metrics store ===' && \
		cargo run -p rondo-cli -- info vmm_metrics && \
		echo '' && echo '=== Data point counts ===' && \
		cargo run -q -p rondo-cli -- query vmm_metrics vmm_uptime_seconds --range all --tier 0 --format csv 2>/dev/null | grep -c '^[0-9]' | xargs -I{} echo 'vmm_uptime_seconds: {} data points (expected ~$(1))' "
endef

vmm-bench-15: vmm-build ## Run 15s VM lifecycle benchmark
	$(call run-vmm-bench,15)

vmm-bench-30: vmm-build ## Run 30s VM lifecycle benchmark
	$(call run-vmm-bench,30)

vmm-bench-45: vmm-build ## Run 45s VM lifecycle benchmark
	$(call run-vmm-bench,45)

vmm-bench-capture: vmm-build ## Run 15/30/45s benchmarks and compare capture rates
	@echo "=== Benchmark C: Ephemeral VM Data Capture ==="
	@echo ""
	@echo "--- 15s VM lifecycle ---"
	$(call run-vmm-bench,15)
	@echo ""
	@echo "--- 30s VM lifecycle ---"
	$(call run-vmm-bench,30)
	@echo ""
	@echo "--- 45s VM lifecycle ---"
	$(call run-vmm-bench,45)
	@echo ""
	@echo "=== Benchmark C complete ==="

# ─── Scale Benchmark ─────────────────────────────────────────────────

vmm-bench-scale: vmm-sync vmm-build-release ## Run scale benchmark (10/50/100 VMMs)
	ssh $(VMM_HOST) "$(VMM_CARGO) && \
		cd rondo-demo-vmm/guest && ./build.sh && cd $(VMM_DIR) && \
		./scripts/benchmark_scale.sh \
			--counts '10 50 100' \
			--duration 15 \
			--kernel $(VMM_DIR)/$(VMM_KERNEL) \
			--initramfs $(VMM_DIR)/$(VMM_INITRAMFS)"

# ─── Cleanup ──────────────────────────────────────────────────────────

clean: ## Remove local build artifacts
	cargo clean

vmm-clean: ## Remove remote build artifacts
	ssh $(VMM_HOST) "$(VMM_CARGO) && cargo clean"
