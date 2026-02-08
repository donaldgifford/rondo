#!/usr/bin/env bash
# Build a minimal initramfs for rondo-demo-vmm using the host kernel and busybox.
#
# Usage:
#   ./build.sh              # build initramfs, symlink host kernel
#
# Prerequisites: busybox installed (apt install busybox-static)
#
# Output:
#   out/bzImage            — symlink to host kernel
#   out/initramfs.cpio.gz  — minimal initramfs with workload

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="${SCRIPT_DIR}/out"
ROOTFS="${SCRIPT_DIR}/build/rootfs"

KERNEL="$(ls /boot/vmlinuz-* 2>/dev/null | sort -V | tail -1)"
BUSYBOX="$(which busybox 2>/dev/null || echo /usr/bin/busybox)"

if [ -z "${KERNEL}" ]; then
    echo "error: no kernel found in /boot/vmlinuz-*" >&2
    exit 1
fi
if [ ! -x "${BUSYBOX}" ]; then
    echo "error: busybox not found — install with: apt install busybox-static" >&2
    exit 1
fi

mkdir -p "${OUT_DIR}"

# ── Kernel — just symlink the host's ─────────────────────────────────
ln -sf "${KERNEL}" "${OUT_DIR}/bzImage"
echo "kernel: ${KERNEL}"

# ── Initramfs ────────────────────────────────────────────────────────
echo "building initramfs..."
rm -rf "${ROOTFS}"
mkdir -p "${ROOTFS}"/{bin,sbin,etc,proc,sys,dev,tmp}

cp "${BUSYBOX}" "${ROOTFS}/bin/busybox"
for cmd in sh ash cat echo ls mkdir mount umount sleep date \
           dmesg poweroff reboot dd; do
    ln -sf busybox "${ROOTFS}/bin/${cmd}"
done

cp "${SCRIPT_DIR}/init"        "${ROOTFS}/init"
cp "${SCRIPT_DIR}/workload.sh" "${ROOTFS}/workload.sh"
chmod +x "${ROOTFS}/init" "${ROOTFS}/workload.sh"

cd "${ROOTFS}"
find . | cpio -o -H newc 2>/dev/null | gzip > "${OUT_DIR}/initramfs.cpio.gz"

echo "initramfs: ${OUT_DIR}/initramfs.cpio.gz"
echo ""
echo "Run with:"
echo "  cargo run -p rondo-demo-vmm -- \\"
echo "    --kernel ${OUT_DIR}/bzImage \\"
echo "    --initramfs ${OUT_DIR}/initramfs.cpio.gz"
