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
#   out/initramfs.cpio     — minimal initramfs with workload

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="${SCRIPT_DIR}/out"
ROOTFS="${SCRIPT_DIR}/build/rootfs"

KERNEL="$(ls /boot/vmlinuz-* 2>/dev/null | sort -V | tail -1)"
# Prefer busybox-static for initramfs (no dynamic linker needed)
if [ -x /bin/busybox-static ]; then
    BUSYBOX=/bin/busybox-static
elif [ -x /usr/bin/busybox-static ]; then
    BUSYBOX=/usr/bin/busybox-static
else
    BUSYBOX="$(which busybox 2>/dev/null || echo /usr/bin/busybox)"
fi

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

# ── Virtio kernel modules (for virtio-blk support) ──────────────────
# Copy and decompress virtio modules so the guest can load them with insmod.
# If modules are built-in to the kernel, these won't exist and that's fine.
KERNEL_VERSION="$(basename "${KERNEL}" | sed 's/vmlinuz-//')"
MODULES_DIR="/lib/modules/${KERNEL_VERSION}"
mkdir -p "${ROOTFS}/lib/modules"
for mod in virtio virtio_ring virtio_mmio virtio_blk; do
    modpath="$(find "${MODULES_DIR}" -name "${mod}.ko*" 2>/dev/null | head -1)"
    if [ -n "${modpath}" ]; then
        case "${modpath}" in
            *.zst) zstd -d -q "${modpath}" -o "${ROOTFS}/lib/modules/${mod}.ko" 2>/dev/null ;;
            *.xz)  xz -d -c "${modpath}" > "${ROOTFS}/lib/modules/${mod}.ko" 2>/dev/null ;;
            *.gz)  gzip -d -c "${modpath}" > "${ROOTFS}/lib/modules/${mod}.ko" 2>/dev/null ;;
            *)     cp "${modpath}" "${ROOTFS}/lib/modules/${mod}.ko" ;;
        esac
        echo "  module: ${mod} (from ${modpath})"
    fi
done

# Add insmod symlink if not already present
ln -sf busybox "${ROOTFS}/bin/insmod" 2>/dev/null || true

cd "${ROOTFS}"
# Sort entries for reproducibility; use --quiet to suppress byte count.
# Using uncompressed cpio — kernel handles it natively without decompressor.
find . -print0 | sort -z | cpio --null -o -H newc --quiet > "${OUT_DIR}/initramfs.cpio"

echo "initramfs: ${OUT_DIR}/initramfs.cpio"
echo ""
echo "Run with:"
echo "  cargo run -p rondo-demo-vmm -- \\"
echo "    --kernel ${OUT_DIR}/bzImage \\"
echo "    --initramfs ${OUT_DIR}/initramfs.cpio"
