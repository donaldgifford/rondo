//! vCPU configuration and run loop.
//!
//! Sets up x86_64 page tables, GDT, segment registers, and general-purpose
//! registers for the Linux 64-bit boot protocol. Provides the KVM_RUN loop
//! with serial console output and metrics recording for every vCPU exit.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, kvm_dtable, kvm_regs, kvm_segment};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::devices::block::{self, VirtioBlock};
use crate::metrics::{BlkOp, VcpuExitReason, VmMetrics};
use crate::vmm::VmmError;

// ── Memory addresses (must match vmm.rs layout) ────────────────────

/// Boot parameters address (RSI for kernel entry).
const BOOT_PARAMS_ADDR: u64 = 0x7000;
/// GDT placed here in guest memory.
const GDT_ADDR: u64 = 0x500;
/// PML4 (Level 4 page table) address.
const PML4_ADDR: u64 = 0x9000;
/// PDPT (Level 3) address.
const PDPT_ADDR: u64 = 0xA000;
/// PD (Level 2, 2 MB pages) address.
const PD_ADDR: u64 = 0xB000;
/// Initial kernel stack pointer (below page tables at 0x9000, above boot_params).
const BOOT_STACK_ADDR: u64 = 0x8FF0;

// ── Serial console constants ────────────────────────────────────────

/// COM1 data register.
const COM1_DATA: u16 = 0x3F8;
/// COM1 line status register.
const COM1_LSR: u16 = 0x3FD;
/// LSR: transmitter holding register empty + transmitter empty.
const LSR_THR_EMPTY: u8 = 0x60;

// ── GDT helpers ─────────────────────────────────────────────────────

/// Encode a GDT entry from (flags, base, limit).
///
/// `flags` high nibble = G,D/B,L,AVL; low byte = access byte.
const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let b = base as u64;
    let l = limit as u64;
    let f = flags as u64;

    ((b & 0xFF00_0000) << (56 - 24))
        | ((f & 0x0000_F0FF) << 40)
        | ((l & 0x000F_0000) << (48 - 16))
        | ((b & 0x00FF_FFFF) << 16)
        | (l & 0x0000_FFFF)
}

/// Null GDT entry.
const GDT_NULL: u64 = 0;
/// 64-bit code segment (DPL 0, execute/read, long mode).
const GDT_CODE: u64 = gdt_entry(0xA09B, 0, 0xFFFFF);
/// 32-bit data segment (DPL 0, read/write).
const GDT_DATA: u64 = gdt_entry(0xC093, 0, 0xFFFFF);

// ── Public setup functions ──────────────────────────────────────────

/// Passes the host's supported CPUID to the vCPU.
pub fn setup_cpuid(kvm: &Kvm, vcpu: &VcpuFd) -> Result<(), VmmError> {
    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    vcpu.set_cpuid2(&cpuid)?;
    Ok(())
}

/// Writes identity-mapped page tables into guest memory.
///
/// Maps the first 1 GiB with 2 MiB huge pages (512 PD entries).
pub fn setup_page_tables(mem: &GuestMemoryMmap) -> Result<(), VmmError> {
    // PML4[0] → PDPT
    mem.write_obj(PDPT_ADDR | 0x03u64, GuestAddress(PML4_ADDR))
        .map_err(|e| VmmError::Memory(format!("PML4: {e}")))?;
    // PDPT[0] → PD
    mem.write_obj(PD_ADDR | 0x03u64, GuestAddress(PDPT_ADDR))
        .map_err(|e| VmmError::Memory(format!("PDPT: {e}")))?;
    // PD: 512 × 2 MiB identity pages
    for i in 0u64..512 {
        let entry = (i << 21) | 0x83; // Present | RW | PS (2 MiB)
        mem.write_obj(entry, GuestAddress(PD_ADDR + i * 8))
            .map_err(|e| VmmError::Memory(format!("PD[{i}]: {e}")))?;
    }
    Ok(())
}

