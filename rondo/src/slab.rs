//! Memory-mapped slab file format for Rondo time-series storage.
//!
//! This module implements the core storage format for time-series data in Rondo.
//! Slabs are memory-mapped files containing a ring buffer of time-series samples
//! arranged in columnar format for optimal cache performance.
//!
//! # File Format
//!
//! ```text
//! [0..64)        Header (SlabHeader)
//! [64..64+N)     Series directory (N = max_series * 4 bytes)
//! [64+N..)       Data region (columnar: timestamps then per-series f64 values)
//! ```
//!
//! # Safety
//!
//! This module uses unsafe operations for direct memory access to the mmap'd
//! region. All unsafe blocks are documented and bounds-checked during slab
//! creation/opening. The hot path write operations assume valid indices for
//! maximum performance.

use std::fs::OpenOptions;
use std::path::Path;
use std::ptr;

use memmap2::MmapMut;

use crate::error::{Result, SlabIoError};

/// Magic bytes identifying a Rondo slab file.
const SLAB_MAGIC: [u8; 4] = *b"RNDO";

/// Current slab format version.
const SLAB_VERSION: u32 = 1;

/// Size of the slab header in bytes.
const HEADER_SIZE: usize = 64;

/// Size of each series directory entry in bytes (u32 column offset).
const SERIES_DIR_ENTRY_SIZE: usize = 4;

/// Size of timestamp column entries in bytes.
const TIMESTAMP_SIZE: usize = 8;

/// Size of value column entries in bytes.
const VALUE_SIZE: usize = 8;

/// Header structure for slab files.
///
/// This header is written at the beginning of each slab file and contains
/// metadata about the slab's configuration and current state. The repr(C)
/// layout ensures consistent binary format across platforms.
#[repr(C)]
#[derive(Debug, Clone)]
struct SlabHeader {
    /// Magic bytes for file type identification.
    magic: [u8; 4],
    /// Slab format version number.
    version: u32,
    /// Hash of the schema configuration.
    schema_hash: u64,
    /// Number of time slots in the ring buffer.
    slot_count: u32,
    /// Maximum number of series columns.
    max_series: u32,
    /// Sample interval in nanoseconds.
    interval_ns: u64,
    /// Current write cursor position.
    write_cursor: u32,
    /// Number of currently registered series.
    series_count: u32,
    /// Reserved space for future use (padding to 64 bytes).
    _reserved: [u8; 16],
}

impl SlabHeader {
    /// Creates a new slab header with the given configuration.
    fn new(schema_hash: u64, slot_count: u32, max_series: u32, interval_ns: u64) -> Self {
        Self {
            magic: SLAB_MAGIC,
            version: SLAB_VERSION,
            schema_hash,
            slot_count,
            max_series,
            interval_ns,
            write_cursor: 0,
            series_count: 0,
            _reserved: [0; 16],
        }
    }

    /// Validates the header magic and version.
    ///
    /// # Errors
    ///
    /// Returns [`SlabIoError::CorruptedSlab`] if the header is invalid.
    fn validate(&self, path: &str) -> Result<()> {
        if self.magic != SLAB_MAGIC {
            return Err(SlabIoError::CorruptedSlab {
                path: path.to_string(),
                reason: format!(
                    "invalid magic bytes: expected {:?}, found {:?}",
                    SLAB_MAGIC, self.magic
                ),
            }
            .into());
        }

        if self.version != SLAB_VERSION {
            return Err(SlabIoError::CorruptedSlab {
                path: path.to_string(),
                reason: format!(
                    "unsupported version: expected {}, found {}",
                    SLAB_VERSION, self.version
                ),
            }
            .into());
        }

        Ok(())
    }
}

/// Helper for computing slab layout sizes and offsets.
#[derive(Debug, Clone, Copy)]
struct SlabLayout {
    /// Total file size in bytes.
    file_size: usize,
    /// Offset to the series directory.
    series_dir_offset: usize,
    /// Offset to the timestamp column.
    timestamp_column_offset: usize,
    /// Offset to the first value column.
    value_columns_offset: usize,
    /// Size of each value column in bytes.
    value_column_size: usize,
}

