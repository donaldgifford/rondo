#!/usr/bin/env bash
# Benchmark B: Resource overhead at scale.
#
# Spawns 10/50/100 concurrent rondo-demo-vmm instances, measures aggregate
# resource usage (RSS, CPU, FDs, disk), and compares against estimated
# Prometheus + node-exporter stack overhead.
#
# Prerequisites:
#   - Linux with KVM (/dev/kvm accessible)
#   - Guest kernel and initramfs built (run: cd rondo-demo-vmm/guest && ./build.sh)
#   - rondo-demo-vmm built in release mode (cargo build -p rondo-demo-vmm --release)
#
# Usage:
#   ./benchmark_scale.sh [--counts "10 50 100"] [--duration 15] [--kernel PATH] [--initramfs PATH]

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

COUNTS="${COUNTS:-10 50 100}"
WORKLOAD_DURATION="${DURATION:-15}"
KERNEL="${KERNEL:-${PROJECT_DIR}/rondo-demo-vmm/guest/out/bzImage}"
INITRAMFS="${INITRAMFS:-${PROJECT_DIR}/rondo-demo-vmm/guest/out/initramfs.cpio}"
VMM_BINARY="${VMM_BINARY:-${PROJECT_DIR}/target/release/rondo-demo-vmm}"
BASE_PORT=9200
BASE_DIR="/tmp/rondo_bench_scale"
CMDLINE_BASE="console=ttyS0 earlyprintk=ttyS0 reboot=k panic=1 noapic notsc clocksource=jiffies lpj=1000000 rdinit=/init"
SAMPLE_INTERVAL=2

# ── Parse arguments ───────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
	case "$1" in
	--counts)
		COUNTS="$2"
		shift 2
		;;
	--duration)
		WORKLOAD_DURATION="$2"
		shift 2
		;;
	--kernel)
		KERNEL="$2"
		shift 2
		;;
	--initramfs)
		INITRAMFS="$2"
		shift 2
		;;
	--binary)
		VMM_BINARY="$2"
		shift 2
		;;
	--help | -h)
		echo "Usage: $0 [--counts '10 50 100'] [--duration 15] [--kernel PATH] [--initramfs PATH]"
		exit 0
		;;
	*)
		echo "Unknown argument: $1" >&2
		exit 1
		;;
	esac
done

# ── Pre-flight checks ────────────────────────────────────────────────

preflight() {
	local ok=true

	if [[ ! -e /dev/kvm ]]; then
		echo "ERROR: /dev/kvm not found — KVM required" >&2
		ok=false
	fi
	if [[ ! -f "$KERNEL" ]]; then
		echo "ERROR: kernel not found at $KERNEL" >&2
		echo "  Run: cd rondo-demo-vmm/guest && ./build.sh" >&2
		ok=false
	fi
	if [[ ! -f "$INITRAMFS" ]]; then
		echo "ERROR: initramfs not found at $INITRAMFS" >&2
		echo "  Run: cd rondo-demo-vmm/guest && ./build.sh" >&2
		ok=false
	fi
	if [[ ! -x "$VMM_BINARY" ]]; then
		echo "ERROR: VMM binary not found at $VMM_BINARY" >&2
		echo "  Run: cargo build -p rondo-demo-vmm --release" >&2
		ok=false
	fi

	if [[ "$ok" != "true" ]]; then
		exit 1
	fi

	echo "Pre-flight OK"
	echo "  kernel:    $KERNEL"
	echo "  initramfs: $INITRAMFS"
	echo "  binary:    $VMM_BINARY"
	echo "  workload:  ${WORKLOAD_DURATION}s"
	echo "  counts:    $COUNTS"
	echo ""
}

# ── Resource sampling ─────────────────────────────────────────────────

