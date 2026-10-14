use super::calibration::{VfsLogCallback, auto_calibrate_drive_weights};
use super::fs::SymStripePoolFs;
use crate::win32::check_winfsp_available;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use winfsp::host::{FileSystemHost, VolumeParams};

pub struct VfsPoolSession {
    _host: FileSystemHost<SymStripePoolFs>,
    pub io_bytes: Arc<Vec<AtomicU64>>,
    pub member_paths: Vec<PathBuf>,
    pub io_max_latency_ms: Arc<Vec<AtomicU64>>,
    pub cache_hits: Arc<AtomicU64>,
    pub cache_misses: Arc<AtomicU64>,
    pub raw_bytes: Arc<AtomicU64>,
    pub compressed_bytes: Arc<AtomicU64>,
    stop_flusher: Arc<AtomicBool>,
    flusher_handle: Option<std::thread::JoinHandle<()>>,
    fs_ref: SymStripePoolFs,
}

impl VfsPoolSession {
    pub fn sample_and_reset_io(&self) -> (Vec<u64>, Vec<u64>) {
        let bytes = self
            .io_bytes
            .iter()
            .map(|c| c.swap(0, Ordering::Relaxed))
            .collect();
        let lats = self
            .io_max_latency_ms
            .iter()
            .map(|c| c.swap(0, Ordering::Relaxed))
            .collect();
        (bytes, lats)
    }
}

impl Drop for VfsPoolSession {
    fn drop(&mut self) {
        self.stop_flusher.store(true, Ordering::Relaxed);
        if let Some(h) = self.flusher_handle.take() {
            let _ = h.join();
        }
        self.fs_ref.flush_all_dirty_nodes();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn mount_storage_pool(
    mount_target: &str,
    members: &[PathBuf],
    policy: i32,
    stripe_size_bytes: u64,
    custom_size_gb: u64,
    codec_idx: i32,
    tiered_storage: bool,
    ram_cache_mb: u32,
    logger: Option<VfsLogCallback>,
) -> Result<VfsPoolSession, String> {
    if members.is_empty() {
        return Err("Cannot mount pool: no storage members specified.".into());
    }

    check_winfsp_available()?;
    let _init = winfsp::winfsp_init().map_err(|e| format!("WinFsp init failed: {:?}", e))?;

    let drive_weights = if policy == 1 && members.len() > 1 {
        Some(auto_calibrate_drive_weights(members, &logger))
    } else {
        None
    };

    let pool_fs = SymStripePoolFs::new(
        members.to_vec(),
        policy,
        stripe_size_bytes,
        custom_size_gb,
        codec_idx,
        tiered_storage,
        ram_cache_mb,
        drive_weights,
        logger,
    );

    let io_bytes = pool_fs.io_bytes.clone();
    let io_max_latency_ms = pool_fs.io_max_latency_ms.clone();
    let cache_hits = pool_fs.cache_hits.clone();
    let cache_misses = pool_fs.cache_misses.clone();
    let raw_bytes = pool_fs.raw_bytes_written.clone();
    let compressed_bytes = pool_fs.compressed_bytes_written.clone();

    let mut vp = VolumeParams::new();
    vp.filesystem_name("NTFS");
    vp.volume_serial_number(0x594D5354);
    vp.case_preserved_names(true);
    vp.case_sensitive_search(false);
    vp.unicode_on_disk(true);
    vp.flush_and_purge_on_cleanup(true);

    vp.file_info_timeout(1000);
    vp.dir_info_timeout(1000);
    vp.volume_info_timeout(1000);
    vp.post_cleanup_when_modified_only(true);

    let mut host =
        FileSystemHost::new(vp, pool_fs.clone()).map_err(|e| format!("Host error: {:?}", e))?;
    host.mount(mount_target)
        .map_err(|e| format!("Mount error '{}': {:?}", mount_target, e))?;
    FileSystemHost::<SymStripePoolFs>::start(&mut host)
        .map_err(|e| format!("Dispatcher error: {:?}", e))?;

    let stop_flusher = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_flusher.clone();
    let fs_bg = pool_fs.clone();

    let flusher_handle = std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(2500));
            if stop_clone.load(Ordering::Relaxed) {
                break;
            }
            fs_bg.flush_all_dirty_nodes();
        }
    });

    Ok(VfsPoolSession {
        _host: host,
        io_bytes,
        member_paths: members.to_vec(),
        io_max_latency_ms,
        cache_hits,
        cache_misses,
        raw_bytes,
        compressed_bytes,
        stop_flusher,
        flusher_handle: Some(flusher_handle),
        fs_ref: pool_fs,
    })
}
