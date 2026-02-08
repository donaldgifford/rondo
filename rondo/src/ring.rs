//! Ring buffer implementation for Rondo time-series storage.
//!
//! This module provides a higher-level ring buffer interface over the low-level
//! slab operations. It implements circular buffer semantics with automatic
//! slot computation from timestamps and efficient read/write operations.
//!
//! # Key Features
//!
//! - Zero-allocation write path using memory-mapped storage
//! - Automatic slot computation from timestamps
//! - Wraparound detection and handling for both reads and writes
//! - Lazy iterators for efficient range queries
//! - NaN sentinel values for unwritten slots
//!
//! # Design
//!
//! The ring buffer is a thin wrapper around a `Slab` that adds ring semantics:
//! - Slot computation: `slot_index = (timestamp_ns / interval_ns) % slot_count`
//! - Write cursor tracks the newest written slot
//! - Wraparound detection enables proper read ordering
//! - NaN values indicate unwritten or missing data


use crate::error::{QueryError, RecordError, Result};
use crate::slab::Slab;

/// A ring buffer wrapper around a slab that provides time-series semantics.
///
/// The ring buffer automatically maps timestamps to slot indices and handles
/// circular buffer wraparound for both reads and writes. It owns the underlying
/// slab for the single-writer design.
///
/// # Thread Safety
///
/// RingBuffer is designed for single-writer, multiple-reader patterns.
/// The underlying slab provides the necessary memory safety guarantees.
#[derive(Debug)]
pub struct RingBuffer {
    /// The underlying memory-mapped slab.
    slab: Slab,
    /// Whether the ring buffer has wrapped around at least once.
    has_wrapped: bool,
}

impl RingBuffer {
    /// Creates a new ring buffer by taking ownership of a slab.
    ///
    /// # Arguments
    ///
    /// * `slab` - The slab to wrap with ring buffer semantics
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use rondo::slab::Slab;
    /// use rondo::ring::RingBuffer;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let slab = Slab::create("data.slab", 0x1234, 3600, 100, 1_000_000_000)?;
    /// let ring = RingBuffer::new(slab);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(slab: Slab) -> Self {
        let write_cursor = slab.write_cursor();

        // Detect if wrapping has occurred by checking if the slot after cursor has data
        let has_wrapped = if write_cursor == 0 {
            // If cursor is at 0, check if slot 1 has data (would indicate we wrapped)
            slab.slot_count() > 1 && slab.read_timestamp(1) != 0
        } else {
            // If cursor > 0, check if the slot after cursor has data
            let next_slot = (write_cursor + 1) % slab.slot_count();
            slab.read_timestamp(next_slot) != 0
        };

