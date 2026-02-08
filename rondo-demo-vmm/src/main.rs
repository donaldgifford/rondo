//! Minimal demo VMM with embedded rondo time-series metrics.
//!
//! This binary boots a Linux guest via KVM and records VM-level metrics
//! (vCPU exits, virtio-blk I/O, process stats) into an embedded rondo store.
//!
//! **Requires Linux with KVM support.** On other platforms, build succeeds
//! but the VMM cannot be started.

#[allow(dead_code)]
mod metrics;

#[cfg(target_os = "linux")]
mod vmm;
#[cfg(target_os = "linux")]
mod vcpu;
#[cfg(target_os = "linux")]
mod devices;
#[cfg(target_os = "linux")]
mod api;

use std::path::PathBuf;

use clap::Parser;

/// rondo-demo-vmm — Minimal VMM with embedded metrics.
#[derive(Parser)]
#[command(name = "rondo-demo-vmm", version, about)]
struct Cli {
    /// Path to the kernel bzImage.
    #[arg(long)]
    kernel: PathBuf,

    /// Path to the initramfs.
    #[arg(long)]
    initramfs: Option<PathBuf>,

    /// Path to the rondo metrics store directory.
    #[arg(long, default_value = "./vmm_metrics")]
    metrics_store: PathBuf,

    /// Kernel command line arguments.
    #[arg(long, default_value = "console=ttyS0 reboot=k panic=1")]
    cmdline: String,

    /// Guest memory size in MiB.
    #[arg(long, default_value = "128")]
    memory_mib: u32,

    /// Port for the HTTP metrics API.
    #[arg(long, default_value = "9100")]
    api_port: u16,
}

fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    #[cfg(target_os = "linux")]
    {
        if let Err(e) = run_vmm(cli) {
            tracing::error!("VMM failed: {e}");
            std::process::exit(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cli;
        eprintln!("rondo-demo-vmm requires Linux with KVM support.");
        eprintln!("This binary was built on a non-Linux platform and cannot start a VM.");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn run_vmm(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = vmm::VmmConfig {
        kernel_path: cli.kernel,
        initramfs_path: cli.initramfs,
        cmdline: cli.cmdline,
        memory_mib: cli.memory_mib,
        metrics_store_path: cli.metrics_store,
        api_port: cli.api_port,
    };

    let mut vmm = vmm::Vmm::new(config)?;
    vmm.run()?;

    tracing::info!("VMM exited cleanly");
    Ok(())
}