/// Writes the GDT (null + code + data) into guest memory.
pub fn setup_gdt(mem: &GuestMemoryMmap) -> Result<(), VmmError> {
    mem.write_obj(GDT_NULL, GuestAddress(GDT_ADDR))
        .map_err(|e| VmmError::Memory(format!("GDT null: {e}")))?;
    mem.write_obj(GDT_CODE, GuestAddress(GDT_ADDR + 8))
        .map_err(|e| VmmError::Memory(format!("GDT code: {e}")))?;
    mem.write_obj(GDT_DATA, GuestAddress(GDT_ADDR + 16))
        .map_err(|e| VmmError::Memory(format!("GDT data: {e}")))?;
    Ok(())
}

/// Configures special registers for 64-bit long-mode boot.
pub fn setup_sregs(vcpu: &VcpuFd) -> Result<(), VmmError> {
    let mut sregs = vcpu.get_sregs()?;

    // CR0: Protected mode + paging
    sregs.cr0 = 0x8000_0011; // PG | ET | PE
    // CR3: PML4 base
    sregs.cr3 = PML4_ADDR;
    // CR4: PAE
    sregs.cr4 = 0x20;
    // EFER: Long Mode Enable + Long Mode Active
    sregs.efer = 0x500; // LME (bit 8) | LMA (bit 10)

    // 64-bit code segment (selector 0x08)
    sregs.cs = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x08,
        type_: 11, // Execute/Read, Accessed
        present: 1,
        dpl: 0,
        db: 0, // must be 0 in 64-bit mode
        s: 1,
        l: 1, // 64-bit
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    };

    // Data segment (selector 0x10)
    let data_seg = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x10,
        type_: 3, // Read/Write, Accessed
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    };
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.ss = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;

    // GDT register
    sregs.gdt = kvm_dtable {
        base: GDT_ADDR,
        limit: 23, // 3 entries × 8 bytes − 1
        padding: [0; 3],
    };

    // IDT (empty for now — kernel will set it up)
    sregs.idt = kvm_dtable {
        base: 0,
        limit: 0,
        padding: [0; 3],
    };

    vcpu.set_sregs(&sregs)?;
    Ok(())
}

/// Sets general-purpose registers: RIP, RSP, RSI (boot params), RFLAGS.
pub fn setup_regs(vcpu: &VcpuFd, entry_addr: u64) -> Result<(), VmmError> {
    let regs = kvm_regs {
        rip: entry_addr,
        rsp: BOOT_STACK_ADDR,
        rsi: BOOT_PARAMS_ADDR, // Linux boot protocol: RSI → boot_params
        rflags: 0x02,          // Reserved bit must be set
        ..Default::default()
    };
    vcpu.set_regs(&regs)?;
    Ok(())
}

/// Initialises the FPU to a sane state.
pub fn setup_fpu(vcpu: &VcpuFd) -> Result<(), VmmError> {
    let fpu = kvm_bindings::kvm_fpu {
        fcw: 0x37F,    // x87 control word: all exceptions masked
        mxcsr: 0x1F80, // SSE control: all exceptions masked
        ..Default::default()
    };
    vcpu.set_fpu(&fpu)?;
    Ok(())
}

// ── vCPU run loop ───────────────────────────────────────────────────

/// Returns current wall-clock time as nanoseconds since epoch.
fn timestamp_ns() -> u64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_secs() * 1_000_000_000 + u64::from(dur.subsec_nanos())
}

/// Sets up a periodic SIGALRM to interrupt KVM_RUN (so we can detect
/// when the guest halts with interrupts disabled).
fn setup_vcpu_timer() {
    // SAFETY: setting a simple signal handler + timer.
    unsafe {
        // Install a no-op SIGALRM handler (just needs to interrupt KVM_RUN)
        libc::signal(
            libc::SIGALRM,
            noop_handler as *const () as libc::sighandler_t,
        );

        // Fire SIGALRM every 1 second
        let timer = libc::itimerval {
            it_interval: libc::timeval {
                tv_sec: 1,
                tv_usec: 0,
            },
            it_value: libc::timeval {
                tv_sec: 1,
                tv_usec: 0,
            },
        };
        libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut());
    }
}