        Self { slab, has_wrapped }
    }

    /// Returns the underlying slab.
    pub fn slab(&self) -> &Slab {
        &self.slab
    }

    /// Returns a mutable reference to the underlying slab.
    pub fn slab_mut(&mut self) -> &mut Slab {
        &mut self.slab
    }

    /// Consumes the ring buffer and returns the underlying slab.
    pub fn into_slab(self) -> Slab {
        self.slab
    }

    /// Computes the slot index for a given timestamp.
    ///
    /// # Arguments
    ///
    /// * `timestamp_ns` - Timestamp in nanoseconds
    ///
    /// # Returns
    ///
    /// The slot index within the ring buffer.
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // Result is bounded by slot_count (u32)
    fn compute_slot(&self, timestamp_ns: u64) -> u32 {
        let interval_ns = self.slab.interval_ns();
        let slot_count = self.slab.slot_count();
        ((timestamp_ns / interval_ns) % slot_count as u64) as u32
    }

    /// Writes a value for a specific series at the given timestamp.
    ///
    /// This is the hot path operation and performs zero allocations.
    /// It computes the slot index, writes the timestamp and value,
    /// and advances the write cursor if necessary.
    ///
    /// # Arguments
    ///
    /// * `series_column` - The series column index
    /// * `value` - The f64 value to write
    /// * `timestamp_ns` - Timestamp in nanoseconds
    ///
    /// # Errors
    ///
    /// Returns [`RecordError`] if the timestamp is invalid or causes buffer overflow.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::ring::RingBuffer;
    /// # use rondo::slab::Slab;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let slab = Slab::create("test.slab", 0x1234, 100, 10, 1_000_000_000)?;
    /// let mut ring = RingBuffer::new(slab);
    ///
    /// // Write CPU usage at timestamp 1000000000ns (series column 0)
    /// ring.write(0, 85.5, 1000000000)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn write(&mut self, series_column: u32, value: f64, timestamp_ns: u64) -> Result<()> {
        // Validate inputs
        if value.is_infinite() {
            return Err(RecordError::InvalidValue {
                value,
                reason: "infinite values are not allowed".to_string(),
            }
            .into());
        }

        if timestamp_ns == 0 {
            return Err(RecordError::InvalidTimestamp { timestamp: timestamp_ns }.into());
        }

        let slot_index = self.compute_slot(timestamp_ns);
        let current_cursor = self.slab.write_cursor();

        // Check for wraparound: if new slot is less than cursor, we've wrapped
        if slot_index < current_cursor && !self.has_wrapped {
            self.has_wrapped = true;
        }

        // Write timestamp and value
        self.slab.write_timestamp(slot_index, timestamp_ns);
        self.slab.write_value(slot_index, series_column, value);

        // Update cursor if this is the newest write
        // The cursor should point to the slot with the highest timestamp
        if self.has_wrapped || slot_index >= current_cursor {
            self.slab.set_write_cursor(slot_index);
        }

        Ok(())
    }

    /// Writes multiple series values at the same timestamp in a single operation.
    ///
    /// This is more efficient than multiple individual writes since it only
    /// advances the cursor once and writes the timestamp once.
    ///
    /// # Arguments
    ///
    /// * `entries` - Slice of (series_column, value) pairs
    /// * `timestamp_ns` - Timestamp in nanoseconds for all entries
    ///
    /// # Errors
    ///
    /// Returns [`RecordError`] if any value is invalid or timestamp causes overflow.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::ring::RingBuffer;
    /// # use rondo::slab::Slab;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let slab = Slab::create("test.slab", 0x1234, 100, 10, 1_000_000_000)?;
    /// let mut ring = RingBuffer::new(slab);
    ///
    /// // Write CPU, memory, and disk usage all at once
    /// let entries = &[(0, 85.5), (1, 67.2), (2, 45.8)];
    /// ring.write_batch(entries, 1000000000)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn write_batch(&mut self, entries: &[(u32, f64)], timestamp_ns: u64) -> Result<()> {
        // Validate timestamp
        if timestamp_ns == 0 {
            return Err(RecordError::InvalidTimestamp { timestamp: timestamp_ns }.into());
        }

        // Validate all values first
        for &(_, value) in entries {
            if value.is_infinite() {
                return Err(RecordError::InvalidValue {
                    value,
                    reason: "infinite values are not allowed".to_string(),
                }
                .into());
            }
        }

        let slot_index = self.compute_slot(timestamp_ns);
        let current_cursor = self.slab.write_cursor();

        // Check for wraparound
        if slot_index < current_cursor && !self.has_wrapped {
            self.has_wrapped = true;
        }

        // Write timestamp once
        self.slab.write_timestamp(slot_index, timestamp_ns);

        // Write all values
        for &(series_column, value) in entries {
            self.slab.write_value(slot_index, series_column, value);
        }

        // Update cursor
        if self.has_wrapped || slot_index >= current_cursor {
            self.slab.set_write_cursor(slot_index);
        }

        Ok(())
    }

    /// Reads values for a specific series within the given time range.
    ///
    /// Returns an iterator that yields `(timestamp, value)` pairs in chronological
    /// order (oldest to newest). NaN values are skipped automatically.
    ///
    /// # Arguments
    ///
    /// * `series_column` - The series column index to read
    /// * `start_ns` - Start timestamp in nanoseconds (inclusive)
    /// * `end_ns` - End timestamp in nanoseconds (exclusive)
    ///
    /// # Errors
    ///
    /// Returns [`QueryError`] if the time range is invalid.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use rondo::ring::RingBuffer;
    /// # use rondo::slab::Slab;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let slab = Slab::create("test.slab", 0x1234, 100, 10, 1_000_000_000)?;
    /// let ring = RingBuffer::new(slab);
    ///
    /// // Read CPU usage from 1 second to 10 seconds
    /// for (timestamp, value) in ring.read(0, 1_000_000_000, 10_000_000_000)? {
    ///     println!("CPU at {}: {}%", timestamp, value);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn read(&self, series_column: u32, start_ns: u64, end_ns: u64) -> Result<RingIterator<'_>> {
        if start_ns >= end_ns {
            return Err(QueryError::InvalidTimeRange {
                start: start_ns,
                end: end_ns,
            }
            .into());
        }

        Ok(RingIterator::new(self, series_column, start_ns, end_ns))
    }

    /// Returns the timestamp of the oldest data in the ring buffer.
    ///
    /// This is the data that will be overwritten next if the buffer is full.
    ///
    /// # Returns
    ///
    /// The oldest timestamp, or `None` if the buffer is empty.
    pub fn oldest_timestamp(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }

        if self.has_wrapped {
            // When wrapped, oldest is at the slot after the cursor
            let cursor = self.slab.write_cursor();
            let oldest_slot = (cursor + 1) % self.slab.slot_count();
            let timestamp = self.slab.read_timestamp(oldest_slot);
            if timestamp != 0 {
                Some(timestamp)
            } else {
                None
            }
        } else {
            // When not wrapped, find the first non-zero timestamp
            for slot in 0..self.slab.slot_count() {
                let timestamp = self.slab.read_timestamp(slot);
                if timestamp != 0 {
                    return Some(timestamp);
                }
            }
            None
        }
    }

    /// Returns the timestamp of the newest data in the ring buffer.
    ///
    /// # Returns
    ///
    /// The newest timestamp, or `None` if the buffer is empty.
    pub fn newest_timestamp(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }

        let cursor = self.slab.write_cursor();
        let timestamp = self.slab.read_timestamp(cursor);
        if timestamp != 0 {
            Some(timestamp)
        } else {
            None
        }
    }

    /// Returns whether the ring buffer is empty.
    ///
    /// # Returns
    ///
    /// `true` if no writes have been performed yet.
    pub fn is_empty(&self) -> bool {
        let cursor = self.slab.write_cursor();
        self.slab.read_timestamp(cursor) == 0
    }

    /// Returns whether the ring buffer has wrapped around.
    ///
    /// # Returns
    ///
    /// `true` if the write cursor has gone past slot_count at least once.
    pub fn has_wrapped(&self) -> bool {
        self.has_wrapped
    }

    /// Returns the number of slots that contain valid data.
    ///
    /// # Returns
    ///
    /// The count of slots with non-zero timestamps.
    pub fn slots_used(&self) -> u32 {
        if self.is_empty() {
            return 0;
        }

        if self.has_wrapped {
            // When wrapped, all slots should be used
            self.slab.slot_count()
        } else {
            // When not wrapped, count slots with non-zero timestamps
            let mut count = 0;
            for slot in 0..self.slab.slot_count() {
                if self.slab.read_timestamp(slot) != 0 {
                    count += 1;
                }
            }
            count
        }
    }
}

