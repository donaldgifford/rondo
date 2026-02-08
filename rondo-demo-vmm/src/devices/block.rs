//! Virtio-blk device over virtio-mmio transport.
//!
//! Provides a minimal virtio block device backed by a host file. The device
//! uses the virtio-mmio transport at a fixed MMIO base address. The guest
//! kernel discovers it via the `virtio_mmio.device=` command-line parameter.
//!
//! Supports read, write, and flush operations. Each completed I/O is returned
//! as a [`CompletedIo`] for metrics instrumentation by the caller.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek, SeekFrom, Write as _};
use std::path::Path;
use std::time::Instant;

use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

use crate::vmm::VmmError;

// ── MMIO region layout ──────────────────────────────────────────────

/// Base guest physical address for the virtio-mmio region.
pub const MMIO_BASE: u64 = 0xd000_0000;

/// Size of the virtio-mmio register region (bytes).
pub const MMIO_SIZE: u64 = 0x200;

/// IRQ line for the virtio-blk device (legacy PIC).
pub const IRQ: u32 = 5;

/// Kernel command-line parameter announcing the device to the guest.
pub const CMDLINE_PARAM: &str = "virtio_mmio.device=512@0xd0000000:5";

// ── Virtio constants ────────────────────────────────────────────────

/// Virtio MMIO magic value ("virt").
const MAGIC: u32 = 0x7472_6976;
/// Virtio MMIO version 2 (modern / non-legacy).
const MMIO_VERSION: u32 = 2;
/// Virtio device ID for block device.
const DEVICE_ID_BLK: u32 = 2;
/// Vendor ID (QEMU convention, widely expected by guests).
const VIRTIO_VENDOR_ID: u32 = 0x554D_4551;

// Feature bits
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Device features advertised to the guest.
const DEVICE_FEATURES: u64 = VIRTIO_BLK_F_FLUSH | VIRTIO_F_VERSION_1;

// Block request types (from virtio spec)
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

// Block request status bytes
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// Virtqueue descriptor flags
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Maximum virtqueue size (number of descriptors).
const QUEUE_MAX_SIZE: u16 = 128;

/// Sector size in bytes.
const SECTOR_SIZE: u64 = 512;
/// Default backing file size when creating a new disk.
const DEFAULT_DISK_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

// ── MMIO register offsets (virtio-mmio v2 spec) ─────────────────────

const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00C;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090;
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0A0;
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0A4;
const REG_CONFIG_GEN: u64 = 0x0FC;
const REG_CONFIG_START: u64 = 0x100;

// ── Public types ────────────────────────────────────────────────────

/// A completed block I/O operation, returned for metrics recording.
#[derive(Debug)]
pub struct CompletedIo {
    /// Operation type.
    pub op: IoOp,
    /// Bytes transferred (0 for flush).
    pub bytes: u64,
    /// Processing duration in nanoseconds.
    pub duration_ns: u64,
}

/// Block I/O operation type.
#[derive(Debug, Clone, Copy)]
pub enum IoOp {
    /// Guest read data from disk.
    Read,
    /// Guest wrote data to disk.
    Write,
    /// Guest flushed the disk.
    Flush,
}

/// Result of handling an MMIO write to the virtio-blk device.
pub struct WriteResult {
    /// Whether the device needs to inject an IRQ to the guest.
    pub needs_interrupt: bool,
    /// Completed I/O operations since the last call.
    pub completed: Vec<CompletedIo>,
}

// ── Guest memory structures (repr(C) for direct read via ByteValued) ─

/// Virtqueue descriptor (16 bytes, virtio spec 2.7.5).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VirtqDesc {
    /// Guest physical address of the buffer.
    addr: u64,
    /// Buffer length in bytes.
    len: u32,
    /// Descriptor flags (NEXT, WRITE, INDIRECT).
    flags: u16,
    /// Index of the next descriptor in the chain.
    next: u16,
}

