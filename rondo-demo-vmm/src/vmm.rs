//! Minimal VMM boot implementation using rust-vmm crates.
//!
//! Creates a KVM VM, configures memory regions, loads a bzImage kernel,
//! and boots the guest to a serial console with embedded rondo metrics.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VmFd};
use linux_loader::loader::KernelLoader;
use linux_loader::loader::bzimage::BzImage;
use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use crate::devices::block::VirtioBlock;
use crate::metrics::VmMetrics;
use crate::vcpu;

// ── Memory Layout ───────────────────────────────────────────────────

/// Boot parameters (zero page) address.
const BOOT_PARAMS_ADDR: u64 = 0x7000;
/// Kernel command line address.
const CMDLINE_ADDR: u64 = 0x20000;
/// High memory start / default kernel load address.
const HIMEM_START: u64 = 0x100000;

/// VMM configuration parsed from CLI arguments.
pub struct VmmConfig {
    /// Path to the kernel bzImage.
    pub kernel_path: PathBuf,
    /// Path to the initramfs (optional).
    pub initramfs_path: Option<PathBuf>,
    /// Kernel command line.
    pub cmdline: String,
    /// Guest memory size in MiB.
    pub memory_mib: u32,
    /// Path to the rondo metrics store directory.
    pub metrics_store_path: PathBuf,
    /// HTTP API listen port.
    pub api_port: u16,
    /// Prometheus remote-write endpoint URL (optional).
    pub remote_write_endpoint: Option<String>,
    /// Extra labels added to every remote-write time series.
    pub external_labels: Vec<(String, String)>,
    /// Path to the virtio-blk backing file (optional).
    pub disk_path: Option<PathBuf>,
}

/// VMM error type.
#[derive(Debug)]
pub enum VmmError {
    /// KVM ioctl error.
    Kvm(kvm_ioctls::Error),
    /// Guest memory error.
    Memory(String),
    /// Kernel loading error.
    KernelLoad(String),
    /// I/O error.
    Io(std::io::Error),
    /// Metrics store error.
    Metrics(rondo::RondoError),
}

impl std::fmt::Display for VmmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kvm(e) => write!(f, "KVM: {e}"),
            Self::Memory(e) => write!(f, "memory: {e}"),
            Self::KernelLoad(e) => write!(f, "kernel load: {e}"),
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Metrics(e) => write!(f, "metrics: {e}"),
        }
    }
}

impl std::error::Error for VmmError {}

impl From<kvm_ioctls::Error> for VmmError {
    fn from(e: kvm_ioctls::Error) -> Self {
        Self::Kvm(e)
    }
}

impl From<std::io::Error> for VmmError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rondo::RondoError> for VmmError {
    fn from(e: rondo::RondoError) -> Self {
        Self::Metrics(e)
    }
}

/// The virtual machine monitor.
///
/// Owns the KVM VM, guest memory, vCPU, and metrics store.
/// Call [`Vmm::run`] to start the vCPU loop and supporting threads.
pub struct Vmm {
    vm_fd: VmFd,
    vcpu_fd: kvm_ioctls::VcpuFd,
    guest_memory: GuestMemoryMmap,
    metrics: Arc<Mutex<VmMetrics>>,
    api_port: u16,
    metrics_store_path: PathBuf,
    remote_write_endpoint: Option<String>,
    external_labels: Vec<(String, String)>,
    block_device: Option<VirtioBlock>,
}