# Sample aggregate resource usage for a list of PIDs.
# Writes CSV lines: timestamp,total_rss_kb,total_cpu_ticks,total_fds,alive_count
sample_resources() {
	local csv_file="$1"
	shift
	local pids=("$@")

	echo "timestamp,total_rss_kb,total_cpu_ticks,total_fds,alive_count" >"$csv_file"

	while [[ -f "${CONTROL_FILE}" ]]; do
		local ts
		ts=$(date +%s)
		local total_rss=0
		local total_cpu=0
		local total_fds=0
		local alive=0

		for pid in "${pids[@]}"; do
			if [[ -d "/proc/$pid" ]]; then
				alive=$((alive + 1))

				# RSS from /proc/PID/status (VmRSS in kB)
				local rss
				rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
				total_rss=$((total_rss + rss))

				# CPU ticks from /proc/PID/stat (field 14=utime + field 15=stime)
				local cpu
				cpu=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || echo 0)
				total_cpu=$((total_cpu + cpu))

				# Open file descriptors
				local fds
				fds=$(find "/proc/$pid/fd" -maxdepth 1 -mindepth 1 2>/dev/null | wc -l || echo 0)
				total_fds=$((total_fds + fds))
			fi
		done

		echo "${ts},${total_rss},${total_cpu},${total_fds},${alive}" >>"$csv_file"

		# Stop sampling if all processes exited
		if [[ $alive -eq 0 ]]; then
			break
		fi

		sleep "$SAMPLE_INTERVAL"
	done
}

# ── VMM launcher ──────────────────────────────────────────────────────

launch_vmms() {
	local count=$1
	local run_dir="$2"
	local pids=()

	mkdir -p "$run_dir/stores" "$run_dir/logs"

	for i in $(seq 1 "$count"); do
		local port=$((BASE_PORT + i))
		local store_dir="$run_dir/stores/vmm_${i}"
		local log_file="$run_dir/logs/vmm_${i}.log"

		"$VMM_BINARY" \
			--kernel "$KERNEL" \
			--initramfs "$INITRAMFS" \
			--metrics-store "$store_dir" \
			--api-port "$port" \
			--cmdline "$CMDLINE_BASE workload_duration=$WORKLOAD_DURATION" \
			>"$log_file" 2>&1 &

		pids+=("$!")

		# Small stagger to avoid thundering herd on KVM
		if [[ $((i % 10)) -eq 0 ]]; then
			sleep 0.2
		fi
	done

	echo "${pids[*]}"
}

# ── Cleanup ───────────────────────────────────────────────────────────

cleanup_pids() {
	local pids=("$@")
	for pid in "${pids[@]}"; do
		if kill -0 "$pid" 2>/dev/null; then
			kill "$pid" 2>/dev/null || true
		fi
	done
	# Give them a moment, then force-kill stragglers
	sleep 2
	for pid in "${pids[@]}"; do
		if kill -0 "$pid" 2>/dev/null; then
			kill -9 "$pid" 2>/dev/null || true
		fi
	done
}

# ── Analyze results ───────────────────────────────────────────────────

analyze_csv() {
	local csv_file="$1"
	local count="$2"

	# Skip header, find peak RSS and compute CPU delta
	local peak_rss=0
	local peak_fds=0
	local first_cpu=0
	local last_cpu=0
	local first_ts=0
	local last_ts=0
	local samples=0

	while IFS=',' read -r ts rss cpu fds alive; do
		[[ "$ts" == "timestamp" ]] && continue
		samples=$((samples + 1))

		if [[ $samples -eq 1 ]]; then
			first_cpu=$cpu
			first_ts=$ts
		fi
		last_cpu=$cpu
		last_ts=$ts

		if [[ $rss -gt $peak_rss ]]; then
			peak_rss=$rss
		fi
		if [[ $fds -gt $peak_fds ]]; then
			peak_fds=$fds
		fi
	done <"$csv_file"

	local duration=$((last_ts - first_ts))
	local cpu_ticks=$((last_cpu - first_cpu))
	# Convert ticks to seconds (100 ticks/sec on Linux)
	local cpu_seconds=0
	if [[ $duration -gt 0 ]]; then
		cpu_seconds=$((cpu_ticks / 100))
	fi

	local peak_rss_mb=$((peak_rss / 1024))
	local per_vm_rss_kb=$((peak_rss / count))

	echo "  Samples:        $samples (over ${duration}s)"
	echo "  Peak total RSS: ${peak_rss_mb} MB (${peak_rss} kB)"
	echo "  Per-VM RSS:     ${per_vm_rss_kb} kB ($((per_vm_rss_kb / 1024)) MB)"
	echo "  Peak total FDs: ${peak_fds}"
	echo "  Per-VM FDs:     $((peak_fds / count))"
	echo "  CPU time:       ${cpu_seconds}s over ${duration}s wall time"
}

# ── Prometheus comparison estimate ────────────────────────────────────

