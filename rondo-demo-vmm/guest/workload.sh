#!/bin/sh
# Synthetic workload for rondo-demo-vmm guest.
# Produces distinct patterns visible in rondo metrics:
#   Phase 1: CPU burst (rapid syscalls for ~5s)
#   Phase 2: Idle period (~3s of HLT-heavy execution)
#   Phase 3: I/O simulation (read /dev/zero for ~5s)
#   Phase 4: Mixed load (~5s alternating CPU and idle)

echo "[workload] phase 1: CPU burst (5s)"
END=$(($(date +%s) + 5))
while [ "$(date +%s)" -lt "$END" ]; do
    cat /proc/uptime > /dev/null
done

echo "[workload] phase 2: idle (3s)"
sleep 3

echo "[workload] phase 3: I/O simulation (5s)"
END=$(($(date +%s) + 5))
while [ "$(date +%s)" -lt "$END" ]; do
    dd if=/dev/zero of=/dev/null bs=4096 count=256 2>/dev/null
done

echo "[workload] phase 4: mixed (5s)"
END=$(($(date +%s) + 5))
while [ "$(date +%s)" -lt "$END" ]; do
    cat /proc/uptime > /dev/null
    sleep 1
done

echo "[workload] done"
