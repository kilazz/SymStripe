#![allow(dead_code)]

/// Describes an I/O slice mapped to a specific physical drive in the storage array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeSlice {
    /// Zero-based index of the target drive in the array.
    pub target_drive_idx: usize,
    /// Sequential index of the stripe block for this file.
    pub part_index: u64,
    /// Byte offset within the stripe block on the destination drive.
    pub offset_in_part: u64,
    /// Number of bytes to transfer for this slice.
    pub length: usize,
}

/// Configuration parameters for RAID-0 block extent striping.
#[derive(Clone, Debug)]
pub struct StripeConfig {
    /// Size of each stripe block in bytes (e.g., 4 MB, 16 MB, 64 MB).
    pub stripe_size_bytes: u64,
    /// Number of physical storage drives in the pool.
    pub num_drives: usize,
}

impl StripeConfig {
    /// Creates a new stripe configuration.
    ///
    /// # Panics
    /// Panics if `num_drives == 0` or `stripe_size_bytes == 0`.
    pub fn new(stripe_size_bytes: u64, num_drives: usize) -> Self {
        assert!(
            num_drives > 0,
            "Storage pool must contain at least one drive"
        );
        assert!(
            stripe_size_bytes > 0,
            "Stripe size must be greater than zero"
        );
        Self {
            stripe_size_bytes,
            num_drives,
        }
    }

    /// Decomposes an arbitrary linear file I/O request (offset + length)
    /// into discrete sub-requests aligned across the storage array drives.
    ///
    /// If a request spans across stripe boundaries or drive transitions,
    /// it is automatically split into consecutive per-drive slices.
    pub fn map_io_slices(&self, global_offset: u64, len: usize) -> Vec<StripeSlice> {
        let mut slices = Vec::new();
        let mut curr_offset = global_offset;
        let mut remaining = len;

        while remaining > 0 {
            // Global sequential stripe index across the file
            let stripe_idx = curr_offset / self.stripe_size_bytes;
            // Target drive determined by round-robin distribution
            let target_drive_idx = (stripe_idx as usize) % self.num_drives;
            // Offset within the current stripe block
            let offset_in_stripe = curr_offset % self.stripe_size_bytes;

            // Maximum bytes remaining until the next stripe boundary
            let bytes_in_this_stripe =
                ((self.stripe_size_bytes - offset_in_stripe) as usize).min(remaining);

            slices.push(StripeSlice {
                target_drive_idx,
                part_index: stripe_idx,
                offset_in_part: offset_in_stripe,
                length: bytes_in_this_stripe,
            });

            curr_offset += bytes_in_this_stripe as u64;
            remaining -= bytes_in_this_stripe;
        }

        slices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_drive_slice() {
        let config = StripeConfig::new(4 * 1024 * 1024, 2); // 4 MB stripe, 2 drives
        let slices = config.map_io_slices(0, 1024 * 1024); // 1 MB read at offset 0
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].target_drive_idx, 0);
        assert_eq!(slices[0].part_index, 0);
        assert_eq!(slices[0].offset_in_part, 0);
        assert_eq!(slices[0].length, 1024 * 1024);
    }

    #[test]
    fn test_cross_boundary_stripe_split() {
        let config = StripeConfig::new(4 * 1024 * 1024, 2); // 4 MB stripe, 2 drives
        // 2 MB read starting at offset 3 MB (crosses 4 MB boundary into Drive 1)
        let slices = config.map_io_slices(3 * 1024 * 1024, 2 * 1024 * 1024);
        assert_eq!(slices.len(), 2);

        // First slice: 1 MB on Drive 0
        assert_eq!(slices[0].target_drive_idx, 0);
        assert_eq!(slices[0].part_index, 0);
        assert_eq!(slices[0].offset_in_part, 3 * 1024 * 1024);
        assert_eq!(slices[0].length, 1024 * 1024);

        // Second slice: 1 MB on Drive 1
        assert_eq!(slices[1].target_drive_idx, 1);
        assert_eq!(slices[1].part_index, 1);
        assert_eq!(slices[1].offset_in_part, 0);
        assert_eq!(slices[1].length, 1024 * 1024);
    }

    #[test]
    fn test_multi_drive_wraparound() {
        let config = StripeConfig::new(1024, 3); // 1 KB stripe, 3 drives
        // 4 KB read starting at offset 512 (spans across 4 stripes over 3 drives)
        let slices = config.map_io_slices(512, 4096);
        assert_eq!(slices.len(), 5);
        assert_eq!(slices[0].target_drive_idx, 0); // 512..1024 (512 bytes)
        assert_eq!(slices[1].target_drive_idx, 1); // 1024..2048 (1024 bytes)
        assert_eq!(slices[2].target_drive_idx, 2); // 2048..3072 (1024 bytes)
        assert_eq!(slices[3].target_drive_idx, 0); // 3072..4096 (1024 bytes - wrapped around)
        assert_eq!(slices[4].target_drive_idx, 1); // 4096..4608 (512 bytes)
    }
}