estimate_prometheus() {
	local count=$1

	# Conservative estimates from node_exporter and Prometheus docs:
	#   node_exporter: ~20-30 MB RSS per instance, ~3 MB/s scrape CPU
	#   Prometheus: ~100 MB base + ~2-5 MB per scrape target for TSDB
	#   Network: ~50 KB per scrape (typical node_exporter metrics page)
	local exporter_rss_mb=25
	local prometheus_base_mb=100
	local prometheus_per_target_mb=3
	local scrape_network_kb=50 # per scrape per target
	local scrape_interval_s=15

	local total_exporters_mb=$((exporter_rss_mb * count))
	local prometheus_mb=$((prometheus_base_mb + (prometheus_per_target_mb * count)))
	local total_mb=$((total_exporters_mb + prometheus_mb))

	# Network: targets * payload / interval = bandwidth
	local scrape_bandwidth_kbps=$((count * scrape_network_kb / scrape_interval_s))

	echo "  Estimated Prometheus + node-exporter stack:"
	echo "    ${count} node-exporters:  ${total_exporters_mb} MB RSS"
	echo "    1 Prometheus server:      ${prometheus_mb} MB RSS"
	echo "    Total estimated RSS:      ${total_mb} MB"
	echo "    Scrape bandwidth:         ${scrape_bandwidth_kbps} kB/s (${count} targets × ${scrape_network_kb}kB @ ${scrape_interval_s}s)"
	echo "    Scrape interval:          ${scrape_interval_s}s (vs rondo 1s embedded recording)"
	echo ""

	PROM_TOTAL_MB=$total_mb
}

# ── Disk usage measurement ────────────────────────────────────────────

measure_disk() {
	local run_dir="$1"
	local count="$2"

	if [[ -d "$run_dir/stores" ]]; then
		local total_kb
		total_kb=$(du -sk "$run_dir/stores" 2>/dev/null | awk '{print $1}')
		local total_mb=$((total_kb / 1024))
		local per_vm_kb=$((total_kb / count))
		echo "  Rondo store disk: ${total_mb} MB total (${per_vm_kb} kB per VM)"
	else
		echo "  Rondo store disk: (stores not found)"
	fi
}

# ── Run one benchmark round ───────────────────────────────────────────

run_benchmark() {
	local count=$1
	local results_dir="$2"
	local run_dir="$results_dir/run_${count}"

	mkdir -p "$run_dir"

	echo "=== $count VMMs ==="
	echo ""

	# Create control file for sampler
	CONTROL_FILE="$run_dir/.sampling"
	touch "$CONTROL_FILE"

	# Launch VMMs
	echo "Launching $count VMMs (workload: ${WORKLOAD_DURATION}s)..."
	local pid_str
	pid_str=$(launch_vmms "$count" "$run_dir")
	local pids
	read -ra pids <<<"$pid_str"
	echo "  Launched ${#pids[@]} VMMs"

	# Start resource sampling in background
	local csv_file="$run_dir/resources.csv"
	sample_resources "$csv_file" "${pids[@]}" &
	local sampler_pid=$!

	# Wait for all VMMs to finish (they exit after workload completes)
	local timeout=$((WORKLOAD_DURATION + 30)) # workload + boot + teardown headroom
	local waited=0
	while [[ $waited -lt $timeout ]]; do
		local alive=0
		for pid in "${pids[@]}"; do
			if kill -0 "$pid" 2>/dev/null; then
				alive=$((alive + 1))
			fi
		done
		if [[ $alive -eq 0 ]]; then
			break
		fi
		echo "  Waiting... ${alive}/$count VMMs still running (${waited}s elapsed)"
		sleep 5
		waited=$((waited + 5))
	done

	# Stop sampler
	rm -f "$CONTROL_FILE"
	sleep 1
	kill "$sampler_pid" 2>/dev/null || true
	wait "$sampler_pid" 2>/dev/null || true

	# Kill any stragglers
	cleanup_pids "${pids[@]}"

	# Count successful exits
	local successes=0
	local failures=0
	for i in $(seq 1 "$count"); do
		local log="$run_dir/logs/vmm_${i}.log"
		if [[ -f "$log" ]] && grep -q "VMM exited cleanly\|Guest workload complete" "$log" 2>/dev/null; then
			successes=$((successes + 1))
		else
			failures=$((failures + 1))
		fi
	done

	echo ""
	echo "  Results ($count VMMs, ${WORKLOAD_DURATION}s workload):"
	echo "  Completed: ${successes}/${count} (${failures} failures)"
	echo ""

	# Analyze resource usage
	echo "  --- Rondo (embedded) ---"
	if [[ -f "$csv_file" ]]; then
		analyze_csv "$csv_file" "$count"
	else
		echo "  (no resource data collected)"
	fi

	# Disk usage
	measure_disk "$run_dir" "$count"
	echo ""

	# Prometheus comparison
	echo "  --- Prometheus + node-exporter (estimated) ---"
	estimate_prometheus "$count"

	# Compute ratio
	if [[ -f "$csv_file" ]]; then
		local peak_rss
		peak_rss=$(awk -F, 'NR>1 {if($2>max)max=$2} END{print max+0}' "$csv_file")
		local rondo_mb=$((peak_rss / 1024))
		if [[ $rondo_mb -gt 0 && ${PROM_TOTAL_MB:-0} -gt 0 ]]; then
			local ratio=$((PROM_TOTAL_MB / rondo_mb))
			local pct=$((rondo_mb * 100 / PROM_TOTAL_MB))
			echo "  --- Comparison ---"
			echo "    Rondo embedded:   ${rondo_mb} MB"
			echo "    Prometheus stack:  ${PROM_TOTAL_MB} MB (estimated)"
			echo "    Ratio:            rondo uses ${pct}% of Prometheus stack memory"
			echo "    (${ratio}x less memory)"
		fi
	fi

	echo ""
	echo "---"
	echo ""
}