/// Iterator for reading time-series data from a ring buffer.
///
/// This iterator handles wraparound automatically and returns data in
/// chronological order (oldest to newest). It skips slots with NaN values.
#[derive(Debug)]
pub struct RingIterator<'a> {
    ring: &'a RingBuffer,
    series_column: u32,
    start_ns: u64,
    end_ns: u64,
    current_slot: u32,
    slots_remaining: u32,
}

impl<'a> RingIterator<'a> {
    /// Creates a new ring iterator.
    fn new(ring: &'a RingBuffer, series_column: u32, start_ns: u64, end_ns: u64) -> Self {
        if ring.is_empty() {
            return Self {
                ring,
                series_column,
                start_ns,
                end_ns,
                current_slot: 0,
                slots_remaining: 0,
            };
        }

        let (start_slot, slot_count) = if ring.has_wrapped {
            // When wrapped, we need to start from the oldest slot
            let cursor = ring.slab.write_cursor();
            let oldest_slot = (cursor + 1) % ring.slab.slot_count();
            (oldest_slot, ring.slab.slot_count())
        } else {
            // When not wrapped, start from slot 0
            let cursor = ring.slab.write_cursor();
            (0, cursor + 1)
        };

        Self {
            ring,
            series_column,
            start_ns,
            end_ns,
            current_slot: start_slot,
            slots_remaining: slot_count,
        }
    }
}