extern "C" fn noop_handler(_sig: libc::c_int) {
    // Intentionally empty — just needs to interrupt KVM_RUN.
}

/// Checks if the guest vCPU is halted with interrupts disabled (IF=0).
fn is_guest_halted(vcpu: &VcpuFd) -> bool {
    if let Ok(regs) = vcpu.get_regs() {
        regs.rflags & 0x200 == 0
    } else {
        false
    }
}

/// Number of consecutive halt detections required before declaring the guest
/// truly halted. A single IF=0 sample can be a transient state (kernel in a
/// critical section with interrupts disabled). Requiring multiple consecutive
/// checks avoids false positives during normal boot.
const HALT_CONSECUTIVE_THRESHOLD: u32 = 3;

/// Runs the KVM_RUN loop, handling vCPU exits and recording metrics.
///
/// Blocks until the guest shuts down or an unrecoverable error occurs.
/// When a `block_device` is provided, MMIO accesses to the virtio-mmio
/// region are dispatched to it, and IRQs are injected via `vm_fd`.
pub fn run_vcpu_loop(
    vcpu: &mut VcpuFd,
    vm_fd: &VmFd,
    guest_memory: &GuestMemoryMmap,
    metrics: Arc<Mutex<VmMetrics>>,
    mut block_device: Option<&mut VirtioBlock>,
) -> Result<(), VmmError> {
    let mut exit_count: u64 = 0;
    let boot_start = Instant::now();
    let mut consecutive_halt_checks: u32 = 0;

    // Set up periodic SIGALRM to interrupt KVM_RUN when guest halts
    setup_vcpu_timer();

    loop {
        let run_start = Instant::now();

        match vcpu.run() {
            Ok(exit) => {
                let exit_start = Instant::now();
                let run_ns = run_start.elapsed().as_secs_f64() * 1e9;
                exit_count += 1;

                // Any successful KVM_RUN means the guest is still active.
                consecutive_halt_checks = 0;

                // Periodic debug: log exit stats every 1M exits
                if exit_count.is_multiple_of(1_000_000) {
                    let elapsed = boot_start.elapsed().as_secs_f64();
                    #[allow(clippy::cast_precision_loss)]
                    let rate = exit_count as f64 / elapsed;
                    tracing::debug!("exit #{exit_count} at {elapsed:.1}s ({rate:.0} exits/s)",);
                }

                let reason = match exit {
                    VcpuExit::IoOut(port, data) => {
                        handle_io_out(port, data);
                        VcpuExitReason::Io
                    }
                    VcpuExit::IoIn(port, data) => {
                        handle_io_in(port, data);
                        VcpuExitReason::Io
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        if let Some(ref blk) = block_device {
                            if addr >= block::MMIO_BASE
                                && addr < block::MMIO_BASE + block::MMIO_SIZE
                            {
                                blk.mmio_read(addr - block::MMIO_BASE, data);
                            } else {
                                for b in data.iter_mut() {
                                    *b = 0;
                                }
                            }
                        } else {
                            for b in data.iter_mut() {
                                *b = 0;
                            }
                        }
                        VcpuExitReason::Mmio
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        if let Some(ref mut blk) = block_device {
                            if addr >= block::MMIO_BASE
                                && addr < block::MMIO_BASE + block::MMIO_SIZE
                            {
                                let write_result =
                                    blk.mmio_write(addr - block::MMIO_BASE, data, guest_memory);
                                if write_result.needs_interrupt {
                                    // Edge-triggered: assert then deassert. The in-kernel
                                    // PIC latches the IRQ on assertion.
                                    let _ = vm_fd.set_irq_line(block::IRQ, true);
                                    let _ = vm_fd.set_irq_line(block::IRQ, false);
                                }
                                // Record block I/O metrics.
                                for io in &write_result.completed {
                                    record_blk_io(&metrics, io);
                                }
                            }
                        }
                        VcpuExitReason::Mmio
                    }
                    VcpuExit::Hlt => {
                        // Normal idle HLT — KVM resumes on next interrupt.
                        VcpuExitReason::Hlt
                    }
                    VcpuExit::Shutdown => {
                        tracing::info!("guest shutdown");
                        record_exit(
                            &metrics,
                            VcpuExitReason::Shutdown,
                            exit_start.elapsed().as_secs_f64() * 1e9,
                            run_ns,
                        );
                        return Ok(());
                    }
                    _ => VcpuExitReason::Other,
                };

                let exit_ns = exit_start.elapsed().as_secs_f64() * 1e9;
                record_exit(&metrics, reason, exit_ns, run_ns);
            }
            Err(e) => {
                // EINTR (errno 4): a signal interrupted KVM_RUN.
                // Check if guest is halted with interrupts disabled.
                if e.errno() == 4 {
                    if is_guest_halted(vcpu) {
                        consecutive_halt_checks += 1;
                        if consecutive_halt_checks >= HALT_CONSECUTIVE_THRESHOLD {
                            tracing::info!(
                                "guest halted (IF=0 detected {consecutive_halt_checks} \
                                 consecutive times)"
                            );
                            return Ok(());
                        }
                        tracing::debug!(
                            "halt check {consecutive_halt_checks}/{HALT_CONSECUTIVE_THRESHOLD}"
                        );
                    } else {
                        consecutive_halt_checks = 0;
                    }
                    continue;
                }
                return Err(VmmError::Kvm(e));
            }
        }
    }
}

