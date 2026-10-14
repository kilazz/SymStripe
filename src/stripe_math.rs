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

/// Configuration parameters for RAID-0 block extent striping with asymmetric weighted distribution.
#[derive(Clone, Debug)]
pub struct StripeConfig {
    /// Size of each stripe block in bytes (e.g., 16 MB, 64 MB).
    pub stripe_size_bytes: u64,
    /// Proportional speed weights for each drive (e.g., [2, 1] for 200 MB/s + 100 MB/s).
    pub drive_weights: Vec<usize>,
    /// Pre-computed interleaved schedule of drive indices based on weights.
    schedule: Vec<usize>,
}

impl StripeConfig {
    /// Creates a uniform (symmetric 1:1) stripe configuration.
    pub fn new(stripe_size_bytes: u64, num_drives: usize) -> Self {
        Self::new_weighted(stripe_size_bytes, vec![1; num_drives])
    }

    /// Creates an asymmetric weighted stripe configuration.
    ///
    /// For example, weights `[2, 1]` creates schedule `[0, 0, 1]`, ensuring the drive that is
    /// twice as fast processes 2 blocks for every 1 block processed by the slower drive.
    pub fn new_weighted(stripe_size_bytes: u64, weights: Vec<usize>) -> Self {
        assert!(
            !weights.is_empty(),
            "Storage pool must contain at least one drive"
        );
        assert!(
            stripe_size_bytes > 0,
            "Stripe size must be greater than zero"
        );

        let mut schedule = Vec::new();

        for (drive_idx, &weight) in weights.iter().enumerate() {
            for _ in 0..weight.max(1) {
                schedule.push(drive_idx);
            }
        }

        if schedule.is_empty() {
            schedule = (0..weights.len()).collect();
        }

        Self {
            stripe_size_bytes,
            drive_weights: weights,
            schedule,
        }
    }

    /// Centralized deterministic mapping of a file part to its physical target drive,
    /// taking into account drive weights and the initial file hash shift.
    #[inline]
    pub fn map_part_to_drive(&self, file_hash: u64, part_index: u64) -> usize {
        if self.schedule.is_empty() {
            return 0;
        }
        let sched_len = self.schedule.len() as u64;
        let drive_shift = file_hash % sched_len;
        let slot = (part_index + drive_shift) % sched_len;
        self.schedule[slot as usize]
    }

    /// Decomposes an arbitrary linear file I/O request (offset + length)
    /// into discrete sub-requests aligned across the storage array drives.
    pub fn map_io_slices(
        &self,
        file_hash: u64,
        global_offset: u64,
        len: usize,
    ) -> Vec<StripeSlice> {
        let mut slices = Vec::new();
        let mut curr_offset = global_offset;
        let mut remaining = len;

        while remaining > 0 {
            let stripe_idx = curr_offset / self.stripe_size_bytes;
            let target_drive_idx = self.map_part_to_drive(file_hash, stripe_idx);
            let offset_in_stripe = curr_offset % self.stripe_size_bytes;

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
    fn test_uniform_striping() {
        let config = StripeConfig::new(4 * 1024 * 1024, 2);
        let slices = config.map_io_slices(0, 0, 8 * 1024 * 1024);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].target_drive_idx, 0);
        assert_eq!(slices[1].target_drive_idx, 1);
        assert_eq!(config.drive_weights.len(), 2);
    }

    #[test]
    fn test_asymmetric_weighted_striping_2_to_1() {
        // Drive 0 is 2x faster than Drive 1 -> Pattern: [0, 0, 1]
        let config = StripeConfig::new_weighted(4 * 1024 * 1024, vec![2, 1]);
        assert_eq!(config.schedule, vec![0, 0, 1]);
        assert_eq!(config.drive_weights.len(), 2);

        assert_eq!(config.map_part_to_drive(0, 0), 0); // Part 0 -> Drive 0
        assert_eq!(config.map_part_to_drive(0, 1), 0); // Part 1 -> Drive 0
        assert_eq!(config.map_part_to_drive(0, 2), 1); // Part 2 -> Drive 1
        assert_eq!(config.map_part_to_drive(0, 3), 0); // Part 3 -> Drive 0
        assert_eq!(config.map_part_to_drive(0, 4), 0); // Part 4 -> Drive 0
        assert_eq!(config.map_part_to_drive(0, 5), 1); // Part 5 -> Drive 1
    }
}