impl SlabLayout {
    /// Computes the layout for a slab with the given parameters.
    fn new(slot_count: u32, max_series: u32) -> Self {
        let slot_count = slot_count as usize;
        let max_series = max_series as usize;

        // Series directory: max_series * 4 bytes per entry
        let series_dir_size = max_series * SERIES_DIR_ENTRY_SIZE;
        let series_dir_offset = HEADER_SIZE;
        let data_region_offset = series_dir_offset + series_dir_size;

        // Data region: timestamp column + value columns
        let timestamp_column_size = slot_count * TIMESTAMP_SIZE;
        let value_column_size = slot_count * VALUE_SIZE;
        let total_value_columns_size = max_series * value_column_size;

        let timestamp_column_offset = data_region_offset;
        let value_columns_offset = timestamp_column_offset + timestamp_column_size;

        let file_size = value_columns_offset + total_value_columns_size;

        Self {
            file_size,
            series_dir_offset,
            timestamp_column_offset,
            value_columns_offset,
            value_column_size,
        }
    }

    /// Returns the byte offset for a specific value column.
    fn value_column_offset(&self, series_column: u32) -> usize {
        self.value_columns_offset + (series_column as usize * self.value_column_size)
    }
}

/// Memory-mapped slab file for storing time-series data.
///
/// A slab contains a ring buffer of time-series samples arranged in columnar
/// format. Data is stored in a memory-mapped file for persistence and
/// zero-copy access.
///
/// # Thread Safety
///
/// Slab is designed for single-writer, multiple-reader access patterns.
/// The memory mapping is `Send + Sync` safe as long as writes are properly
/// coordinated by the caller.
#[derive(Debug)]
pub struct Slab {
    /// Memory mapping of the slab file.
    mmap: MmapMut,
    /// Pre-computed layout information for fast offset calculations.
    layout: SlabLayout,
    /// Path to the slab file (for error reporting).
    path: String,
}

// SAFETY: Slab is designed for single-writer access patterns with proper
// external synchronization. The memory mapping itself is thread-safe.
unsafe impl Send for Slab {}

// SAFETY: Slab uses memory-mapped files which are safe to share across threads.
// All access is through validated offsets and the single-writer pattern ensures
// no data races. Read operations are naturally thread-safe.
unsafe impl Sync for Slab {}

impl Slab {
    /// Creates a new slab file with the specified configuration.
    ///
    /// The file is pre-allocated to the exact size needed and initialized
    /// with appropriate defaults (NaN for data values, zero for metadata).
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the slab file should be created
    /// * `schema_hash` - Hash of the schema configuration
    /// * `slot_count` - Number of time slots in the ring buffer
    /// * `max_series` - Maximum number of series that can be stored
    /// * `interval_ns` - Sample interval in nanoseconds
    ///
    /// # Errors
    ///
    /// Returns [`SlabIoError`] if file creation or memory mapping fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use rondo::slab::Slab;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let slab = Slab::create(
    ///     "data.slab",
    ///     0x1234567890abcdef,
    ///     3600,  // 1 hour of 1-second samples
    ///     100,   // Up to 100 series
    ///     1_000_000_000, // 1 second interval
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn create<P: AsRef<Path>>(
        path: P,
        schema_hash: u64,
        slot_count: u32,
        max_series: u32,
        interval_ns: u64,
    ) -> Result<Self> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy().to_string();

        // Compute layout
        let layout = SlabLayout::new(slot_count, max_series);