/// Records a single vCPU exit metric (best-effort, never panics).
fn record_exit(metrics: &Arc<Mutex<VmMetrics>>, reason: VcpuExitReason, exit_ns: f64, run_ns: f64) {
    if let Ok(mut m) = metrics.lock() {
        let ts = timestamp_ns();
        let _ = m.record_vcpu_exit(reason, exit_ns, run_ns, ts);
    }
}

/// Records a completed block I/O operation as metrics (best-effort, never panics).
fn record_blk_io(metrics: &Arc<Mutex<VmMetrics>>, io: &block::CompletedIo) {
    if let Ok(mut m) = metrics.lock() {
        let ts = timestamp_ns();
        let op = match io.op {
            block::IoOp::Read => BlkOp::Read,
            block::IoOp::Write => BlkOp::Write,
            block::IoOp::Flush => BlkOp::Flush,
        };
        #[allow(clippy::cast_precision_loss)]
        let _ = m.record_blk_request(op, io.duration_ns as f64, io.bytes as f64, ts);
    }
}

/// Handles an IO-port write from the guest (serial console output).
fn handle_io_out(port: u16, data: &[u8]) {
    if port == COM1_DATA {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(data);
        let _ = lock.flush();
    }
    // All other port writes are silently ignored.
}

/// Port 0x61: system control port B (speaker / PIT channel 2 gate).
const SYSTEM_CTRL_PORT_B: u16 = 0x61;
/// PCI configuration data port (0xCFC–0xCFF).
const PCI_CONFIG_DATA: u16 = 0xCFC;