// SAFETY: VirtqDesc is repr(C) with only fixed-size integer fields.
// All bit patterns are valid for these types.
unsafe impl ByteValued for VirtqDesc {}

/// Virtio-blk request header (16 bytes, virtio spec 5.2.6).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VirtioBlkReqHdr {
    /// Request type (IN=0, OUT=1, FLUSH=4).
    type_: u32,
    /// Reserved (must be 0).
    _reserved: u32,
    /// Starting sector for the I/O (512-byte units).
    sector: u64,
}

// SAFETY: VirtioBlkReqHdr is repr(C) with only fixed-size integer fields.
unsafe impl ByteValued for VirtioBlkReqHdr {}

// ── Virtqueue state ─────────────────────────────────────────────────

/// State for a single virtqueue.
struct VirtQueue {
    /// Queue size (number of descriptors, must be power of 2).
    num: u16,
    /// Whether the driver has activated this queue.
    ready: bool,
    /// Guest physical address of the descriptor table.
    desc_table: u64,
    /// Guest physical address of the available ring.
    avail_ring: u64,
    /// Guest physical address of the used ring.
    used_ring: u64,
    /// Device-side index tracking which available entries have been consumed.
    last_avail_idx: u16,
}

impl VirtQueue {
    fn new() -> Self {
        Self {
            num: 0,
            ready: false,
            desc_table: 0,
            avail_ring: 0,
            used_ring: 0,
            last_avail_idx: 0,
        }
    }
}

// ── VirtioBlock device ──────────────────────────────────────────────

/// Virtio block device with virtio-mmio transport.
///
/// Created by [`VirtioBlock::new`] and wired into the vCPU MMIO exit
/// handler. Handles register reads/writes per the virtio-mmio spec and
/// processes block I/O requests against a backing file.
pub struct VirtioBlock {
    /// Host-side backing file for disk I/O.
    backing: File,
    /// Disk capacity in 512-byte sectors.
    capacity_sectors: u64,

    // ── Virtio MMIO transport state ──
    /// Device status register (written by driver during initialization).
    status: u32,
    /// Selector for which 32-bit page of device features to read.
    device_features_sel: u32,
    /// Features accepted by the driver.
    driver_features: u64,
    /// Selector for which 32-bit page of driver features to write.
    driver_features_sel: u32,
    /// Pending interrupt status (bit 0 = used buffer notification).
    interrupt_status: u32,
    /// Configuration space generation counter.
    config_generation: u32,

    /// The single request virtqueue.
    queue: VirtQueue,
}

impl VirtioBlock {
    /// Creates a new virtio-blk device backed by the file at `path`.
    ///
    /// If the file does not exist, it is created with a default size of 64 MiB.
    /// The capacity is rounded down to the nearest 512-byte sector boundary.
    ///
    /// # Errors
    ///
    /// Returns `VmmError::Io` if the file cannot be opened, created, or sized.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, VmmError> {
        let path = path.as_ref();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let metadata = file.metadata()?;
        let size = if metadata.len() == 0 {
            file.set_len(DEFAULT_DISK_BYTES)?;
            DEFAULT_DISK_BYTES
        } else {
            metadata.len()
        };

        let capacity_sectors = size / SECTOR_SIZE;
        tracing::info!(
            "virtio-blk: {} ({} sectors, {} MiB)",
            path.display(),
            capacity_sectors,
            size / (1024 * 1024),
        );