impl<'a> Iterator for RingIterator<'a> {
    type Item = (u64, f64);

    fn next(&mut self) -> Option<Self::Item> {
        while self.slots_remaining > 0 {
            let timestamp = self.ring.slab.read_timestamp(self.current_slot);
            let value = self.ring.slab.read_value(self.current_slot, self.series_column);

            // Move to next slot
            self.current_slot = (self.current_slot + 1) % self.ring.slab.slot_count();
            self.slots_remaining -= 1;

            // Check if timestamp is within range and value is valid
            if timestamp >= self.start_ns && timestamp < self.end_ns && !value.is_nan() {
                return Some((timestamp, value));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_ring(slot_count: u32, interval_ns: u64) -> RingBuffer {
        let temp_dir = tempdir().unwrap();
        let slab_path = temp_dir.path().join("test.slab");
        let slab = Slab::create(slab_path, 0x1234567890abcdef, slot_count, 10, interval_ns).unwrap();
        RingBuffer::new(slab)
    }

    #[test]
    fn test_empty_buffer() {
        let ring = create_test_ring(10, 1_000_000_000);

        assert!(ring.is_empty());
        assert!(!ring.has_wrapped());
        assert_eq!(ring.slots_used(), 0);
        assert_eq!(ring.oldest_timestamp(), None);
        assert_eq!(ring.newest_timestamp(), None);
    }

    #[test]
    fn test_single_write() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        ring.write(0, 42.5, 1_000_000_000).unwrap();

        assert!(!ring.is_empty());
        assert!(!ring.has_wrapped());
        assert_eq!(ring.slots_used(), 1);
        assert_eq!(ring.oldest_timestamp(), Some(1_000_000_000));
        assert_eq!(ring.newest_timestamp(), Some(1_000_000_000));
        assert_eq!(ring.slab.write_cursor(), 1); // slot 1 for timestamp 1s at 1s interval
    }

    #[test]
    fn test_slot_computation() {
        let ring = create_test_ring(10, 1_000_000_000);

        // 1 second interval, so timestamp 0 -> slot 0, timestamp 1s -> slot 1, etc.
        assert_eq!(ring.compute_slot(0), 0);
        assert_eq!(ring.compute_slot(1_000_000_000), 1);
        assert_eq!(ring.compute_slot(2_000_000_000), 2);
        assert_eq!(ring.compute_slot(9_000_000_000), 9);
        assert_eq!(ring.compute_slot(10_000_000_000), 0); // wraps around
        assert_eq!(ring.compute_slot(11_000_000_000), 1);
    }

    #[test]
    fn test_multiple_writes() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        // Write some data points
        ring.write(0, 10.0, 1_000_000_000).unwrap();
        ring.write(0, 20.0, 2_000_000_000).unwrap();
        ring.write(0, 30.0, 3_000_000_000).unwrap();

        assert!(!ring.is_empty());
        assert!(!ring.has_wrapped());
        assert_eq!(ring.slots_used(), 3); // slots 1, 2, 3 (cursor at 3)
        assert_eq!(ring.oldest_timestamp(), Some(1_000_000_000));
        assert_eq!(ring.newest_timestamp(), Some(3_000_000_000));
    }

    #[test]
    fn test_batch_write() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        let entries = &[(0, 10.0), (1, 20.0), (2, 30.0)];
        ring.write_batch(entries, 1_000_000_000).unwrap();

        // Check that all series were written at the same slot
        let slot = ring.compute_slot(1_000_000_000);
        assert_eq!(ring.slab.read_timestamp(slot), 1_000_000_000);
        assert_eq!(ring.slab.read_value(slot, 0), 10.0);
        assert_eq!(ring.slab.read_value(slot, 1), 20.0);
        assert_eq!(ring.slab.read_value(slot, 2), 30.0);
    }

