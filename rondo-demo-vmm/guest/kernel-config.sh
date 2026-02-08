#!/usr/bin/env bash
# Apply minimal kernel config overrides for a KVM guest.
# Usage: kernel-config.sh <.config>
set -euo pipefail

CONFIG="${1:-.config}"

enable()  { sed -i "s/.*${1}.*/${1}=y/"        "${CONFIG}" 2>/dev/null || echo "${1}=y"  >> "${CONFIG}"; }
disable() { sed -i "s/.*${1}.*/${1}=n/"        "${CONFIG}" 2>/dev/null || echo "${1}=n"  >> "${CONFIG}"; }
setval()  { sed -i "s/.*${1}.*/${1}=${2}/"     "${CONFIG}" 2>/dev/null || echo "${1}=${2}" >> "${CONFIG}"; }

# ── Guest essentials ─────────────────────────────────────────────────
enable  CONFIG_KVM_GUEST
enable  CONFIG_PARAVIRT
enable  CONFIG_HYPERVISOR_GUEST

# ── Serial console ───────────────────────────────────────────────────
enable  CONFIG_SERIAL_8250
enable  CONFIG_SERIAL_8250_CONSOLE
enable  CONFIG_EARLY_PRINTK

# ── Virtio (for future block device support) ─────────────────────────
enable  CONFIG_VIRTIO
enable  CONFIG_VIRTIO_PCI
enable  CONFIG_VIRTIO_MMIO
enable  CONFIG_VIRTIO_BLK
enable  CONFIG_VIRTIO_NET
enable  CONFIG_VIRTIO_CONSOLE

# ── Minimal RAM disk for initramfs ───────────────────────────────────
enable  CONFIG_BLK_DEV_INITRD
enable  CONFIG_BLK_DEV_RAM
enable  CONFIG_RD_GZIP

# ── Trim unnecessary subsystems ──────────────────────────────────────
disable CONFIG_MODULES
disable CONFIG_SOUND
disable CONFIG_USB_SUPPORT
disable CONFIG_WIRELESS
disable CONFIG_WLAN
disable CONFIG_DRM
disable CONFIG_FB
disable CONFIG_VGA_CONSOLE
disable CONFIG_INPUT_MOUSE
disable CONFIG_INPUT_JOYSTICK

# ── Fast boot ────────────────────────────────────────────────────────
disable CONFIG_DEBUG_KERNEL
setval  CONFIG_LOG_BUF_SHIFT 14

echo "Kernel config updated: ${CONFIG}"
