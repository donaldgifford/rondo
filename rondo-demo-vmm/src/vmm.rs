//! Minimal VMM boot implementation using rust-vmm crates.
//!
//! Creates a KVM VM, configures memory regions, loads a bzImage kernel,
//! and boots the guest to a serial console.
//!
//! This module is only compiled on Linux (requires KVM).

// TODO(phase4): Implement tasks 4.2 (VMM boot):
// - Create KVM VM via kvm-ioctls
// - Configure memory regions via vm-memory
// - Set up CPUID, MSRs, special registers for x86_64 boot
// - Load bzImage kernel via linux-loader
// - Load initramfs into guest memory