    #[test]
    fn test_wraparound() {
        let mut ring = create_test_ring(3, 1_000_000_000);

        // Fill the buffer
        ring.write(0, 10.0, 1_000_000_000).unwrap(); // slot 1
        ring.write(0, 20.0, 2_000_000_000).unwrap(); // slot 2
        ring.write(0, 30.0, 3_000_000_000).unwrap(); // slot 0 (wraps)

        assert!(ring.has_wrapped());
        assert_eq!(ring.slots_used(), 3);

        // Write one more to overwrite the oldest
        ring.write(0, 40.0, 4_000_000_000).unwrap(); // slot 1 (overwrites first write)

        assert_eq!(ring.oldest_timestamp(), Some(2_000_000_000));
        assert_eq!(ring.newest_timestamp(), Some(4_000_000_000));
    }

    #[test]
    fn test_read_empty() {
        let ring = create_test_ring(10, 1_000_000_000);

        let mut iter = ring.read(0, 0, 10_000_000_000).unwrap();
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_read_basic() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        ring.write(0, 10.0, 1_000_000_000).unwrap();
        ring.write(0, 20.0, 3_000_000_000).unwrap();
        ring.write(0, 30.0, 5_000_000_000).unwrap();

        let data: Vec<_> = ring.read(0, 0, 10_000_000_000).unwrap().collect();
        assert_eq!(data, vec![
            (1_000_000_000, 10.0),
            (3_000_000_000, 20.0),
            (5_000_000_000, 30.0),
        ]);
    }

    #[test]
    fn test_read_time_range() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        ring.write(0, 10.0, 1_000_000_000).unwrap();
        ring.write(0, 20.0, 3_000_000_000).unwrap();
        ring.write(0, 30.0, 5_000_000_000).unwrap();
        ring.write(0, 40.0, 7_000_000_000).unwrap();

