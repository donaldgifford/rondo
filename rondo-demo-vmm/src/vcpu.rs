//! vCPU thread implementation.
//!
//! Runs the KVM_RUN loop, handles vCPU exits (IO, MMIO, HLT, shutdown),
//! and records metrics for each exit via the `VmMetrics` wrapper.
//!
//! This module is only compiled on Linux (requires KVM).

// TODO(phase4): Implement task 4.3 (vCPU thread):
// - KVM_RUN loop with exit handling
// - Serial console output via vm-superio
// - Task 4.6 (vCPU exit instrumentation):
//   - Record vcpu_exits_total by exit reason
//   - Record vcpu_exit_duration_ns per exit
//   - Record vcpu_run_duration_ns (time in KVM_RUN)