        // Create and pre-allocate the file
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| SlabIoError::WriteFailed {
                path: path_str.clone(),
                offset: 0,
                source: e,
            })?;

        file.set_len(layout.file_size as u64)
            .map_err(|e| SlabIoError::WriteFailed {
                path: path_str.clone(),
                offset: 0,
                source: e,
            })?;

        // Memory map the file
        // SAFETY: The file was just created and has the correct size. We have exclusive
        // access to the file descriptor.
        let mut mmap = unsafe {
            MmapMut::map_mut(&file).map_err(|e| SlabIoError::WriteFailed {
                path: path_str.clone(),
                offset: 0,
                source: e,
            })?
        };

        // Initialize header
        let header = SlabHeader::new(schema_hash, slot_count, max_series, interval_ns);
        // SAFETY: The mmap is valid and large enough for SlabHeader. The pointer
        // is properly aligned for SlabHeader due to repr(C) and file start alignment.
        unsafe {
            ptr::write(mmap.as_mut_ptr() as *mut SlabHeader, header);
        }

        // Initialize series directory to zeros (invalid column indices)
        // SAFETY: series_dir_offset is computed from validated layout and is within mmap bounds.
        let series_dir_ptr = unsafe { mmap.as_mut_ptr().add(layout.series_dir_offset) as *mut u32 };
        for i in 0..max_series {
            // SAFETY: We're writing within the series directory region that was
            // pre-allocated. The index i is bounded by max_series.
            unsafe {
                ptr::write(series_dir_ptr.add(i as usize), u32::MAX);
            }
        }

        // Initialize data region with NaN values
        Self::initialize_data_region(&mut mmap, &layout, slot_count, max_series);

        let slab = Self {
            mmap,
            layout,
            path: path_str,
        };

        Ok(slab)
    }

    /// Opens an existing slab file.
    ///
    /// Validates the header and memory maps the file for access.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the existing slab file
    ///
    /// # Errors
    ///
    /// Returns [`SlabIoError`] if the file cannot be opened, is corrupted,
    /// or memory mapping fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use rondo::slab::Slab;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let slab = Slab::open("existing.slab")?;
    /// println!("Opened slab with {} slots", slab.slot_count());
    /// # Ok(())
    /// # }
    /// ```
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy().to_string();

        // Open the file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| SlabIoError::ReadFailed {
                path: path_str.clone(),
                offset: 0,
                source: e,
            })?;

        // Memory map the file
        // SAFETY: The file was successfully opened and we have read/write access.
        let mmap = unsafe {
            MmapMut::map_mut(&file).map_err(|e| SlabIoError::ReadFailed {
                path: path_str.clone(),
                offset: 0,
                source: e,
            })?
        };

        // Validate file size
        if mmap.len() < HEADER_SIZE {
            return Err(SlabIoError::CorruptedSlab {
                path: path_str,
                reason: format!(
                    "file too small: {} bytes, expected at least {}",
                    mmap.len(),
                    HEADER_SIZE
                ),
            }
            .into());
        }

        // Read and validate header
        // SAFETY: We verified the file is at least HEADER_SIZE bytes and the pointer
        // is properly aligned for SlabHeader due to file start alignment.
        let header = unsafe { ptr::read(mmap.as_ptr() as *const SlabHeader) };
        header.validate(&path_str)?;

        // Compute layout and validate file size
        let layout = SlabLayout::new(header.slot_count, header.max_series);
        if mmap.len() != layout.file_size {
            return Err(SlabIoError::CorruptedSlab {
                path: path_str,
                reason: format!(
                    "file size mismatch: {} bytes, expected {}",
                    mmap.len(),
                    layout.file_size
                ),
            }
            .into());
        }

        let slab = Self {
            mmap,
            layout,
            path: path_str,
        };

        Ok(slab)
    }

    /// Initializes the data region with NaN values.
    fn initialize_data_region(
        mmap: &mut MmapMut,
        layout: &SlabLayout,
        slot_count: u32,
        max_series: u32,
    ) {
        // Initialize timestamp column with zeros
        // SAFETY: timestamp_column_offset is computed from validated layout parameters.
        let timestamp_ptr = unsafe { mmap.as_mut_ptr().add(layout.timestamp_column_offset) as *mut u64 };
        for i in 0..slot_count {
            // SAFETY: We're writing within the pre-allocated timestamp column region.
            // The index i is bounded by slot_count.
            unsafe {
                ptr::write(timestamp_ptr.add(i as usize), 0);
            }
        }

        // Initialize value columns with NaN
        let nan_bits = f64::NAN.to_bits();
        for series in 0..max_series {
            let column_offset = layout.value_column_offset(series);
            // SAFETY: column_offset is computed from validated layout parameters.
            let column_ptr = unsafe { mmap.as_mut_ptr().add(column_offset) as *mut u64 };
            for i in 0..slot_count {
                // SAFETY: We're writing within the pre-allocated value column region.
                // Both series and i are bounded by their respective limits.
                unsafe {
                    ptr::write(column_ptr.add(i as usize), nan_bits);
                }
            }
        }
    }

    /// Returns the schema hash from the header.
    pub fn schema_hash(&self) -> u64 {
        // SAFETY: The slab was validated during open/create, ensuring the header is valid.
        let header = unsafe { ptr::read(self.mmap.as_ptr() as *const SlabHeader) };
        header.schema_hash
    }

    /// Returns the number of slots in the ring buffer.
    pub fn slot_count(&self) -> u32 {
        // SAFETY: The slab was validated during open/create, ensuring the header is valid.
        let header = unsafe { ptr::read(self.mmap.as_ptr() as *const SlabHeader) };
        header.slot_count
    }

    /// Returns the maximum number of series.
    pub fn max_series(&self) -> u32 {
        // SAFETY: The slab was validated during open/create, ensuring the header is valid.
        let header = unsafe { ptr::read(self.mmap.as_ptr() as *const SlabHeader) };
        header.max_series
    }

    /// Returns the sample interval in nanoseconds.
    pub fn interval_ns(&self) -> u64 {
        // SAFETY: The slab was validated during open/create, ensuring the header is valid.
        let header = unsafe { ptr::read(self.mmap.as_ptr() as *const SlabHeader) };
        header.interval_ns
    }

    /// Returns the current write cursor position.
    pub fn write_cursor(&self) -> u32 {
        // SAFETY: The slab was validated during open/create, ensuring the header is valid.
        let header = unsafe { ptr::read(self.mmap.as_ptr() as *const SlabHeader) };
        header.write_cursor
    }

    /// Sets the write cursor position.
    ///
    /// # Arguments
    ///
    /// * `pos` - New cursor position (must be < slot_count)
    ///
    /// # Safety
    ///
    /// The caller must ensure `pos` is within valid bounds. This is not
    /// checked for performance on the hot path.
    pub fn set_write_cursor(&mut self, pos: u32) {
        let header_ptr = self.mmap.as_mut_ptr() as *mut SlabHeader;
        // SAFETY: We're modifying only the write_cursor field of a properly
        // initialized SlabHeader. The pointer is valid as it points to the
        // start of our memory mapping.
        unsafe {
            ptr::write(&mut (*header_ptr).write_cursor, pos);
        }
    }

    /// Returns the current number of registered series.
    pub fn series_count(&self) -> u32 {
        // SAFETY: The slab was validated during open/create, ensuring the header is valid.
        let header = unsafe { ptr::read(self.mmap.as_ptr() as *const SlabHeader) };
        header.series_count
    }

    /// Sets the number of registered series.
    ///
    /// # Arguments
    ///
    /// * `count` - New series count (must be <= max_series)
    ///
    /// # Safety
    ///
    /// The caller must ensure `count` is within valid bounds.
    pub fn set_series_count(&mut self, count: u32) {
        let header_ptr = self.mmap.as_mut_ptr() as *mut SlabHeader;
        // SAFETY: We're modifying only the series_count field of a properly
        // initialized SlabHeader. The pointer is valid as it points to the
        // start of our memory mapping.
        unsafe {
            ptr::write(&mut (*header_ptr).series_count, count);
        }
    }

    /// Writes a timestamp to the specified slot.
    ///
    /// # Arguments
    ///
    /// * `slot_index` - Ring buffer slot index
    /// * `timestamp` - Timestamp value in nanoseconds
    ///
    /// # Safety
    ///
    /// The caller must ensure `slot_index` is within valid bounds
    /// (< slot_count). This is not checked for performance on the hot path.
    pub fn write_timestamp(&mut self, slot_index: u32, timestamp: u64) {
        let offset = self.layout.timestamp_column_offset + (slot_index as usize * TIMESTAMP_SIZE);
        // SAFETY: The offset is computed from validated layout parameters
        // and the caller guarantees slot_index is in bounds.
        let ptr = unsafe { self.mmap.as_mut_ptr().add(offset) as *mut u64 };
        // SAFETY: The pointer is valid and points to a properly aligned u64
        // within the memory-mapped region.
        unsafe {
            ptr::write(ptr, timestamp);
        }
    }

    /// Reads a timestamp from the specified slot.
    ///
    /// # Arguments
    ///
    /// * `slot_index` - Ring buffer slot index
    ///
    /// # Returns
    ///
    /// The timestamp value, or 0 if the slot is uninitialized.
    ///
    /// # Safety
    ///
    /// The caller must ensure `slot_index` is within valid bounds.
    pub fn read_timestamp(&self, slot_index: u32) -> u64 {
        let offset = self.layout.timestamp_column_offset + (slot_index as usize * TIMESTAMP_SIZE);
        // SAFETY: The offset is computed from validated layout parameters
        // and the caller guarantees slot_index is in bounds.
        let ptr = unsafe { self.mmap.as_ptr().add(offset) as *const u64 };
        // SAFETY: The pointer is valid and points to a properly aligned u64
        // within the memory-mapped region.
        unsafe {
            ptr::read(ptr)
        }
    }

    /// Writes a value to the specified slot and series column.
    ///
    /// # Arguments
    ///
    /// * `slot_index` - Ring buffer slot index
    /// * `series_column` - Series column index
    /// * `value` - The f64 value to write
    ///
    /// # Safety
    ///
    /// The caller must ensure both `slot_index` and `series_column` are
    /// within valid bounds. This is not checked for performance on the hot path.
    pub fn write_value(&mut self, slot_index: u32, series_column: u32, value: f64) {
        let column_offset = self.layout.value_column_offset(series_column);
        let offset = column_offset + (slot_index as usize * VALUE_SIZE);
        // SAFETY: Offset computation includes validated layout parameters
        // and the caller guarantees both indices are in bounds.
        let ptr = unsafe { self.mmap.as_mut_ptr().add(offset) as *mut f64 };
        // SAFETY: The pointer is valid and points to a properly aligned f64
        // within the memory-mapped region.
        unsafe {
            ptr::write(ptr, value);
        }
    }

    /// Reads a value from the specified slot and series column.
    ///
    /// # Arguments
    ///
    /// * `slot_index` - Ring buffer slot index
    /// * `series_column` - Series column index
    ///
    /// # Returns
    ///
    /// The f64 value, or NaN if the slot is uninitialized.
    ///
    /// # Safety
    ///
    /// The caller must ensure both indices are within valid bounds.
    pub fn read_value(&self, slot_index: u32, series_column: u32) -> f64 {
        let column_offset = self.layout.value_column_offset(series_column);
        let offset = column_offset + (slot_index as usize * VALUE_SIZE);
        // SAFETY: Offset computation includes validated layout parameters
        // and the caller guarantees both indices are in bounds.
        let ptr = unsafe { self.mmap.as_ptr().add(offset) as *const f64 };
        // SAFETY: The pointer is valid and points to a properly aligned f64
        // within the memory-mapped region.
        unsafe {
            ptr::read(ptr)
        }
    }

    /// Gets the column offset for a series from the series directory.
    ///
    /// # Arguments
    ///
    /// * `series_id` - The series ID to look up
    ///
    /// # Returns
    ///
    /// The column offset, or `None` if the series is not registered.
    pub fn get_series_column(&self, series_id: u32) -> Option<u32> {
        if series_id >= self.max_series() {
            return None;
        }

        let offset = self.layout.series_dir_offset + (series_id as usize * SERIES_DIR_ENTRY_SIZE);
        // SAFETY: The offset is computed from validated layout parameters
        // and series_id was bounds-checked above.
        let ptr = unsafe { self.mmap.as_ptr().add(offset) as *const u32 };
        // SAFETY: The pointer is valid and points to a properly aligned u32
        // within the series directory region.
        let column = unsafe { ptr::read(ptr) };

        if column == u32::MAX {
            None
        } else {
            Some(column)
        }
    }

    /// Sets the column offset for a series in the series directory.
    ///
    /// # Arguments
    ///
    /// * `series_id` - The series ID to register
    /// * `column` - The column offset to assign
    ///
    /// # Safety
    ///
    /// The caller must ensure `series_id` is within bounds.
    pub fn set_series_column(&mut self, series_id: u32, column: u32) {
        let offset = self.layout.series_dir_offset + (series_id as usize * SERIES_DIR_ENTRY_SIZE);
        // SAFETY: The offset is computed from validated layout parameters
        // and the caller guarantees series_id is in bounds.
        let ptr = unsafe { self.mmap.as_mut_ptr().add(offset) as *mut u32 };
        // SAFETY: The pointer is valid and points to a properly aligned u32
        // within the series directory region.
        unsafe {
            ptr::write(ptr, column);
        }
    }

    /// Syncs the memory mapping to disk.
    ///
    /// # Errors
    ///
    /// Returns [`SlabIoError::SyncFailed`] if the sync operation fails.
    pub fn sync(&self) -> Result<()> {
        self.mmap.flush().map_err(|e| {
            SlabIoError::SyncFailed {
                path: self.path.clone(),
                source: e,
            }
            .into()
        })
    }

    /// Returns the path to this slab file.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        // Memory mapping is automatically unmapped by memmap2
        // No additional cleanup needed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_slab_layout() {
        let layout = SlabLayout::new(1000, 10);

        // Header: 64 bytes
        // Series dir: 10 * 4 = 40 bytes
        // Data region starts at: 64 + 40 = 104
        // Timestamp column: 1000 * 8 = 8000 bytes
        // Value columns: 10 * (1000 * 8) = 80000 bytes
        // Total: 104 + 8000 + 80000 = 88104 bytes

        assert_eq!(layout.series_dir_offset, 64);
        assert_eq!(layout.timestamp_column_offset, 104);
        assert_eq!(layout.value_columns_offset, 8104);
        assert_eq!(layout.value_column_size, 8000);
        assert_eq!(layout.file_size, 88104);

        // Check value column offsets
        assert_eq!(layout.value_column_offset(0), 8104);
        assert_eq!(layout.value_column_offset(1), 16104);
        assert_eq!(layout.value_column_offset(9), 80104);
    }

    #[test]
    fn test_slab_create_and_open() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("test.slab");

        // Create a new slab
        let slab = Slab::create(
            &slab_path,
            0x1234567890abcdef,
            100, // 100 slots
            5,   // 5 series max
            1_000_000_000, // 1 second interval
        )
        .unwrap();

        assert_eq!(slab.schema_hash(), 0x1234567890abcdef);
        assert_eq!(slab.slot_count(), 100);
        assert_eq!(slab.max_series(), 5);
        assert_eq!(slab.interval_ns(), 1_000_000_000);
        assert_eq!(slab.write_cursor(), 0);
        assert_eq!(slab.series_count(), 0);

        drop(slab);

        // Reopen the slab
        let slab = Slab::open(&slab_path).unwrap();

        assert_eq!(slab.schema_hash(), 0x1234567890abcdef);
        assert_eq!(slab.slot_count(), 100);
        assert_eq!(slab.max_series(), 5);
        assert_eq!(slab.interval_ns(), 1_000_000_000);
        assert_eq!(slab.write_cursor(), 0);
        assert_eq!(slab.series_count(), 0);
    }

    #[test]
    fn test_slab_header_updates() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("test.slab");

        let mut slab = Slab::create(&slab_path, 0x1234567890abcdef, 100, 5, 1_000_000_000)
            .unwrap();

        // Update cursor and series count
        slab.set_write_cursor(42);
        slab.set_series_count(3);

        assert_eq!(slab.write_cursor(), 42);
        assert_eq!(slab.series_count(), 3);

        drop(slab);

        // Verify persistence
        let slab = Slab::open(&slab_path).unwrap();
        assert_eq!(slab.write_cursor(), 42);
        assert_eq!(slab.series_count(), 3);
    }

    #[test]
    fn test_timestamp_operations() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("test.slab");

        let mut slab = Slab::create(&slab_path, 0x1234567890abcdef, 100, 5, 1_000_000_000)
            .unwrap();

        // Initially timestamps should be zero
        assert_eq!(slab.read_timestamp(0), 0);
        assert_eq!(slab.read_timestamp(50), 0);
        assert_eq!(slab.read_timestamp(99), 0);

        // Write some timestamps
        slab.write_timestamp(0, 1000);
        slab.write_timestamp(50, 2000);
        slab.write_timestamp(99, 3000);

        // Verify reads
        assert_eq!(slab.read_timestamp(0), 1000);
        assert_eq!(slab.read_timestamp(50), 2000);
        assert_eq!(slab.read_timestamp(99), 3000);

        // Other slots should still be zero
        assert_eq!(slab.read_timestamp(1), 0);
        assert_eq!(slab.read_timestamp(49), 0);
        assert_eq!(slab.read_timestamp(98), 0);
    }

    #[test]
    fn test_value_operations() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("test.slab");

        let mut slab = Slab::create(&slab_path, 0x1234567890abcdef, 100, 5, 1_000_000_000)
            .unwrap();

        // Initially values should be NaN
        assert!(slab.read_value(0, 0).is_nan());
        assert!(slab.read_value(50, 2).is_nan());
        assert!(slab.read_value(99, 4).is_nan());

        // Write some values
        slab.write_value(0, 0, 42.0);
        slab.write_value(50, 2, 3.125);
        slab.write_value(99, 4, -273.15);

        // Verify reads
        assert_eq!(slab.read_value(0, 0), 42.0);
        assert_eq!(slab.read_value(50, 2), 3.125);
        assert_eq!(slab.read_value(99, 4), -273.15);

        // Other slots/columns should still be NaN
        assert!(slab.read_value(0, 1).is_nan());
        assert!(slab.read_value(1, 0).is_nan());
        assert!(slab.read_value(50, 0).is_nan());
    }

    #[test]
    fn test_series_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("test.slab");

        let mut slab = Slab::create(&slab_path, 0x1234567890abcdef, 100, 5, 1_000_000_000)
            .unwrap();

        // Initially no series registered
        assert_eq!(slab.get_series_column(0), None);
        assert_eq!(slab.get_series_column(4), None);

        // Register some series
        slab.set_series_column(0, 0);  // series 0 -> column 0
        slab.set_series_column(2, 1);  // series 2 -> column 1
        slab.set_series_column(4, 3);  // series 4 -> column 3

        // Verify lookups
        assert_eq!(slab.get_series_column(0), Some(0));
        assert_eq!(slab.get_series_column(2), Some(1));
        assert_eq!(slab.get_series_column(4), Some(3));

        // Unregistered series should return None
        assert_eq!(slab.get_series_column(1), None);
        assert_eq!(slab.get_series_column(3), None);
    }

    #[test]
    fn test_invalid_header() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("invalid.slab");

        // Create a file with invalid magic but correct size
        let mut invalid_header = vec![0u8; 64];
        invalid_header[0..4].copy_from_slice(b"BAD\0");
        fs::write(&slab_path, invalid_header).unwrap();

        let result = Slab::open(&slab_path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid magic bytes"));
    }

    #[test]
    fn test_file_size_validation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("small.slab");

        // Create a file that's too small
        fs::write(&slab_path, b"small").unwrap();

        let result = Slab::open(&slab_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("file too small"));
    }

    #[test]
    fn test_persistence() {
        let temp_dir = tempfile::tempdir().unwrap();
        let slab_path = temp_dir.path().join("persist.slab");

        // Create slab and write some data
        {
            let mut slab = Slab::create(&slab_path, 0x1234567890abcdef, 10, 3, 1_000_000_000)
                .unwrap();

            slab.set_write_cursor(5);
            slab.set_series_count(2);
            slab.set_series_column(0, 0);
            slab.set_series_column(1, 1);

            slab.write_timestamp(0, 1000000000);
            slab.write_timestamp(5, 2000000000);

            slab.write_value(0, 0, 42.5);
            slab.write_value(5, 1, -17.25);

            slab.sync().unwrap();
        }

        // Reopen and verify data
        {
            let slab = Slab::open(&slab_path).unwrap();

            assert_eq!(slab.write_cursor(), 5);
            assert_eq!(slab.series_count(), 2);
            assert_eq!(slab.get_series_column(0), Some(0));
            assert_eq!(slab.get_series_column(1), Some(1));

            assert_eq!(slab.read_timestamp(0), 1000000000);
            assert_eq!(slab.read_timestamp(5), 2000000000);

            assert_eq!(slab.read_value(0, 0), 42.5);
            assert_eq!(slab.read_value(5, 1), -17.25);

            // Unwritten slots should have default values
            assert_eq!(slab.read_timestamp(1), 0);
            assert!(slab.read_value(1, 0).is_nan());
        }
    }
}