/// Handles an IO-port read from the guest (serial status, PIT gate, PCI).
fn handle_io_in(port: u16, data: &mut [u8]) {
    if port == COM1_LSR && !data.is_empty() {
        // Transmitter idle and ready
        data[0] = LSR_THR_EMPTY;
    } else if port == SYSTEM_CTRL_PORT_B && !data.is_empty() {
        // Toggle PIT channel 2 output (bit 5) on each read.
        // The kernel reads this in a busy loop during timer calibration,
        // waiting for the output bit to change.
        static TOGGLE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        data[0] = TOGGLE.fetch_xor(0x20, std::sync::atomic::Ordering::Relaxed);
    } else if port == PCI_CONFIG_DATA && !data.is_empty() {
        // No PCI devices — return 0xFF (no device present)
        for b in data.iter_mut() {
            *b = 0xFF;
        }
    } else {
        // Default: return zeros
        for b in data.iter_mut() {
            *b = 0;
        }
    }
}

// ── Export loop (remote-write) ──────────────────────────────────────

/// Interval between remote-write pushes.
const EXPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Runs a periodic export loop: drain tier 0 → push to Prometheus remote-write.
///
/// This runs in its own thread and never returns (daemon thread).
pub fn export_loop(
    metrics: Arc<Mutex<VmMetrics>>,
    endpoint: &str,
    cursor_path: &std::path::Path,
    external_labels: &[(String, String)],
) {
    use rondo::export::ExportCursor;
    use rondo::remote_write::{RemoteWriteConfig, push};

    let config = RemoteWriteConfig::new(endpoint);

    let mut cursor = match ExportCursor::load_or_new(cursor_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to load export cursor: {e}");
            return;
        }
    };

    tracing::info!("remote-write export loop started (interval: {EXPORT_INTERVAL:?})");

    loop {
        std::thread::sleep(EXPORT_INTERVAL);

        // Drain and push under the metrics lock
        let result = if let Ok(m) = metrics.lock() {
            let store = m.store();
            match store.drain(0, &mut cursor) {
                Ok(exports) if exports.is_empty() => {
                    tracing::debug!("remote-write: no new data to export");
                    continue;
                }
                Ok(exports) => {
                    let count = exports.len();
                    match push(&config, &exports, store, external_labels) {
                        Ok(n) => {
                            tracing::info!("remote-write: pushed {n} series ({count} with data)");
                            Ok(())
                        }
                        Err(e) => Err(format!("push failed: {e}")),
                    }
                }
                Err(e) => Err(format!("drain failed: {e}")),
            }
        } else {
            Err("failed to acquire metrics lock".to_string())
        };

        match result {
            Ok(()) => {
                if let Err(e) = cursor.save() {
                    tracing::warn!("remote-write: failed to save cursor: {e}");
                }
            }
            Err(msg) => {
                tracing::warn!("remote-write: {msg}");
            }
        }
    }
}

// ── Maintenance loop ────────────────────────────────────────────────

/// Runs a 1-second maintenance tick: consolidation + process metrics.
///
/// This runs in its own thread and never returns (daemon thread).
pub fn maintenance_loop(metrics: Arc<Mutex<VmMetrics>>) {
    let start = Instant::now();

    loop {
        std::thread::sleep(Duration::from_secs(1));

        let ts = timestamp_ns();
        let uptime = start.elapsed().as_secs_f64();

        if let Ok(mut m) = metrics.lock() {
            // Process metrics
            let rss = read_rss_bytes().unwrap_or(0.0);
            let fds = read_open_fds().unwrap_or(0.0);
            let _ = m.record_process_stats(rss, fds, uptime, ts);

            // Consolidation tick
            match m.consolidate() {
                Ok(n) if n > 0 => {
                    tracing::debug!("consolidated {n} slot(s)");
                }
                Err(e) => {
                    tracing::warn!("consolidation error: {e}");
                }
                _ => {}
            }
        }
    }
}

/// Reads RSS from `/proc/self/status` (Linux only).
fn read_rss_bytes() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
            return Some(kb * 1024.0);
        }
    }
    None
}

/// Counts open file descriptors via `/proc/self/fd` (Linux only).
#[allow(clippy::cast_precision_loss)]
fn read_open_fds() -> Option<f64> {
    let count = std::fs::read_dir("/proc/self/fd").ok()?.count();
    Some(count as f64)
}