impl Vmm {
    /// Creates a new VMM: opens KVM, configures memory, loads the kernel,
    /// sets up the vCPU for 64-bit boot, and initializes the metrics store.
    pub fn new(mut config: VmmConfig) -> Result<Self, VmmError> {
        // Append virtio-mmio device announcement to cmdline if a disk is configured.
        if config.disk_path.is_some() {
            config.cmdline.push(' ');
            config
                .cmdline
                .push_str(crate::devices::block::CMDLINE_PARAM);
        }

        // 1. Open KVM
        let kvm = Kvm::new()?;
        tracing::info!("KVM API version: {}", kvm.get_api_version());

        // 2. Create VM
        let vm_fd = kvm.create_vm()?;

        // 3. Create guest memory
        let mem_size = (config.memory_mib as usize) << 20;
        let guest_memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)])
            .map_err(|e| VmmError::Memory(e.to_string()))?;

        // 4. Register memory regions with KVM
        for (slot, region) in guest_memory.iter().enumerate() {
            let host_addr = region
                .get_host_address(vm_memory::MemoryRegionAddress(0))
                .map_err(|e| VmmError::Memory(e.to_string()))?;

            #[allow(clippy::cast_possible_truncation)]
            let kvm_region = kvm_userspace_memory_region {
                slot: slot as u32,
                guest_phys_addr: region.start_addr().0,
                memory_size: region.len(),
                userspace_addr: host_addr as u64,
                flags: 0,
            };
            // SAFETY: host_addr points to the mmap'd region that outlives vm_fd.
            unsafe {
                vm_fd.set_user_memory_region(kvm_region)?;
            }
        }

        // 5. Create in-kernel IRQ chip and PIT (required for interrupts / timer)
        vm_fd.create_irq_chip()?;
        vm_fd.create_pit2(kvm_bindings::kvm_pit_config::default())?;

        // 6. Create vCPU and set supported CPUID
        let vcpu_fd = vm_fd.create_vcpu(0)?;
        vcpu::setup_cpuid(&kvm, &vcpu_fd)?;

        // 7. Load kernel
        let mut kernel_file = std::fs::File::open(&config.kernel_path)?;
        let kernel_result = BzImage::load(
            &guest_memory,
            Some(GuestAddress(HIMEM_START)),
            &mut kernel_file,
            Some(GuestAddress(HIMEM_START)),
        )
        .map_err(|e| VmmError::KernelLoad(format!("{e}")))?;

        let kernel_load = kernel_result.kernel_load.0;
        let kernel_end = kernel_result.kernel_end;
        // 64-bit entry point (startup_64) is at offset 0x200 from load address
        let kernel_entry = kernel_load + 0x200;
        tracing::info!(
            "kernel loaded at {:#x}, entry at {:#x}, ends at {:#x}",
            kernel_load,
            kernel_entry,
            kernel_end
        );

        // 8. Load initramfs (if provided)
        let initramfs_info = match config.initramfs_path {
            Some(ref path) => Some(Self::load_initramfs(&guest_memory, path, kernel_end)?),
            None => None,
        };

        // 9. Write boot parameters (zero page)
        Self::setup_boot_params(
            &guest_memory,
            &config.cmdline,
            kernel_result.setup_header,
            initramfs_info,
            mem_size as u64,
        )?;

        // 10. Write page tables and GDT into guest memory
        vcpu::setup_page_tables(&guest_memory)?;
        vcpu::setup_gdt(&guest_memory)?;

        // 11. Configure vCPU registers for 64-bit Linux boot
        vcpu::setup_sregs(&vcpu_fd)?;
        vcpu::setup_regs(&vcpu_fd, kernel_entry)?;
        vcpu::setup_fpu(&vcpu_fd)?;

        // 12. Initialize metrics store
        let metrics = VmMetrics::open(&config.metrics_store_path)?;
        tracing::info!("metrics store opened at {:?}", config.metrics_store_path);

        // 13. Create virtio-blk device (if disk path is configured)
        let block_device = match config.disk_path {
            Some(ref path) => Some(VirtioBlock::new(path)?),
            None => None,
        };

        Ok(Self {
            vm_fd,
            vcpu_fd,
            guest_memory,
            metrics: Arc::new(Mutex::new(metrics)),
            api_port: config.api_port,
            metrics_store_path: config.metrics_store_path,
            remote_write_endpoint: config.remote_write_endpoint,
            external_labels: config.external_labels,
            block_device,
        })
    }

    /// Starts the VMM: spawns the API server and maintenance threads,
    /// then runs the vCPU loop in the calling thread (blocks until guest
    /// shuts down).
    pub fn run(&mut self) -> Result<(), VmmError> {
        // Spawn HTTP API server
        let api_metrics = self.metrics.clone();
        let api_port = self.api_port;
        std::thread::Builder::new()
            .name("api-server".into())
            .spawn(move || {
                crate::api::run_api_server(api_metrics, api_port);
            })
            .map_err(VmmError::Io)?;
        tracing::info!("API server listening on port {}", self.api_port);

        // Spawn maintenance tick (consolidation + process metrics)
        let maint_metrics = self.metrics.clone();
        std::thread::Builder::new()
            .name("maintenance".into())
            .spawn(move || {
                vcpu::maintenance_loop(maint_metrics);
            })
            .map_err(VmmError::Io)?;

        // Spawn remote-write export thread (if configured)
        if let Some(ref endpoint) = self.remote_write_endpoint {
            let export_metrics = self.metrics.clone();
            let cursor_path = self.metrics_store_path.join("cursor_prometheus.json");
            let endpoint = endpoint.clone();
            let external_labels = self.external_labels.clone();
            std::thread::Builder::new()
                .name("remote-write".into())
                .spawn(move || {
                    vcpu::export_loop(export_metrics, &endpoint, &cursor_path, &external_labels);
                })
                .map_err(VmmError::Io)?;
            tracing::info!(
                "remote-write export thread started → {}",
                self.remote_write_endpoint.as_ref().unwrap()
            );
        }

        // Run vCPU loop in this thread (blocks)
        tracing::info!("starting vCPU");
        vcpu::run_vcpu_loop(
            &mut self.vcpu_fd,
            &self.vm_fd,
            &self.guest_memory,
            self.metrics.clone(),
            self.block_device.as_mut(),
        )
    }

    /// Loads an initramfs file into guest memory above the kernel.
    fn load_initramfs(
        mem: &GuestMemoryMmap,
        path: &PathBuf,
        kernel_end: u64,
    ) -> Result<(u32, u32), VmmError> {
        let data = std::fs::read(path)?;
        // Page-align above kernel end
        let addr = (kernel_end + 0xFFF) & !0xFFF;
        mem.write(&data, GuestAddress(addr))
            .map_err(|e| VmmError::Memory(format!("initramfs write: {e}")))?;

        tracing::info!("initramfs at {:#x}, {} bytes", addr, data.len());

        #[allow(clippy::cast_possible_truncation)]
        Ok((addr as u32, data.len() as u32))
    }

    /// Sets up the boot parameters (zero page) at [`BOOT_PARAMS_ADDR`].
    fn setup_boot_params(
        mem: &GuestMemoryMmap,
        cmdline: &str,
        setup_header: Option<linux_loader::bootparam::setup_header>,
        initramfs: Option<(u32, u32)>,
        mem_size: u64,
    ) -> Result<(), VmmError> {
        use linux_loader::bootparam::boot_params;

        // Write kernel command line
        let cmdline_bytes = cmdline.as_bytes();
        mem.write(cmdline_bytes, GuestAddress(CMDLINE_ADDR))
            .map_err(|e| VmmError::Memory(format!("cmdline: {e}")))?;
        mem.write(
            &[0u8],
            GuestAddress(CMDLINE_ADDR + cmdline_bytes.len() as u64),
        )
        .map_err(|e| VmmError::Memory(format!("cmdline null: {e}")))?;

        // SAFETY: boot_params is a repr(C) struct; zero-init is valid.
        let mut params: boot_params = unsafe { std::mem::zeroed() };

        // Carry forward the kernel's setup header if available
        if let Some(hdr) = setup_header {
            params.hdr = hdr;
        }

        // Boot loader identification
        params.hdr.type_of_loader = 0xFF;
        // Kernel loaded at high address
        params.hdr.loadflags |= 0x01; // LOADED_HIGH

        // Command line
        #[allow(clippy::cast_possible_truncation)]
        {
            params.hdr.cmd_line_ptr = CMDLINE_ADDR as u32;
            params.hdr.cmdline_size = cmdline_bytes.len() as u32;
        }

        // Initramfs
        if let Some((addr, size)) = initramfs {
            params.hdr.ramdisk_image = addr;
            params.hdr.ramdisk_size = size;
        }

        // E820 memory map
        // 0 .. 640K: usable RAM
        params.e820_table[0].addr = 0;
        params.e820_table[0].size = 0x9_FC00;
        params.e820_table[0].type_ = 1; // E820_RAM

        // 640K .. 1MB: reserved (BIOS, video, etc.)
        params.e820_table[1].addr = 0x9_FC00;
        params.e820_table[1].size = 0x10_0000 - 0x9_FC00;
        params.e820_table[1].type_ = 2; // E820_RESERVED

        // 1MB .. end: usable RAM
        params.e820_table[2].addr = 0x10_0000;
        params.e820_table[2].size = mem_size.saturating_sub(0x10_0000);
        params.e820_table[2].type_ = 1; // E820_RAM

        params.e820_entries = 3;

        // Write zero page to guest memory as raw bytes
        // SAFETY: boot_params is repr(C) and fully initialized via zeroed() + field writes.
        let params_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&params).cast::<u8>(),
                std::mem::size_of::<boot_params>(),
            )
        };
        mem.write(params_bytes, GuestAddress(BOOT_PARAMS_ADDR))
            .map_err(|e| VmmError::Memory(format!("boot params: {e}")))?;

        Ok(())
    }
}