        Ok(Self {
            backing: file,
            capacity_sectors,
            status: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            interrupt_status: 0,
            config_generation: 0,
            queue: VirtQueue::new(),
        })
    }

    /// Handles an MMIO read from the guest.
    ///
    /// Fills `data` with the register value at `offset` (relative to
    /// [`MMIO_BASE`]).
    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        // Config space reads can be various sizes; register reads must be 4 bytes.
        if offset >= REG_CONFIG_START {
            self.read_config(offset - REG_CONFIG_START, data);
            return;
        }

        if data.len() != 4 {
            for b in data.iter_mut() {
                *b = 0;
            }
            return;
        }

        #[allow(clippy::cast_possible_truncation)]
        let val: u32 = match offset {
            REG_MAGIC => MAGIC,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => DEVICE_ID_BLK,
            REG_VENDOR_ID => VIRTIO_VENDOR_ID,
            REG_DEVICE_FEATURES => {
                if self.device_features_sel == 0 {
                    DEVICE_FEATURES as u32
                } else if self.device_features_sel == 1 {
                    (DEVICE_FEATURES >> 32) as u32
                } else {
                    0
                }
            }
            REG_QUEUE_NUM_MAX => u32::from(QUEUE_MAX_SIZE),
            REG_QUEUE_READY => u32::from(self.queue.ready),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => self.status,
            REG_CONFIG_GEN => self.config_generation,
            _ => 0,
        };

        data.copy_from_slice(&val.to_le_bytes());
    }

    /// Handles an MMIO write from the guest.
    ///
    /// Returns a [`WriteResult`] indicating whether an IRQ should be
    /// injected and any completed I/O operations for metrics recording.
    pub fn mmio_write(
        &mut self,
        offset: u64,
        data: &[u8],
        mem: &GuestMemoryMmap,
    ) -> WriteResult {
        let no_op = WriteResult {
            needs_interrupt: false,
            completed: Vec::new(),
        };

        if data.len() != 4 {
            return no_op;
        }

        let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = val,
            REG_DRIVER_FEATURES => {
                let mask = 0xFFFF_FFFF_0000_0000_u64;
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & mask) | u64::from(val);
                } else if self.driver_features_sel == 1 {
                    self.driver_features =
                        (self.driver_features & !mask) | (u64::from(val) << 32);
                }
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = val,
            REG_QUEUE_SEL => {
                if val != 0 {
                    tracing::debug!("virtio-blk: guest selected non-existent queue {val}");
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            REG_QUEUE_NUM => {
                let clamped = val.min(u32::from(QUEUE_MAX_SIZE));
                self.queue.num = clamped as u16;
            }
            REG_QUEUE_READY => {
                self.queue.ready = val != 0;
                if self.queue.ready {
                    tracing::info!(
                        "virtio-blk: queue ready (size={}, desc={:#x}, avail={:#x}, used={:#x})",
                        self.queue.num,
                        self.queue.desc_table,
                        self.queue.avail_ring,
                        self.queue.used_ring,
                    );
                }
            }
            REG_QUEUE_NOTIFY => {
                if val == 0 && self.queue.ready {
                    return self.process_queue(mem);
                }
            }
            REG_INTERRUPT_ACK => {
                self.interrupt_status &= !val;
            }
            REG_STATUS => {
                if val == 0 {
                    self.reset();
                } else {
                    self.status = val;
                }
            }
            REG_QUEUE_DESC_LOW => {
                self.queue.desc_table =
                    (self.queue.desc_table & 0xFFFF_FFFF_0000_0000) | u64::from(val);
            }
            REG_QUEUE_DESC_HIGH => {
                self.queue.desc_table =
                    (self.queue.desc_table & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
            }
            REG_QUEUE_DRIVER_LOW => {
                self.queue.avail_ring =
                    (self.queue.avail_ring & 0xFFFF_FFFF_0000_0000) | u64::from(val);
            }
            REG_QUEUE_DRIVER_HIGH => {
                self.queue.avail_ring =
                    (self.queue.avail_ring & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
            }
            REG_QUEUE_DEVICE_LOW => {
                self.queue.used_ring =
                    (self.queue.used_ring & 0xFFFF_FFFF_0000_0000) | u64::from(val);
            }
            REG_QUEUE_DEVICE_HIGH => {
                self.queue.used_ring =
                    (self.queue.used_ring & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
            }
            _ => {
                tracing::debug!("virtio-blk: unhandled write at offset {offset:#x}");
            }
        }

        no_op
    }

    // ── Config space ────────────────────────────────────────────────

    /// Reads from the device configuration space.
    ///
    /// The config space contains a single `u64` field: disk capacity in
    /// 512-byte sectors.
    fn read_config(&self, config_offset: u64, data: &mut [u8]) {
        let config_bytes = self.capacity_sectors.to_le_bytes();
        let start = config_offset as usize;
        if start < config_bytes.len() {
            let end = (start + data.len()).min(config_bytes.len());
            let len = end - start;
            data[..len].copy_from_slice(&config_bytes[start..end]);
            // Zero-fill any remaining bytes
            for b in data.iter_mut().skip(len) {
                *b = 0;
            }
        } else {
            for b in data.iter_mut() {
                *b = 0;
            }
        }
    }

    /// Resets the device to its initial state.
    fn reset(&mut self) {
        self.status = 0;
        self.driver_features = 0;
        self.driver_features_sel = 0;
        self.device_features_sel = 0;
        self.interrupt_status = 0;
        self.queue = VirtQueue::new();
        tracing::debug!("virtio-blk: device reset");
    }

    // ── Queue processing ────────────────────────────────────────────

    /// Processes all pending requests in the virtqueue.
    ///
    /// Reads new entries from the available ring, processes each descriptor
    /// chain, writes results to the used ring, and signals completion.
    fn process_queue(&mut self, mem: &GuestMemoryMmap) -> WriteResult {
        let mut result = WriteResult {
            needs_interrupt: false,
            completed: Vec::new(),
        };

        if !self.queue.ready || self.queue.num == 0 {
            return result;
        }

        // Read the current available ring index (avail_ring + 2 = idx field).
        let avail_idx: u16 = match mem.read_obj(GuestAddress(self.queue.avail_ring + 2)) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("virtio-blk: failed to read avail idx: {e}");
                return result;
            }
        };

        while self.queue.last_avail_idx != avail_idx {
            // Read the descriptor chain head from avail ring.
            // avail ring layout: flags(u16) | idx(u16) | ring[N](u16 each)
            let ring_entry_offset =
                4 + u64::from(self.queue.last_avail_idx % self.queue.num) * 2;
            let desc_head: u16 =
                match mem.read_obj(GuestAddress(self.queue.avail_ring + ring_entry_offset)) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("virtio-blk: failed to read avail ring entry: {e}");
                        break;
                    }
                };

            // Process the descriptor chain (returns bytes written to used ring len).
            let (used_len, completed) = self.process_request(desc_head, mem);
            if let Some(io) = completed {
                result.completed.push(io);
            }

            // Write to the used ring.
            // used ring layout: flags(u16) | idx(u16) | ring[N](UsedElem: id(u32) + len(u32))
            let used_idx: u16 = mem
                .read_obj(GuestAddress(self.queue.used_ring + 2))
                .unwrap_or(0);
            let used_entry_offset = 4 + u64::from(used_idx % self.queue.num) * 8;

            let _ = mem.write_obj(
                u32::from(desc_head),
                GuestAddress(self.queue.used_ring + used_entry_offset),
            );
            let _ = mem.write_obj(
                used_len,
                GuestAddress(self.queue.used_ring + used_entry_offset + 4),
            );
            // Advance the used ring index.
            let _ = mem.write_obj(
                used_idx.wrapping_add(1),
                GuestAddress(self.queue.used_ring + 2),
            );

            self.queue.last_avail_idx = self.queue.last_avail_idx.wrapping_add(1);
        }

        if !result.completed.is_empty() {
            self.interrupt_status |= 1; // Used buffer notification
            result.needs_interrupt = true;
        }

        result
    }

    /// Processes a single block I/O request from a descriptor chain.
    ///
    /// Returns `(used_len, Option<CompletedIo>)` where `used_len` is the
    /// total bytes written into device-writable descriptors (for the used
    /// ring entry).
    fn process_request(
        &mut self,
        head: u16,
        mem: &GuestMemoryMmap,
    ) -> (u32, Option<CompletedIo>) {
        let start = Instant::now();

        // 1. Read the header descriptor.
        let header_desc = match self.read_desc(head, mem) {
            Some(d) => d,
            None => return (0, None),
        };

        if header_desc.len < 16 {
            tracing::warn!(
                "virtio-blk: header too short ({} bytes, need 16)",
                header_desc.len,
            );
            return (0, None);
        }

        // 2. Read the virtio-blk request header from guest memory.
        let hdr: VirtioBlkReqHdr = match mem.read_obj(GuestAddress(header_desc.addr)) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("virtio-blk: failed to read request header: {e}");
                return (0, None);
            }
        };

        // 3. Walk the descriptor chain to collect data and status descriptors.
        let mut data_descs: Vec<VirtqDesc> = Vec::with_capacity(4);
        let mut status_desc: Option<VirtqDesc> = None;
        let mut current = header_desc;

        while current.flags & VIRTQ_DESC_F_NEXT != 0 {
            let next = match self.read_desc(current.next, mem) {
                Some(d) => d,
                None => break,
            };
            data_descs.push(next);
            current = next;
        }

        // The last descriptor with WRITE flag and length 1 is the status byte.
        if let Some(last) = data_descs.last() {
            if last.len == 1 && last.flags & VIRTQ_DESC_F_WRITE != 0 {
                status_desc = data_descs.pop();
            }
        }

        // 4. Dispatch the request by type.
        let (status_byte, bytes, op) = match hdr.type_ {
            VIRTIO_BLK_T_IN => self.handle_read(hdr.sector, &data_descs, mem),
            VIRTIO_BLK_T_OUT => self.handle_write(hdr.sector, &data_descs, mem),
            VIRTIO_BLK_T_FLUSH => self.handle_flush(),
            other => {
                tracing::debug!("virtio-blk: unsupported request type {other}");
                (VIRTIO_BLK_S_UNSUPP, 0, IoOp::Read)
            }
        };

        // 5. Write the status byte to the status descriptor.
        let mut used_len = 0_u32;
        if let Some(sd) = status_desc {
            let _ = mem.write_obj(status_byte, GuestAddress(sd.addr));
            used_len += 1; // status byte is writable
        }

        // For reads, add the data bytes to used_len (data descs are writable).
        if matches!(op, IoOp::Read) {
            #[allow(clippy::cast_possible_truncation)]
            {
                used_len = used_len.saturating_add(bytes as u32);
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        let duration_ns = start.elapsed().as_nanos() as u64;

        let completed = CompletedIo {
            op,
            bytes,
            duration_ns,
        };

        (used_len, Some(completed))
    }

    /// Reads a virtqueue descriptor by index from guest memory.
    fn read_desc(&self, idx: u16, mem: &GuestMemoryMmap) -> Option<VirtqDesc> {
        if idx >= self.queue.num {
            tracing::warn!(
                "virtio-blk: descriptor index {idx} >= queue size {}",
                self.queue.num,
            );
            return None;
        }
        let addr = self.queue.desc_table + u64::from(idx) * 16;
        match mem.read_obj(GuestAddress(addr)) {
            Ok(desc) => Some(desc),
            Err(e) => {
                tracing::warn!("virtio-blk: failed to read descriptor {idx}: {e}");
                None
            }
        }
    }

    // ── I/O handlers ────────────────────────────────────────────────

    /// Handles a read request: reads sectors from backing file into guest memory.
    fn handle_read(
        &mut self,
        sector: u64,
        data_descs: &[VirtqDesc],
        mem: &GuestMemoryMmap,
    ) -> (u8, u64, IoOp) {
        let mut offset = sector * SECTOR_SIZE;
        let mut total_bytes: u64 = 0;

        for desc in data_descs {
            // Read data descriptors must be device-writable.
            if desc.flags & VIRTQ_DESC_F_WRITE == 0 {
                continue;
            }

            let len = desc.len as usize;
            let mut buf = vec![0u8; len];

            if let Err(e) = self.backing.seek(SeekFrom::Start(offset)) {
                tracing::warn!("virtio-blk read: seek to {offset} failed: {e}");
                return (VIRTIO_BLK_S_IOERR, total_bytes, IoOp::Read);
            }

            match self.backing.read_exact(&mut buf) {
                Ok(()) => {
                    if let Err(e) = mem.write(&buf, GuestAddress(desc.addr)) {
                        tracing::warn!("virtio-blk read: guest write at {:#x} failed: {e}", desc.addr);
                        return (VIRTIO_BLK_S_IOERR, total_bytes, IoOp::Read);
                    }
                    offset += len as u64;
                    total_bytes += len as u64;
                }
                Err(e) => {
                    tracing::warn!("virtio-blk read: file read at {offset} failed: {e}");
                    return (VIRTIO_BLK_S_IOERR, total_bytes, IoOp::Read);
                }
            }
        }

        (VIRTIO_BLK_S_OK, total_bytes, IoOp::Read)
    }

    /// Handles a write request: reads from guest memory, writes sectors to backing file.
    fn handle_write(
        &mut self,
        sector: u64,
        data_descs: &[VirtqDesc],
        mem: &GuestMemoryMmap,
    ) -> (u8, u64, IoOp) {
        let mut offset = sector * SECTOR_SIZE;
        let mut total_bytes: u64 = 0;

        for desc in data_descs {
            // Write data descriptors must be device-readable (NOT writable).
            if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
                continue;
            }

            let len = desc.len as usize;
            let mut buf = vec![0u8; len];

            if let Err(e) = mem.read(&mut buf, GuestAddress(desc.addr)) {
                tracing::warn!("virtio-blk write: guest read at {:#x} failed: {e}", desc.addr);
                return (VIRTIO_BLK_S_IOERR, total_bytes, IoOp::Write);
            }

            if let Err(e) = self.backing.seek(SeekFrom::Start(offset)) {
                tracing::warn!("virtio-blk write: seek to {offset} failed: {e}");
                return (VIRTIO_BLK_S_IOERR, total_bytes, IoOp::Write);
            }

            match self.backing.write_all(&buf) {
                Ok(()) => {
                    offset += len as u64;
                    total_bytes += len as u64;
                }
                Err(e) => {
                    tracing::warn!("virtio-blk write: file write at {offset} failed: {e}");
                    return (VIRTIO_BLK_S_IOERR, total_bytes, IoOp::Write);
                }
            }
        }

        (VIRTIO_BLK_S_OK, total_bytes, IoOp::Write)
    }

    /// Handles a flush request: syncs the backing file to disk.
    fn handle_flush(&mut self) -> (u8, u64, IoOp) {
        match self.backing.sync_all() {
            Ok(()) => (VIRTIO_BLK_S_OK, 0, IoOp::Flush),
            Err(e) => {
                tracing::warn!("virtio-blk flush failed: {e}");
                (VIRTIO_BLK_S_IOERR, 0, IoOp::Flush)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_new_creates_backing_file() {
        let dir = tempdir().unwrap();
        let disk_path = dir.path().join("disk.img");
        let blk = VirtioBlock::new(&disk_path).unwrap();
        assert_eq!(blk.capacity_sectors, DEFAULT_DISK_BYTES / SECTOR_SIZE);
        assert!(disk_path.exists());
        assert_eq!(disk_path.metadata().unwrap().len(), DEFAULT_DISK_BYTES);
    }

    #[test]
    fn test_new_uses_existing_file() {
        let dir = tempdir().unwrap();
        let disk_path = dir.path().join("disk.img");
        let size = 1024 * 1024; // 1 MiB
        std::fs::write(&disk_path, vec![0u8; size]).unwrap();

        let blk = VirtioBlock::new(&disk_path).unwrap();
        #[allow(clippy::cast_possible_truncation)]
        let expected = (size as u64) / SECTOR_SIZE;
        assert_eq!(blk.capacity_sectors, expected);
    }

    #[test]
    fn test_mmio_read_magic() {
        let dir = tempdir().unwrap();
        let blk = VirtioBlock::new(dir.path().join("disk.img")).unwrap();
        let mut data = [0u8; 4];
        blk.mmio_read(REG_MAGIC, &mut data);
        assert_eq!(u32::from_le_bytes(data), MAGIC);
    }

    #[test]
    fn test_mmio_read_version() {
        let dir = tempdir().unwrap();
        let blk = VirtioBlock::new(dir.path().join("disk.img")).unwrap();
        let mut data = [0u8; 4];
        blk.mmio_read(REG_VERSION, &mut data);
        assert_eq!(u32::from_le_bytes(data), MMIO_VERSION);
    }

    #[test]
    fn test_mmio_read_device_id() {
        let dir = tempdir().unwrap();
        let blk = VirtioBlock::new(dir.path().join("disk.img")).unwrap();
        let mut data = [0u8; 4];
        blk.mmio_read(REG_DEVICE_ID, &mut data);
        assert_eq!(u32::from_le_bytes(data), DEVICE_ID_BLK);
    }

    #[test]
    fn test_mmio_read_config_capacity() {
        let dir = tempdir().unwrap();
        let blk = VirtioBlock::new(dir.path().join("disk.img")).unwrap();
        // Read capacity as two 32-bit reads (low then high)
        let mut low = [0u8; 4];
        let mut high = [0u8; 4];
        blk.mmio_read(REG_CONFIG_START, &mut low);
        blk.mmio_read(REG_CONFIG_START + 4, &mut high);
        let capacity = u64::from(u32::from_le_bytes(low))
            | (u64::from(u32::from_le_bytes(high)) << 32);
        assert_eq!(capacity, DEFAULT_DISK_BYTES / SECTOR_SIZE);
    }

    #[test]
    fn test_device_reset() {
        let dir = tempdir().unwrap();
        let mut blk = VirtioBlock::new(dir.path().join("disk.img")).unwrap();
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)])
            .expect("create guest memory");

        // Set some state
        blk.status = 0x0F;
        blk.driver_features = 0x1234;
        blk.interrupt_status = 1;

        // Write 0 to status register triggers reset
        blk.mmio_write(REG_STATUS, &0u32.to_le_bytes(), &mem);

        assert_eq!(blk.status, 0);
        assert_eq!(blk.driver_features, 0);
        assert_eq!(blk.interrupt_status, 0);
        assert!(!blk.queue.ready);
    }

    #[test]
    fn test_feature_negotiation() {
        let dir = tempdir().unwrap();
        let mut blk = VirtioBlock::new(dir.path().join("disk.img")).unwrap();
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)])
            .expect("create guest memory");

        // Read device features page 0 (low 32 bits)
        blk.mmio_write(REG_DEVICE_FEATURES_SEL, &0u32.to_le_bytes(), &mem);
        let mut data = [0u8; 4];
        blk.mmio_read(REG_DEVICE_FEATURES, &mut data);
        let features_lo = u32::from_le_bytes(data);
        // VIRTIO_BLK_F_FLUSH is bit 9
        assert_ne!(features_lo & (1 << 9), 0);

        // Read device features page 1 (high 32 bits)
        blk.mmio_write(REG_DEVICE_FEATURES_SEL, &1u32.to_le_bytes(), &mem);
        blk.mmio_read(REG_DEVICE_FEATURES, &mut data);
        let features_hi = u32::from_le_bytes(data);
        // VIRTIO_F_VERSION_1 is bit 32, so bit 0 of page 1
        assert_ne!(features_hi & 1, 0);
    }
}
