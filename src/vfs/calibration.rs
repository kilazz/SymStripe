use crate::aligned_buf::AlignedBuffer;
use crate::win32::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub type VfsLogCallback = Arc<dyn Fn(String) + Send + Sync>;

pub fn auto_calibrate_drive_weights(
    members: &[PathBuf],
    logger: &Option<VfsLogCallback>,
) -> Vec<usize> {
    if members.len() <= 1 {
        return vec![1; members.len()];
    }

    let mut measured_speeds = Vec::new();
    let probe_bytes = 8 * 1024 * 1024;
    let mut write_buf = AlignedBuffer::new(probe_bytes, 4096);
    write_buf.as_slice_mut().fill(0x33);
    let mut read_buf = AlignedBuffer::new(probe_bytes, 4096);

    for (idx, member) in members.iter().enumerate() {
        let probe_file = member.join(format!(".symstripe_calib_{}.tmp", idx));

        let speed_mbs = (|| -> Option<f64> {
            let mut wf = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(FILE_FLAG_WRITE_THROUGH)
                .open(&probe_file)
                .ok()?;
            wf.write_all(write_buf.as_slice_mut()).ok()?;
            wf.sync_all().ok()?;
            drop(wf);

            let t0 = Instant::now();
            let mut f = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_NO_BUFFERING)
                .open(&probe_file)
                .or_else(|_| File::open(&probe_file))
                .ok()?;

            f.read_exact(read_buf.as_slice_mut()).ok()?;
            let elapsed = t0.elapsed().as_secs_f64();
            let _ = fs::remove_file(&probe_file);
            Some((probe_bytes as f64 / 1_048_576.0) / elapsed.max(0.0001))
        })()
        .unwrap_or(100.0);

        measured_speeds.push(speed_mbs);
    }

    let min_speed = measured_speeds
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        .max(1.0);

    let weights: Vec<usize> = measured_speeds
        .iter()
        .map(|&spd| ((spd / min_speed).round() as usize).clamp(1, 4))
        .collect();

    if let Some(cb) = logger {
        let details: Vec<String> = members
            .iter()
            .enumerate()
            .map(|(i, _)| {
                format!(
                    "Drive {}: {}x ({:.0} MB/s)",
                    i + 1,
                    weights[i],
                    measured_speeds[i]
                )
            })
            .collect();
        cb(format!(
            "⚖️ [AUTO-CALIBRATION] RAID-0 Asymmetric Weights: {} -> Schedule pattern calibrated.",
            details.join(", ")
        ));
    }

    weights
}
