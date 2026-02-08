#!/bin/sh
# Synthetic workload for rondo-demo-vmm guest.
# Produces distinct patterns visible in rondo metrics.
#
# Uses $WORKLOAD_DURATION (seconds, default 18) set by init from kernel cmdline.
# Phases are distributed proportionally:
#   Phase 1: CPU burst  — 28% of total (rapid syscalls)
#   Phase 2: Idle       — 17% of total (HLT-heavy execution)
#   Phase 3: I/O sim    — 28% of total (read /dev/zero)
#   Phase 4: Mixed      — 28% of total (alternating CPU and idle)

TOTAL="${WORKLOAD_DURATION:-18}"

# Compute phase durations (integer arithmetic, busybox sh compatible)
# Phase 1: 28% — multiply by 28, divide by 100
P1=$(( TOTAL * 28 / 100 ))
# Phase 2: 17%
P2=$(( TOTAL * 17 / 100 ))
# Phase 3: 28%
P3=$(( TOTAL * 28 / 100 ))
# Phase 4: remainder (ensures total adds up exactly)
P4=$(( TOTAL - P1 - P2 - P3 ))

# Ensure minimum 1s per phase
[ "$P1" -lt 1 ] && P1=1
[ "$P2" -lt 1 ] && P2=1
[ "$P3" -lt 1 ] && P3=1
[ "$P4" -lt 1 ] && P4=1

echo "[workload] total=${TOTAL}s phases: cpu=${P1}s idle=${P2}s io=${P3}s mixed=${P4}s"

echo "[workload] phase 1: CPU burst (${P1}s)"
END=$(($(date +%s) + P1))
while [ "$(date +%s)" -lt "$END" ]; do
    cat /proc/uptime > /dev/null
done

echo "[workload] phase 2: idle (${P2}s)"
sleep "$P2"

echo "[workload] phase 3: I/O simulation (${P3}s)"
if [ -b /dev/vda ]; then
    echo "[workload]   using /dev/vda (virtio-blk)"
    END=$(($(date +%s) + P3))
    while [ "$(date +%s)" -lt "$END" ]; do
        # Read from block device, write to block device
        dd if=/dev/vda of=/dev/null bs=4096 count=64 2>/dev/null
        dd if=/dev/zero of=/dev/vda bs=4096 count=64 seek=128 2>/dev/null
    done
else
    END=$(($(date +%s) + P3))
    while [ "$(date +%s)" -lt "$END" ]; do
        dd if=/dev/zero of=/dev/null bs=4096 count=256 2>/dev/null
    done
fi

echo "[workload] phase 4: mixed (${P4}s)"
END=$(($(date +%s) + P4))
while [ "$(date +%s)" -lt "$END" ]; do
    cat /proc/uptime > /dev/null
    sleep 1
done

echo "[workload] done"
