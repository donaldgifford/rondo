#!/usr/bin/env bash
# Build a minimal guest kernel and BusyBox initramfs for rondo-demo-vmm.
#
# Usage:
#   ./build.sh                # build both kernel and initramfs
#   ./build.sh kernel         # build kernel only
#   ./build.sh initramfs      # build initramfs only
#
# Prerequisites (Ubuntu/Debian):
#   sudo apt install build-essential flex bison bc libelf-dev libssl-dev \
#                    cpio wget
#
# Output:
#   out/bzImage        — kernel image
#   out/initramfs.cpio — compressed initramfs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"
OUT_DIR="${SCRIPT_DIR}/out"

KERNEL_VERSION="${KERNEL_VERSION:-6.6.70}"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz"

BUSYBOX_VERSION="${BUSYBOX_VERSION:-1.36.1}"
BUSYBOX_URL="https://busybox.net/downloads/busybox-${BUSYBOX_VERSION}.tar.bz2"

NPROC="$(nproc 2>/dev/null || echo 4)"

mkdir -p "${BUILD_DIR}" "${OUT_DIR}"

# ── Kernel ───────────────────────────────────────────────────────────

build_kernel() {
    local src="${BUILD_DIR}/linux-${KERNEL_VERSION}"

    if [ ! -d "${src}" ]; then
        echo "==> Downloading kernel ${KERNEL_VERSION}..."
        wget -qO- "${KERNEL_URL}" | tar -xJ -C "${BUILD_DIR}"
    fi

    echo "==> Configuring kernel..."
    cd "${src}"

    # Start with a minimal KVM guest config
    make defconfig
    # Apply our overrides for a tiny, fast-booting guest
    "${SCRIPT_DIR}/kernel-config.sh" .config

    echo "==> Building kernel (${NPROC} jobs)..."
    make -j"${NPROC}" bzImage

    cp arch/x86/boot/bzImage "${OUT_DIR}/bzImage"
    echo "==> Kernel built: ${OUT_DIR}/bzImage"
}

# ── Initramfs ────────────────────────────────────────────────────────

build_initramfs() {
    local src="${BUILD_DIR}/busybox-${BUSYBOX_VERSION}"
    local rootfs="${BUILD_DIR}/rootfs"

    if [ ! -d "${src}" ]; then
        echo "==> Downloading BusyBox ${BUSYBOX_VERSION}..."
        wget -qO- "${BUSYBOX_URL}" | tar -xj -C "${BUILD_DIR}"
    fi

    echo "==> Building BusyBox (static)..."
    cd "${src}"
    make defconfig
    # Enable static linking
    sed -i 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config
    make -j"${NPROC}"

    echo "==> Creating initramfs rootfs..."
    rm -rf "${rootfs}"
    mkdir -p "${rootfs}"/{bin,sbin,etc,proc,sys,dev,tmp}

    cp "${src}/busybox" "${rootfs}/bin/busybox"
    # Create symlinks for common commands
    for cmd in sh ash cat echo ls mkdir mount umount sleep date \
               dmesg poweroff reboot dd; do
        ln -sf busybox "${rootfs}/bin/${cmd}"
    done

    # Copy our init script
    cp "${SCRIPT_DIR}/init" "${rootfs}/init"
    chmod +x "${rootfs}/init"

    # Copy the workload script
    cp "${SCRIPT_DIR}/workload.sh" "${rootfs}/workload.sh"
    chmod +x "${rootfs}/workload.sh"

    echo "==> Packing initramfs..."
    cd "${rootfs}"
    find . | cpio -o -H newc 2>/dev/null | gzip > "${OUT_DIR}/initramfs.cpio.gz"
    echo "==> Initramfs built: ${OUT_DIR}/initramfs.cpio.gz"
}

# ── Main ─────────────────────────────────────────────────────────────

case "${1:-all}" in
    kernel)    build_kernel ;;
    initramfs) build_initramfs ;;
    all)       build_kernel; build_initramfs ;;
    *)         echo "Usage: $0 [kernel|initramfs|all]"; exit 1 ;;
esac

echo "==> Done."
echo ""
echo "Run the VMM with:"
echo "  cargo run -p rondo-demo-vmm -- \\"
echo "    --kernel ${OUT_DIR}/bzImage \\"
echo "    --initramfs ${OUT_DIR}/initramfs.cpio.gz"