# ── Main ──────────────────────────────────────────────────────────────

main() {
	echo "╔══════════════════════════════════════════════════════════╗"
	echo "║  Benchmark B: Resource Overhead at Scale                ║"
	echo "║  rondo embedded metrics vs Prometheus + node-exporter   ║"
	echo "╚══════════════════════════════════════════════════════════╝"
	echo ""

	preflight

	local results_dir
	results_dir="${BASE_DIR}/results_$(date +%Y%m%d_%H%M%S)"
	mkdir -p "$results_dir"

	# System baseline
	echo "System baseline:"
	echo "  CPUs:   $(nproc)"
	echo "  Memory: $(awk '/MemTotal/ {printf "%.1f GB", $2/1024/1024}' /proc/meminfo)"
	echo "  Kernel: $(uname -r)"
	echo ""

	# Run benchmarks for each count
	for count in $COUNTS; do
		run_benchmark "$count" "$results_dir"
		# Clean up stores between runs
		rm -rf "${BASE_DIR}/run_${count}/stores" 2>/dev/null || true
		sleep 2
	done

	echo "╔══════════════════════════════════════════════════════════╗"
	echo "║  Benchmark B Complete                                   ║"
	echo "╚══════════════════════════════════════════════════════════╝"
	echo ""
	echo "Results saved to: $results_dir"
	echo ""

	# Summary table
	echo "Summary:"
	printf "  %-8s %-12s %-12s %-12s %-10s\n" "VMs" "Rondo RSS" "Prom Est." "Ratio" "Rondo %"
	printf "  %-8s %-12s %-12s %-12s %-10s\n" "---" "---------" "---------" "-----" "-------"

	for count in $COUNTS; do
		local csv_file="$results_dir/run_${count}/resources.csv"
		if [[ -f "$csv_file" ]]; then
			local peak_rss
			peak_rss=$(awk -F, 'NR>1 {if($2>max)max=$2} END{print max+0}' "$csv_file")
			local rondo_mb=$((peak_rss / 1024))

			# Recalculate Prometheus estimate
			local prom_mb=$((25 * count + 100 + 3 * count))

			if [[ $rondo_mb -gt 0 ]]; then
				local ratio=$((prom_mb / rondo_mb))
				local pct=$((rondo_mb * 100 / prom_mb))
				printf "  %-8s %-12s %-12s %-12s %-10s\n" \
					"$count" "${rondo_mb} MB" "${prom_mb} MB" "${ratio}x" "${pct}%"
			else
				printf "  %-8s %-12s %-12s %-12s %-10s\n" \
					"$count" "N/A" "${prom_mb} MB" "N/A" "N/A"
			fi
		fi
	done
	echo ""
}

main "$@"