        // Read from 2s to 6s (should get entries at 3s and 5s)
        let data: Vec<_> = ring.read(0, 2_000_000_000, 6_000_000_000).unwrap().collect();
        assert_eq!(data, vec![
            (3_000_000_000, 20.0),
            (5_000_000_000, 30.0),
        ]);
    }

    #[test]
    fn test_read_with_wraparound() {
        let mut ring = create_test_ring(3, 1_000_000_000);

        // Fill the buffer and cause wraparound
        ring.write(0, 10.0, 1_000_000_000).unwrap(); // slot 1
        ring.write(0, 20.0, 2_000_000_000).unwrap(); // slot 2
        ring.write(0, 30.0, 3_000_000_000).unwrap(); // slot 0
        ring.write(0, 40.0, 4_000_000_000).unwrap(); // slot 1 (overwrites first)

        // Read all data - should be in chronological order
        let data: Vec<_> = ring.read(0, 0, 10_000_000_000).unwrap().collect();
        assert_eq!(data, vec![
            (2_000_000_000, 20.0),
            (3_000_000_000, 30.0),
            (4_000_000_000, 40.0),
        ]);
    }

    #[test]
    fn test_read_skips_nan() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        ring.write(0, 10.0, 1_000_000_000).unwrap();
        ring.write(1, 20.0, 3_000_000_000).unwrap(); // different series
        ring.write(0, 30.0, 5_000_000_000).unwrap();

        // Read series 0 - should skip the slot with only series 1 data
        let data: Vec<_> = ring.read(0, 0, 10_000_000_000).unwrap().collect();
        assert_eq!(data, vec![
            (1_000_000_000, 10.0),
            (5_000_000_000, 30.0),
        ]);
    }

    #[test]
    fn test_invalid_value_errors() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        // Infinite values should be rejected
        assert!(ring.write(0, f64::INFINITY, 1_000_000_000).is_err());
        assert!(ring.write(0, f64::NEG_INFINITY, 1_000_000_000).is_err());

        // NaN is allowed for writes (it's the sentinel value)
        assert!(ring.write(0, f64::NAN, 1_000_000_000).is_ok());
    }

    #[test]
    fn test_invalid_timestamp_errors() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        // Timestamp 0 should be rejected
        assert!(ring.write(0, 10.0, 0).is_err());
    }

    #[test]
    fn test_invalid_time_range_error() {
        let ring = create_test_ring(10, 1_000_000_000);

        // start >= end should error
        assert!(ring.read(0, 5_000_000_000, 5_000_000_000).is_err());
        assert!(ring.read(0, 10_000_000_000, 5_000_000_000).is_err());
    }

    #[test]
    fn test_multiple_series() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        ring.write(0, 10.0, 1_000_000_000).unwrap();
        ring.write(1, 20.0, 1_000_000_000).unwrap();
        ring.write(2, 30.0, 1_000_000_000).unwrap();

        // Each series should have its own data at the same timestamp
        let data0: Vec<_> = ring.read(0, 0, 10_000_000_000).unwrap().collect();
        let data1: Vec<_> = ring.read(1, 0, 10_000_000_000).unwrap().collect();
        let data2: Vec<_> = ring.read(2, 0, 10_000_000_000).unwrap().collect();

        assert_eq!(data0, vec![(1_000_000_000, 10.0)]);
        assert_eq!(data1, vec![(1_000_000_000, 20.0)]);
        assert_eq!(data2, vec![(1_000_000_000, 30.0)]);
    }

    #[test]
    fn test_batch_error_on_invalid_values() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        let entries = &[(0, 10.0), (1, f64::INFINITY), (2, 30.0)];
        assert!(ring.write_batch(entries, 1_000_000_000).is_err());

        // Verify no data was written
        assert!(ring.is_empty());
    }

    #[test]
    fn test_batch_error_on_invalid_timestamp() {
        let mut ring = create_test_ring(10, 1_000_000_000);

        let entries = &[(0, 10.0), (1, 20.0)];
        assert!(ring.write_batch(entries, 0).is_err());

        // Verify no data was written
        assert!(ring.is_empty());
    }

    #[test]
    fn test_state_persistence() {
        let temp_dir = tempdir().unwrap();
        let slab_path = temp_dir.path().join("persist.slab");

        // Create ring buffer and write some data
        {
            let slab = Slab::create(&slab_path, 0x1234, 10, 5, 1_000_000_000).unwrap();
            let mut ring = RingBuffer::new(slab);

            ring.write(0, 10.0, 1_000_000_000).unwrap();
            ring.write(0, 20.0, 3_000_000_000).unwrap();
        }

        // Reopen and verify data
        {
            let slab = Slab::open(&slab_path).unwrap();
            let ring = RingBuffer::new(slab);

            let data: Vec<_> = ring.read(0, 0, 10_000_000_000).unwrap().collect();
            assert_eq!(data, vec![
                (1_000_000_000, 10.0),
                (3_000_000_000, 20.0),
            ]);

            assert_eq!(ring.oldest_timestamp(), Some(1_000_000_000));
            assert_eq!(ring.newest_timestamp(), Some(3_000_000_000));
        }
    }

    #[test]
    fn test_wraparound_state_detection() {
        let temp_dir = tempdir().unwrap();
        let slab_path = temp_dir.path().join("wrap.slab");

        // Create ring buffer and cause wraparound
        {
            let slab = Slab::create(&slab_path, 0x1234, 3, 5, 1_000_000_000).unwrap();
            let mut ring = RingBuffer::new(slab);

            ring.write(0, 10.0, 1_000_000_000).unwrap(); // slot 1
            ring.write(0, 20.0, 2_000_000_000).unwrap(); // slot 2
            ring.write(0, 30.0, 3_000_000_000).unwrap(); // slot 0
            ring.write(0, 40.0, 4_000_000_000).unwrap(); // slot 1

            assert!(ring.has_wrapped());
        }

        // Reopen and verify wrap detection works
        {
            let slab = Slab::open(&slab_path).unwrap();
            let ring = RingBuffer::new(slab);

            assert!(ring.has_wrapped());
            assert_eq!(ring.oldest_timestamp(), Some(2_000_000_000));
            assert_eq!(ring.newest_timestamp(), Some(4_000_000_000));
        }
    }
}