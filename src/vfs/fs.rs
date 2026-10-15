use super::cache::{CacheInner, CachedPart, GlobalStripeCache};
use super::calibration::VfsLogCallback;
use super::iocp::IocpDispatcher;
use super::node::{FileNode, PartKey, PoolFileHandle, VfsCodec, hash_path_id};
use crate::aligned_buf::AlignedBuffer;
use crate::stripe_math::StripeConfig;
use crate::win32::{FILE_FLAG_NO_BUFFERING, get_default_security_descriptor, get_system_ram_mb};
use crate::zstd_atg::ZstdAtgCompressor;
use parking_lot::{Condvar, Mutex, RwLock};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use widestring::U16CStr;
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

pub const MIN_HDD_OPTIMAL_STRIPE_BYTES: u64 = 64 * 1024 * 1024;
pub const DIRECT_PAGE_SIZE: u64 = 2 * 1024 * 1024; // 2 MB Virtual Page Extent for Mirror/Direct files
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC0000023_u32 as i32;

type CachedPartTuple = (u64, Arc<RwLock<CachedPart>>);

/// Hardware NCQ Throttler: allows up to 4 concurrent in-flight requests per spindle
/// allowing the drive firmware elevator algorithm to optimize seek head sweeps without thrashing.
pub struct SpindleThrottle {
    active: AtomicUsize,
    cv: Condvar,
    lock: Mutex<()>,
    max_concurrent: usize,
}

impl SpindleThrottle {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            cv: Condvar::new(),
            lock: Mutex::new(()),
            max_concurrent,
        }
    }

    pub fn acquire(&self) -> SpindleGuard<'_> {
        let mut g = self.lock.lock();
        while self.active.load(Ordering::Acquire) >= self.max_concurrent {
            self.cv.wait(&mut g);
        }
        self.active.fetch_add(1, Ordering::Release);
        SpindleGuard { throttle: self }
    }
}

pub struct SpindleGuard<'a> {
    throttle: &'a SpindleThrottle,
}

impl<'a> Drop for SpindleGuard<'a> {
    fn drop(&mut self) {
        self.throttle.active.fetch_sub(1, Ordering::Release);
        let _g = self.throttle.lock.lock();
        self.throttle.cv.notify_one();
    }
}

struct LoadingLeaderGuard {
    table: Arc<Mutex<HashMap<PartKey, Arc<Condvar>>>>,
    key: PartKey,
}

impl Drop for LoadingLeaderGuard {
    fn drop(&mut self) {
        let mut inflight = self.table.lock();
        if let Some(cv) = inflight.remove(&self.key) {
            cv.notify_all();
        }
    }
}

#[derive(Clone)]
pub struct SymStripePoolFs {
    pub members: Arc<Vec<PathBuf>>,
    pub policy: i32,
    pub stripe_config: StripeConfig,
    pub custom_size_gb: u64,
    pub pool_codec: VfsCodec,
    pub tiered_storage: bool,
    pub logger: Option<VfsLogCallback>,
    pub jbod_map: Arc<RwLock<HashMap<String, usize>>>,
    pub file_nodes: Arc<RwLock<HashMap<String, Arc<FileNode>>>>,
    pub global_cache: Arc<GlobalStripeCache>,
    pub prefetch_in_flight: Arc<Mutex<HashSet<PartKey>>>,
    pub in_flight_loads: Arc<Mutex<HashMap<PartKey, Arc<Condvar>>>>,
    pub meta_sizes: Arc<RwLock<HashMap<String, u64>>>,
    pub pool_used_bytes: Arc<AtomicU64>,
    pub spindle_throttles: Arc<Vec<SpindleThrottle>>,
    pub rr_counter: Arc<AtomicUsize>,
    pub iocp: Arc<IocpDispatcher>,

    pub io_bytes: Arc<Vec<AtomicU64>>,
    pub io_max_latency_ms: Arc<Vec<AtomicU64>>,
    pub cache_hits: Arc<AtomicU64>,
    pub cache_misses: Arc<AtomicU64>,
    pub raw_bytes_written: Arc<AtomicU64>,
    pub compressed_bytes_written: Arc<AtomicU64>,
}

impl SymStripePoolFs {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        members: Vec<PathBuf>,
        policy: i32,
        stripe_size_bytes: u64,
        custom_size_gb: u64,
        codec_idx: i32,
        tiered_storage: bool,
        ram_cache_mb: u32,
        drive_weights: Option<Vec<usize>>,
        logger: Option<VfsLogCallback>,
    ) -> Self {
        let num_drives = members.len().max(1);
        let io_bytes = Arc::new(
            (0..num_drives)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>(),
        );
        let io_max_latency_ms = Arc::new(
            (0..num_drives)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>(),
        );

        // Limit concurrency to 4 per spindle to enable hardware NCQ optimizations
        let spindle_throttles = Arc::new(
            (0..num_drives)
                .map(|_| SpindleThrottle::new(4))
                .collect::<Vec<_>>(),
        );

        let effective_stripe = stripe_size_bytes.max(MIN_HDD_OPTIMAL_STRIPE_BYTES);

        let ram_mb = get_system_ram_mb();
        let cache_budget_bytes = if ram_cache_mb > 0 {
            (ram_cache_mb as usize) * 1024 * 1024
        } else if ram_mb >= 32768 {
            4096 * 1024 * 1024
        } else if ram_mb >= 16384 {
            2048 * 1024 * 1024
        } else {
            1024 * 1024 * 1024
        };

        let stripe_config = if let Some(weights) = drive_weights {
            StripeConfig::new_weighted(effective_stripe, weights)
        } else {
            StripeConfig::new(effective_stripe, num_drives)
        };

        let mut valid_meta_keys = HashSet::new();
        let mut preloaded_meta = HashMap::new();

        if let Some(master) = members.first() {
            for entry in walkdir::WalkDir::new(master)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    let name = entry.file_name().to_string_lossy();
                    if name.ends_with(".symstripe_meta")
                        && let Ok(s) = fs::read_to_string(entry.path())
                        && let Ok(size) = s.trim().parse::<u64>()
                        && let Ok(rel) = entry.path().strip_prefix(master)
                    {
                        let rel_str = rel.to_string_lossy();
                        let clean = rel_str
                            .strip_suffix(".symstripe_meta")
                            .unwrap_or(&rel_str)
                            .to_lowercase();
                        valid_meta_keys.insert(clean.clone());
                        preloaded_meta.insert(clean, size);
                    }
                }
            }
        }

        for m in &members {
            for entry in walkdir::WalkDir::new(m).into_iter().filter_map(|e| e.ok()) {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if let Some(part_pos) = name.find(".part") {
                        let base_name = &name[..part_pos];
                        let rel = entry
                            .path()
                            .strip_prefix(m.join(".symstripe_store"))
                            .or_else(|_| entry.path().strip_prefix(m))
                            .unwrap_or(entry.path());
                        let parent = rel.parent().unwrap_or(Path::new(""));
                        let full_rel_key = parent.join(base_name).to_string_lossy().to_lowercase();

                        if !valid_meta_keys.contains(&full_rel_key) {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }

        let mut initial_used: u64 = 0;
        for m in &members {
            for entry in walkdir::WalkDir::new(m).into_iter().filter_map(|e| e.ok()) {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    let name = entry.file_name().to_string_lossy();
                    if !name.starts_with(".symstripe_jbod") {
                        initial_used += meta.len();
                    }
                }
            }
        }

        let iocp = IocpDispatcher::new();
        iocp.start_workers(4.min(num_drives * 2));

        let fs = Self {
            members: Arc::new(members),
            policy,
            stripe_config,
            custom_size_gb,
            pool_codec: VfsCodec::from_i32(codec_idx),
            tiered_storage,
            logger,
            jbod_map: Arc::new(RwLock::new(HashMap::new())),
            file_nodes: Arc::new(RwLock::new(HashMap::new())),
            global_cache: Arc::new(GlobalStripeCache {
                inner: Mutex::new(CacheInner {
                    entries: HashMap::with_capacity(cache_budget_bytes / (2 * 1024 * 1024)),
                    clock_ring: Vec::with_capacity(cache_budget_bytes / (2 * 1024 * 1024)),
                    clock_hand: 0,
                    current_bytes: 0,
                }),
                max_bytes: cache_budget_bytes,
            }),
            prefetch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            in_flight_loads: Arc::new(Mutex::new(HashMap::new())),
            meta_sizes: Arc::new(RwLock::new(preloaded_meta)),
            pool_used_bytes: Arc::new(AtomicU64::new(initial_used)),
            spindle_throttles,
            rr_counter: Arc::new(AtomicUsize::new(0)),
            iocp,
            io_bytes,
            io_max_latency_ms,
            cache_hits: Arc::new(AtomicU64::new(0)),
            cache_misses: Arc::new(AtomicU64::new(0)),
            raw_bytes_written: Arc::new(AtomicU64::new(0)),
            compressed_bytes_written: Arc::new(AtomicU64::new(0)),
        };

        *fs.jbod_map.write() = fs.load_jbod_catalog();
        fs
    }

    #[inline]
    fn add_used_bytes(&self, val: u64) {
        self.pool_used_bytes.fetch_add(val, Ordering::Relaxed);
    }

    #[inline]
    fn sub_used_bytes(&self, val: u64) {
        let _ = self
            .pool_used_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| {
                Some(x.saturating_sub(val))
            });
    }

    fn log(&self, msg: String) {
        if let Some(cb) = &self.logger {
            cb(msg);
        }
    }

    fn normalize_path(raw_path: &U16CStr) -> String {
        raw_path
            .to_string_lossy()
            .trim_start_matches('\\')
            .to_string()
    }

    fn master(&self) -> &Path {
        &self.members[0]
    }

    fn meta_path(&self, rel_path: &str) -> PathBuf {
        self.master().join(format!("{}.symstripe_meta", rel_path))
    }

    pub fn is_pinned_metadata(rel_path: &str) -> bool {
        let p = rel_path.to_lowercase();
        p.ends_with(".utoc")
            || p.ends_with(".sig")
            || p.ends_with(".toc")
            || p.ends_with(".idx")
            || p.ends_with(".symstripe_meta")
    }

    fn read_meta_size(&self, rel_path: &str) -> Option<u64> {
        let key = rel_path.to_lowercase();
        if let Some(&size) = self.meta_sizes.read().get(&key) {
            return Some(size);
        }
        let p = self.meta_path(rel_path);
        if let Ok(s) = fs::read_to_string(p)
            && let Ok(size) = s.trim().parse::<u64>()
        {
            self.meta_sizes.write().insert(key, size);
            return Some(size);
        }
        None
    }

    fn save_meta_size(&self, rel_path: &str, size: u64) {
        self.meta_sizes
            .write()
            .insert(rel_path.to_lowercase(), size);
        let p = self.meta_path(rel_path);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(p, size.to_string());
    }

    fn catalog_path(&self) -> PathBuf {
        self.master().join(".symstripe_jbod_catalog.json")
    }

    fn load_jbod_catalog(&self) -> HashMap<String, usize> {
        fs::read_to_string(self.catalog_path())
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default()
    }

    fn save_jbod_catalog(&self) {
        let tmp = self.master().join(".symstripe_jbod_catalog.json.tmp");
        if let Ok(json) = serde_json::to_string(&*self.jbod_map.read())
            && fs::write(&tmp, json).is_ok()
        {
            let _ = fs::rename(&tmp, self.catalog_path());
        }
    }

    fn select_jbod_member(&self) -> usize {
        let mut best_idx = 0;
        let mut max_free = 0u64;
        for (idx, m) in self.members.iter().enumerate() {
            if let Some((free, _)) = crate::win32::get_disk_space(m)
                && free > max_free
            {
                max_free = free;
                best_idx = idx;
            }
        }
        best_idx
    }

    fn find_direct_files(&self, rel_path: &str) -> Vec<usize> {
        let mut found = Vec::new();
        for (idx, m) in self.members.iter().enumerate() {
            if m.join(rel_path).is_file() {
                found.push(idx);
            }
        }
        if found.is_empty() {
            let key = rel_path.to_lowercase();
            if let Some(&idx) = self.jbod_map.read().get(&key)
                && idx < self.members.len()
                && self.members[idx].join(rel_path).is_file()
            {
                found.push(idx);
            }
        }
        found
    }

    fn get_or_create_file_node(
        &self,
        rel_path: &str,
        is_striped: bool,
        present_on: Vec<usize>,
        file_codec: VfsCodec,
        initial_size: u64,
    ) -> Arc<FileNode> {
        let key = rel_path.to_lowercase();
        {
            let map = self.file_nodes.read();
            if let Some(node) = map.get(&key) {
                node.open_handles.fetch_add(1, Ordering::Relaxed);
                return node.clone();
            }
        }
        let mut map = self.file_nodes.write();
        let node = map.entry(key).or_insert_with(|| {
            Arc::new(FileNode {
                rel_path: RwLock::new(rel_path.to_string()),
                file_hash: AtomicU64::new(hash_path_id(rel_path)),
                is_striped,
                present_on,
                file_codec,
                file_size: Mutex::new(initial_size),
                dirty_parts: Mutex::new(HashSet::new()),
                is_modified: Mutex::new(false),
                open_handles: AtomicUsize::new(0),
                open_files: Mutex::new(HashMap::new()),
                last_accessed_part: AtomicU64::new(u64::MAX),
                predictive_transitions: Mutex::new(HashMap::new()),
            })
        });
        node.open_handles.fetch_add(1, Ordering::Relaxed);
        node.clone()
    }

    /// Fast-path: retrieves a cached unbuffered Direct I/O handle via DuplicateHandle
    fn get_or_open_direct_file(&self, node: &FileNode, drive_idx: usize) -> Option<File> {
        let mut map = node.open_files.lock();
        if let Some(f) = map.get(&drive_idx) {
            return f.try_clone().ok();
        }
        let p = self.members[drive_idx].join(node.rel_path());
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(&p)
            .or_else(|_| File::open(&p))
            .ok()?;

        if let Ok(cloned) = file.try_clone() {
            map.insert(drive_idx, cloned);
        }
        map.get(&drive_idx).and_then(|f| f.try_clone().ok())
    }

    pub fn resolve_part_paths(
        &self,
        member_idx: usize,
        rel_path: &str,
        part_index: u64,
        ext: &str,
    ) -> (PathBuf, PathBuf) {
        let part_filename = format!("{}.part{}.{}", rel_path, part_index, ext);
        let store_path = self.members[member_idx]
            .join(".symstripe_store")
            .join(&part_filename);
        let legacy_path = self.members[member_idx].join(&part_filename);
        (store_path, legacy_path)
    }

    fn evict_part_to_disk(&self, part_arc: &Arc<RwLock<CachedPart>>) {
        let mut part = part_arc.write();
        if !part.dirty {
            return;
        }

        let target_drive_idx = self
            .stripe_config
            .map_part_to_drive(part.file_hash, part.part_index);
        let ext = match part.codec {
            VfsCodec::Lz4 => "lz4",
            VfsCodec::Zstd => "zst",
            VfsCodec::Store => "raw",
        };
        let (base_path, legacy_path) =
            self.resolve_part_paths(target_drive_idx, &part.rel_path, part.part_index, ext);

        if let Some(parent) = base_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _spindle_guard = self.spindle_throttles[target_drive_idx].acquire();

        let old_len = fs::metadata(&base_path)
            .or_else(|_| fs::metadata(&legacy_path))
            .map(|m| m.len())
            .unwrap_or(0);
        let compressed = match part.codec {
            VfsCodec::Lz4 => lz4_flex::compress_prepend_size(&part.data),
            VfsCodec::Zstd => {
                ZstdAtgCompressor::compress_atg(&part.data, 3).unwrap_or_else(|_| part.data.clone())
            }
            VfsCodec::Store => part.data.clone(),
        };

        let new_len = compressed.len() as u64;
        let diff = new_len as i64 - old_len as i64;
        if diff > 0 {
            self.add_used_bytes(diff as u64);
        } else if diff < 0 {
            self.sub_used_bytes((-diff) as u64);
        }

        self.raw_bytes_written
            .fetch_add(part.data.len() as u64, Ordering::Relaxed);
        self.compressed_bytes_written
            .fetch_add(new_len, Ordering::Relaxed);
        let _ = fs::write(&base_path, compressed);
        if legacy_path.exists() && legacy_path != base_path {
            let _ = fs::remove_file(legacy_path);
        }
        part.dirty = false;
    }

    pub fn insert_into_cache(&self, key: PartKey, new_part: Arc<RwLock<CachedPart>>) {
        let new_part_bytes = new_part.read().data.len();
        let mut evicted_parts = Vec::new();

        {
            let mut inner = self.global_cache.inner.lock();

            // Strict byte-bounded CLOCK eviction
            while inner.current_bytes + new_part_bytes > self.global_cache.max_bytes
                && !inner.clock_ring.is_empty()
            {
                let ring_len = inner.clock_ring.len();
                let mut loops = 0;
                let mut found_candidate = false;

                while loops < ring_len * 2 {
                    let hand = inner.clock_hand;
                    let candidate_key = inner.clock_ring[hand];

                    if let Some(part_arc) = inner.entries.get(&candidate_key).cloned() {
                        let is_pinned = {
                            let guard = part_arc.read();
                            Self::is_pinned_metadata(&guard.rel_path)
                        };

                        if is_pinned {
                            inner.clock_hand = (hand + 1) % ring_len;
                            loops += 1;
                            continue;
                        }

                        let was_referenced = {
                            let guard = part_arc.read();
                            guard.referenced.swap(false, Ordering::Relaxed)
                        };

                        if was_referenced {
                            inner.clock_hand = (hand + 1) % ring_len;
                            loops += 1;
                            continue;
                        }

                        if let Some(evicted) = inner.entries.remove(&candidate_key) {
                            inner.current_bytes = inner
                                .current_bytes
                                .saturating_sub(evicted.read().data.len());
                            evicted_parts.push(evicted);
                        }
                        inner.clock_ring.swap_remove(hand);
                        if !inner.clock_ring.is_empty() {
                            inner.clock_hand = hand % inner.clock_ring.len();
                        } else {
                            inner.clock_hand = 0;
                        }
                        found_candidate = true;
                        break;
                    } else {
                        inner.clock_ring.swap_remove(hand);
                        if !inner.clock_ring.is_empty() {
                            inner.clock_hand = hand % inner.clock_ring.len();
                        } else {
                            inner.clock_hand = 0;
                        }
                        loops += 1;
                    }
                }

                if !found_candidate {
                    break;
                }
            }

            inner.current_bytes += new_part_bytes;
            inner.clock_ring.push(key);
            inner.entries.insert(key, new_part);
        }

        for ev in evicted_parts {
            self.evict_part_to_disk(&ev);
        }

        self.prefetch_in_flight.lock().remove(&key);
        let mut inflight = self.in_flight_loads.lock();
        if let Some(cv) = inflight.remove(&key) {
            cv.notify_all();
        }
    }

    pub fn flush_node_dirty_parts(&self, node: &FileNode) {
        let dirty_indices: Vec<u64> = node.dirty_parts.lock().drain().collect();
        if dirty_indices.is_empty() {
            return;
        }

        let f_hash = node.file_hash.load(Ordering::Relaxed);
        let inner = self.global_cache.inner.lock();
        let mut drive_buckets: HashMap<usize, Vec<CachedPartTuple>> = HashMap::new();

        for part_idx in dirty_indices {
            let key = PartKey {
                file_hash: f_hash,
                part_index: part_idx,
            };
            if let Some(part_arc) = inner.entries.get(&key) {
                let target_drive = self.stripe_config.map_part_to_drive(f_hash, part_idx);
                drive_buckets
                    .entry(target_drive)
                    .or_default()
                    .push((part_idx, part_arc.clone()));
            }
        }
        drop(inner);

        drive_buckets
            .into_par_iter()
            .for_each(|(_drive, mut parts)| {
                parts.sort_unstable_by_key(|(idx, _)| *idx);
                for (_, part_arc) in parts {
                    self.evict_part_to_disk(&part_arc);
                }
            });
    }

    pub fn flush_all_dirty_nodes(&self) {
        let nodes: Vec<Arc<FileNode>> = self.file_nodes.read().values().cloned().collect();
        for node in nodes {
            if *node.is_modified.lock() {
                self.flush_node_dirty_parts(&node);
                let size = *node.file_size.lock();
                if node.is_striped && size > 0 {
                    self.save_meta_size(&node.rel_path(), size);
                }
            }
        }
    }

    fn fetch_cached_direct_page(
        &self,
        node: &Arc<FileNode>,
        page_index: u64,
        target_drive_idx: usize,
    ) -> Result<Arc<RwLock<CachedPart>>, FspError> {
        let current_hash = node.file_hash.load(Ordering::Relaxed);
        let key = PartKey {
            file_hash: current_hash,
            part_index: page_index,
        };

        let _loading_guard = loop {
            {
                let inner = self.global_cache.inner.lock();
                if let Some(part) = inner.entries.get(&key) {
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let guard = part.read();
                    guard.referenced.store(true, Ordering::Relaxed);
                    return Ok(part.clone());
                }
            }

            let mut inflight = self.in_flight_loads.lock();
            if let Some(cv) = inflight.get(&key).cloned() {
                cv.wait(&mut inflight);
                continue;
            } else {
                let cv = Arc::new(Condvar::new());
                inflight.insert(key, cv);
                break LoadingLeaderGuard {
                    table: self.in_flight_loads.clone(),
                    key,
                };
            }
        };

        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        let current_file_size = *node.file_size.lock();
        let page_offset = page_index * DIRECT_PAGE_SIZE;

        if page_offset >= current_file_size {
            let empty_part = Arc::new(RwLock::new(CachedPart {
                data: Vec::new(),
                dirty: false,
                referenced: AtomicBool::new(true),
                codec: VfsCodec::Store,
                rel_path: node.rel_path(),
                file_hash: current_hash,
                part_index: page_index,
            }));
            self.insert_into_cache(key, empty_part.clone());
            return Ok(empty_part);
        }

        let to_read = (current_file_size - page_offset).min(DIRECT_PAGE_SIZE) as usize;
        // Sector-align read length to 4096 for hardware Direct I/O (Bypass OS Cache)
        let aligned_len = (to_read + 4095) & !4095;
        let mut aligned_buf = AlignedBuffer::new(aligned_len, 4096);
        let mut actual_read = 0;

        let t0 = Instant::now();
        {
            let _spindle_guard = self.spindle_throttles[target_drive_idx].acquire();
            if let Some(file) = self.get_or_open_direct_file(node, target_drive_idx) {
                if let Ok(n) = file.seek_read(aligned_buf.as_slice_mut(), page_offset) {
                    actual_read = n.min(to_read);
                }
            } else {
                let p = self.members[target_drive_idx].join(node.rel_path());
                if let Ok(file) = File::open(&p) {
                    let _ = file.seek_read(&mut aligned_buf.as_slice_mut()[..to_read], page_offset);
                    actual_read = to_read;
                }
            }
        }
        let lat = t0.elapsed().as_millis() as u64;
        self.io_max_latency_ms[target_drive_idx].fetch_max(lat, Ordering::Relaxed);

        let data = aligned_buf.as_slice_mut()[..actual_read].to_vec();

        let new_part = Arc::new(RwLock::new(CachedPart {
            data,
            dirty: false,
            referenced: AtomicBool::new(true),
            codec: VfsCodec::Store,
            rel_path: node.rel_path(),
            file_hash: current_hash,
            part_index: page_index,
        }));

        self.insert_into_cache(key, new_part.clone());
        Ok(new_part)
    }

    fn fetch_cached_part(
        &self,
        node: &Arc<FileNode>,
        part_index: u64,
        target_drive_idx: usize,
        allow_prefetch: bool,
    ) -> Result<Arc<RwLock<CachedPart>>, FspError> {
        let current_hash = node.file_hash.load(Ordering::Relaxed);
        let key = PartKey {
            file_hash: current_hash,
            part_index,
        };

        let _loading_guard = loop {
            {
                let inner = self.global_cache.inner.lock();
                if let Some(part) = inner.entries.get(&key) {
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let guard = part.read();
                    guard.referenced.store(true, Ordering::Relaxed);
                    return Ok(part.clone());
                }
            }

            let mut inflight = self.in_flight_loads.lock();
            if let Some(cv) = inflight.get(&key).cloned() {
                cv.wait(&mut inflight);
                continue;
            } else {
                let cv = Arc::new(Condvar::new());
                inflight.insert(key, cv);
                break LoadingLeaderGuard {
                    table: self.in_flight_loads.clone(),
                    key,
                };
            }
        };

        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        let rel = node.rel_path();
        let (store_lz4, legacy_lz4) =
            self.resolve_part_paths(target_drive_idx, &rel, part_index, "lz4");
        let (store_zst, legacy_zst) =
            self.resolve_part_paths(target_drive_idx, &rel, part_index, "zst");
        let (store_raw, legacy_raw) =
            self.resolve_part_paths(target_drive_idx, &rel, part_index, "raw");

        let lz4_p = if store_lz4.exists() {
            store_lz4
        } else {
            legacy_lz4
        };
        let zst_p = if store_zst.exists() {
            store_zst
        } else {
            legacy_zst
        };
        let raw_p = if store_raw.exists() {
            store_raw
        } else {
            legacy_raw
        };

        let mut data = Vec::new();
        let mut loaded_codec = node.file_codec;
        let t0 = Instant::now();

        {
            let _spindle_guard = self.spindle_throttles[target_drive_idx].acquire();
            if lz4_p.exists() {
                if let Ok(comp) = fs::read(&lz4_p) {
                    data = lz4_flex::decompress_size_prepended(&comp).unwrap_or_default();
                    loaded_codec = VfsCodec::Lz4;
                }
            } else if zst_p.exists() {
                if let Ok(comp) = fs::read(&zst_p) {
                    data = ZstdAtgCompressor::decompress_atg(&comp).unwrap_or_default();
                    loaded_codec = VfsCodec::Zstd;
                }
            } else if raw_p.exists() {
                data = fs::read(&raw_p).unwrap_or_default();
                loaded_codec = VfsCodec::Store;
            }
        }

        let lat = t0.elapsed().as_millis() as u64;
        self.io_max_latency_ms[target_drive_idx].fetch_max(lat, Ordering::Relaxed);

        let new_part = Arc::new(RwLock::new(CachedPart {
            data,
            dirty: false,
            referenced: AtomicBool::new(true),
            codec: loaded_codec,
            rel_path: rel,
            file_hash: current_hash,
            part_index,
        }));

        self.insert_into_cache(key, new_part.clone());

        if allow_prefetch {
            let last = node.last_accessed_part.swap(part_index, Ordering::Relaxed);
            if last != part_index && last != u64::MAX {
                node.predictive_transitions.lock().insert(last, part_index);
            }

            let file_size = *node.file_size.lock();
            let stripe_size = self.stripe_config.stripe_size_bytes;
            let mut candidates = Vec::new();

            if let Some(&predicted) = node.predictive_transitions.lock().get(&part_index) {
                candidates.push(predicted);
            }

            let lookahead_depth = if self.members.len() > 1 { 4 } else { 2 };
            for step in 1..=lookahead_depth {
                candidates.push(part_index + step);
            }

            let mut final_candidates = Vec::new();
            {
                let inner = self.global_cache.inner.lock();
                let mut in_flight = self.prefetch_in_flight.lock();

                for p_idx in candidates {
                    let next_key = PartKey {
                        file_hash: current_hash,
                        part_index: p_idx,
                    };
                    if p_idx.saturating_mul(stripe_size) < file_size
                        && !inner.entries.contains_key(&next_key)
                        && !in_flight.contains(&next_key)
                    {
                        in_flight.insert(next_key);
                        final_candidates.push(p_idx);
                    }
                }
            }

            if !final_candidates.is_empty() {
                final_candidates.sort_by_key(|&p_idx| {
                    let d = self.stripe_config.map_part_to_drive(current_hash, p_idx);
                    let drive_lat = self
                        .io_max_latency_ms
                        .get(d)
                        .map(|a| a.load(Ordering::Relaxed))
                        .unwrap_or(0);
                    std::cmp::Reverse(drive_lat)
                });

                for p_idx in final_candidates {
                    let target_d = self.stripe_config.map_part_to_drive(current_hash, p_idx);
                    let p_key = PartKey {
                        file_hash: current_hash,
                        part_index: p_idx,
                    };
                    self.iocp
                        .submit_prefetch(self.clone(), Arc::clone(node), p_key, target_d);
                }
            }
        }

        Ok(new_part)
    }
}

impl FileSystemContext for SymStripePoolFs {
    type FileContext = PoolFileHandle;

    fn get_volume_info(&self, out_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let mut total_free = 0u64;
        let mut total_size = 0u64;

        if self.policy == 1 {
            let weights = &self.stripe_config.drive_weights;
            let mut min_unit_free = u64::MAX;
            let mut min_unit_total = u64::MAX;
            let mut sum_weights: u64 = 0;

            for (idx, m) in self.members.iter().enumerate() {
                let w = weights.get(idx).copied().unwrap_or(1).max(1) as u64;
                sum_weights += w;
                if let Some((free, tot)) = crate::win32::get_disk_space(m) {
                    min_unit_free = min_unit_free.min(free / w);
                    min_unit_total = min_unit_total.min(tot / w);
                }
            }
            if min_unit_total != u64::MAX && sum_weights > 0 {
                total_free = min_unit_free.saturating_mul(sum_weights);
                total_size = min_unit_total.saturating_mul(sum_weights);
            }
        } else if self.policy == 2 {
            let mut min_free = u64::MAX;
            let mut min_total = u64::MAX;
            for m in self.members.iter() {
                if let Some((free, tot)) = crate::win32::get_disk_space(m) {
                    min_free = min_free.min(free);
                    min_total = min_total.min(tot);
                }
            }
            if min_total != u64::MAX {
                total_free = min_free;
                total_size = min_total;
            }
        } else {
            for m in self.members.iter() {
                if let Some((free, tot)) = crate::win32::get_disk_space(m) {
                    total_free += free;
                    total_size += tot;
                }
            }
        }

        if self.custom_size_gb > 0 {
            total_size = self.custom_size_gb * 1024 * 1024 * 1024;
            let used = self.pool_used_bytes.load(Ordering::Relaxed);
            total_free = total_size.saturating_sub(used);
        }

        out_info.total_size = total_size;
        out_info.free_size = total_free;
        Ok(())
    }

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        sd: Option<&mut [c_void]>,
        _rp: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let rel_path = Self::normalize_path(file_name);

        let is_dir = if rel_path.is_empty() {
            true
        } else {
            let key = rel_path.to_lowercase();
            if self.meta_sizes.read().contains_key(&key)
                || self.jbod_map.read().contains_key(&key)
                || self.file_nodes.read().contains_key(&key)
            {
                false
            } else {
                let mut found_as_file = false;
                let mut found_as_dir = false;
                for m in self.members.iter() {
                    let p = m.join(&rel_path);
                    if p.is_dir() {
                        found_as_dir = true;
                        break;
                    }
                    if p.is_file() {
                        found_as_file = true;
                        break;
                    }
                }

                if found_as_dir {
                    true
                } else if found_as_file {
                    false
                } else {
                    return Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND));
                }
            }
        };

        let desc = get_default_security_descriptor();
        if let Some(buf) = sd {
            if buf.len() < desc.len() {
                return Err(FspError::NTSTATUS(STATUS_BUFFER_TOO_SMALL));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    desc.as_ptr(),
                    buf.as_mut_ptr() as *mut u8,
                    desc.len(),
                );
            }
        }
        Ok(FileSecurity {
            attributes: if is_dir { 0x10 } else { 0x80 },
            reparse: false,
            sz_security_descriptor: desc.len() as u64,
        })
    }

    fn get_security(
        &self,
        _ctx: &Self::FileContext,
        sd: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        let desc = get_default_security_descriptor();
        if let Some(buf) = sd {
            if buf.len() < desc.len() {
                return Err(FspError::NTSTATUS(STATUS_BUFFER_TOO_SMALL));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    desc.as_ptr(),
                    buf.as_mut_ptr() as *mut u8,
                    desc.len(),
                );
            }
        }
        Ok(desc.len() as u64)
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let rel_path = Self::normalize_path(file_name);

        let (is_dir, is_striped, present_on, file_size) =
            if rel_path.is_empty() || self.members.iter().any(|m| m.join(&rel_path).is_dir()) {
                (true, false, vec![], 0)
            } else if let Some(size) = self.read_meta_size(&rel_path) {
                (false, true, vec![], size)
            } else {
                let found = self.find_direct_files(&rel_path);
                if found.is_empty() {
                    return Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND));
                }
                let meta = fs::metadata(self.members[found[0]].join(&rel_path))
                    .map_err(|_| FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
                (false, false, found, meta.len())
            };

        let node = if !is_dir {
            Some(self.get_or_create_file_node(
                &rel_path,
                is_striped,
                present_on,
                self.pool_codec,
                file_size,
            ))
        } else {
            None
        };

        {
            let current_size = if let Some(n) = &node {
                *n.file_size.lock()
            } else {
                0
            };
            let info = file_info.as_mut();
            *info = FileInfo::default();
            info.file_size = current_size;
            info.allocation_size = if is_dir {
                0
            } else {
                (current_size + 4095) & !4095
            };
            info.file_attributes = if is_dir { 0x10 } else { 0x80 };
            info.hard_links = 1;
            info.index_number = hash_path_id(&rel_path);

            let check_p = if is_striped {
                self.meta_path(&rel_path)
            } else if let Some(n) = &node
                && !n.present_on.is_empty()
            {
                self.members[n.present_on[0]].join(&rel_path)
            } else {
                PathBuf::new()
            };

            if check_p.exists()
                && let Ok(m) = fs::metadata(&check_p)
            {
                let ft = get_metadata_times(&m);
                info.creation_time = ft.creation_time;
                info.last_access_time = ft.last_access_time;
                info.last_write_time = ft.last_write_time;
                info.change_time = ft.change_time;
            }
        }

        Ok(PoolFileHandle {
            is_dir,
            rel_path: RwLock::new(rel_path),
            node,
            dir_buffer: Mutex::new(DirBuffer::new()),
            delete_on_close: Mutex::new(false),
        })
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let rel_path = Self::normalize_path(file_name);
        let is_dir = (create_options & 1 != 0) || (file_attributes & 0x10 != 0);
        let lower_rel = rel_path.to_lowercase();

        let is_hot_meta = lower_rel.ends_with(".utoc")
            || lower_rel.ends_with(".sig")
            || lower_rel.ends_with(".toc")
            || lower_rel.ends_with(".idx")
            || lower_rel.ends_with(".exe")
            || lower_rel.ends_with(".dll")
            || lower_rel.ends_with(".ini")
            || lower_rel.ends_with(".cfg");

        let is_bulk_stream = lower_rel.ends_with(".ucas")
            || lower_rel.ends_with(".bk2")
            || lower_rel.ends_with(".mp4")
            || lower_rel.ends_with(".wem")
            || lower_rel.ends_with(".pck");

        let (is_striped, present_on, file_codec) = if is_dir {
            for m in self.members.iter() {
                let _ = fs::create_dir_all(m.join(&rel_path));
                let _ = fs::create_dir_all(m.join(".symstripe_store").join(&rel_path));
            }
            (false, vec![], self.pool_codec)
        } else if self.tiered_storage && is_hot_meta {
            (false, vec![0], VfsCodec::Store)
        } else if (self.tiered_storage && is_bulk_stream) || self.policy == 1 {
            (true, vec![], self.pool_codec)
        } else if self.policy == 2 {
            let all: Vec<usize> = (0..self.members.len()).collect();
            (false, all, self.pool_codec)
        } else {
            (false, vec![self.select_jbod_member()], self.pool_codec)
        };

        let node = if !is_dir {
            let n = self.get_or_create_file_node(
                &rel_path,
                is_striped,
                present_on.clone(),
                file_codec,
                0,
            );
            *n.is_modified.lock() = true;

            if !is_striped {
                for &idx in &present_on {
                    let p = self.members[idx].join(&rel_path);
                    if let Some(parent) = p.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&p);
                }
                if present_on.len() == 1 && present_on[0] != 0 {
                    self.jbod_map
                        .write()
                        .insert(rel_path.to_lowercase(), present_on[0]);
                    self.save_jbod_catalog();
                }
            } else {
                self.save_meta_size(&rel_path, 0);
            }
            Some(n)
        } else {
            None
        };

        {
            let now_ft = get_current_filetime();
            let info = file_info.as_mut();
            *info = FileInfo::default();
            info.file_size = 0;
            info.allocation_size = 0;
            info.file_attributes = if is_dir { 0x10 } else { 0x80 };
            info.creation_time = now_ft;
            info.last_access_time = now_ft;
            info.last_write_time = now_ft;
            info.change_time = now_ft;
            info.hard_links = 1;
            info.index_number = hash_path_id(&rel_path);
        }

        self.log(format!(
            "[VFS CREATE] {} '{}'",
            if is_dir { "DIR" } else { "FILE" },
            rel_path
        ));

        Ok(PoolFileHandle {
            is_dir,
            rel_path: RwLock::new(rel_path),
            node,
            dir_buffer: Mutex::new(DirBuffer::new()),
            delete_on_close: Mutex::new(false),
        })
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let node = context
            .node
            .as_ref()
            .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))?;

        *node.is_modified.lock() = true;

        let mut current_size = node.file_size.lock();
        let write_offset = if write_to_eof { *current_size } else { offset };

        if !node.is_striped {
            // Invalidate any overlapping cached direct pages in GlobalStripeCache using div_ceil
            let current_hash = node.file_hash.load(Ordering::Relaxed);
            let start_page = write_offset / DIRECT_PAGE_SIZE;
            let end_page = (write_offset + buffer.len() as u64).div_ceil(DIRECT_PAGE_SIZE);

            {
                let mut inner = self.global_cache.inner.lock();
                for p in start_page..end_page {
                    let k = PartKey {
                        file_hash: current_hash,
                        part_index: p,
                    };
                    if let Some(removed) = inner.entries.remove(&k) {
                        inner.current_bytes = inner
                            .current_bytes
                            .saturating_sub(removed.read().data.len());
                    }
                }
            }

            let first_idx = node.present_on[0];
            let p_first = self.members[first_idx].join(node.rel_path());
            let old_len = fs::metadata(&p_first).map(|m| m.len()).unwrap_or(0);

            let write_results: Vec<Result<(), FspError>> = node
                .present_on
                .par_iter()
                .map(|&member_idx| {
                    let _spindle_guard = self.spindle_throttles[member_idx].acquire();
                    let p = self.members[member_idx].join(node.rel_path());

                    if let Some(parent) = p.parent() {
                        let _ = fs::create_dir_all(parent);
                    }

                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&p)
                        .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

                    file.seek_write(buffer, write_offset)
                        .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

                    if let Some(counter) = self.io_bytes.get(member_idx) {
                        counter.fetch_add(buffer.len() as u64, Ordering::Relaxed);
                    }
                    Ok::<(), FspError>(())
                })
                .collect();

            for res in write_results {
                res?;
            }

            let new_len = fs::metadata(&p_first).map(|m| m.len()).unwrap_or(old_len);
            let diff = new_len as i64 - old_len as i64;

            if diff > 0 {
                self.add_used_bytes((diff as u64) * (node.present_on.len() as u64));
            } else if diff < 0 {
                self.sub_used_bytes(((-diff) as u64) * (node.present_on.len() as u64));
            }
        } else {
            let buffer_len = buffer.len();
            let current_hash = node.file_hash.load(Ordering::Relaxed);
            let slices = self
                .stripe_config
                .map_io_slices(current_hash, write_offset, buffer_len);
            let mut tasks = Vec::with_capacity(slices.len());
            let mut remaining_buf = buffer;

            for slice in slices {
                let part_arc =
                    self.fetch_cached_part(node, slice.part_index, slice.target_drive_idx, false)?;
                let len = slice.length.min(remaining_buf.len());
                let (src_slice, rest) = remaining_buf.split_at(len);
                remaining_buf = rest;
                tasks.push((slice, part_arc, src_slice));
            }

            let io_bytes = self.io_bytes.clone();
            let codec = node.file_codec;

            for (slice, part_arc, src_slice) in tasks {
                node.dirty_parts.lock().insert(slice.part_index);
                let mut part = part_arc.write();
                let start_idx = slice.offset_in_part as usize;
                let end_pos = start_idx + src_slice.len();

                if part.data.len() < end_pos {
                    part.data.resize(end_pos, 0);
                }

                part.data[start_idx..end_pos].copy_from_slice(src_slice);
                part.dirty = true;
                part.codec = codec;
                part.referenced.store(true, Ordering::Relaxed);

                if let Some(counter) = io_bytes.get(slice.target_drive_idx) {
                    counter.fetch_add(src_slice.len() as u64, Ordering::Relaxed);
                }
            }
        }

        let new_len = write_offset + buffer.len() as u64;
        if new_len > *current_size {
            *current_size = new_len;
        }

        file_info.file_size = *current_size;
        file_info.allocation_size = (*current_size + 4095) & !4095;
        file_info.file_attributes = 0x80;
        file_info.hard_links = 1;
        file_info.index_number = hash_path_id(&node.rel_path());

        Ok(buffer.len() as u32)
    }

    fn read(&self, ctx: &Self::FileContext, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        let node = context_node(ctx)?;

        let current_file_size = *node.file_size.lock();
        if offset >= current_file_size {
            return Ok(0);
        }

        let max_possible = (current_file_size - offset) as usize;
        let bytes_to_read = max_possible.min(buffer.len());
        if bytes_to_read == 0 {
            return Ok(0);
        }

        let target_slice = &mut buffer[..bytes_to_read];

        // -------------------------------------------------------------
        // UNIFIED CLOCK CACHE FOR NON-STRIPED / RAID-1 MIRROR FILES
        // -------------------------------------------------------------
        if !node.is_striped {
            let num_present = node.present_on.len().max(1);
            let mut curr_offset = offset;
            let mut remaining = bytes_to_read;
            let mut dest_buf = target_slice;
            let mut total_copied = 0;

            while remaining > 0 {
                let page_index = curr_offset / DIRECT_PAGE_SIZE;
                let offset_in_page = (curr_offset % DIRECT_PAGE_SIZE) as usize;
                let bytes_in_page = ((DIRECT_PAGE_SIZE as usize) - offset_in_page).min(remaining);

                // Alternates page scheduling across active mirror spindles for concurrent reading
                let drive_slot = (page_index as usize
                    + self.rr_counter.fetch_add(1, Ordering::Relaxed))
                    % num_present;
                let target_drive = node.present_on[drive_slot];

                let part_arc = self.fetch_cached_direct_page(node, page_index, target_drive)?;
                let part = part_arc.read();
                part.referenced.store(true, Ordering::Relaxed);

                let src_data = &part.data;
                if offset_in_page < src_data.len() {
                    let copy_len = (src_data.len() - offset_in_page).min(bytes_in_page);
                    dest_buf[..copy_len]
                        .copy_from_slice(&src_data[offset_in_page..offset_in_page + copy_len]);
                    if copy_len < bytes_in_page {
                        dest_buf[copy_len..bytes_in_page].fill(0);
                    }
                    total_copied += copy_len;
                } else {
                    dest_buf[..bytes_in_page].fill(0);
                }

                if let Some(counter) = self.io_bytes.get(target_drive) {
                    counter.fetch_add(bytes_in_page as u64, Ordering::Relaxed);
                }

                // Lookahead Prefetch: queue next page on alternate spindle via IOCP
                let next_page = page_index + 1;
                if next_page * DIRECT_PAGE_SIZE < current_file_size {
                    let next_drive_slot = (drive_slot + 1) % num_present;
                    let next_target_drive = node.present_on[next_drive_slot];
                    let next_key = PartKey {
                        file_hash: node.file_hash.load(Ordering::Relaxed),
                        part_index: next_page,
                    };

                    let should_prefetch = {
                        let inner = self.global_cache.inner.lock();
                        let mut in_flight = self.prefetch_in_flight.lock();
                        if !inner.entries.contains_key(&next_key) && !in_flight.contains(&next_key)
                        {
                            in_flight.insert(next_key);
                            true
                        } else {
                            false
                        }
                    };

                    if should_prefetch {
                        self.iocp.submit_prefetch_direct(
                            self.clone(),
                            Arc::clone(node),
                            next_page,
                            next_target_drive,
                        );
                    }
                }

                dest_buf = &mut dest_buf[bytes_in_page..];
                curr_offset += bytes_in_page as u64;
                remaining -= bytes_in_page;
            }

            return Ok(total_copied as u32);
        }

        // -------------------------------------------------------------
        // PARALLEL EXTENT STRIPING (RAID-0)
        // -------------------------------------------------------------
        let current_hash = node.file_hash.load(Ordering::Relaxed);
        let slices = self
            .stripe_config
            .map_io_slices(current_hash, offset, bytes_to_read);

        let mut tasks = Vec::with_capacity(slices.len());
        let mut remaining_dest = target_slice;

        let stripe_size = self.stripe_config.stripe_size_bytes;
        let offset_in_stripe = offset % stripe_size;
        let trigger_lookahead = offset_in_stripe > (stripe_size / 2);

        for (i, slice) in slices.iter().enumerate() {
            let is_last = (i + 1 == slices.len()) && trigger_lookahead;
            let part_arc =
                self.fetch_cached_part(node, slice.part_index, slice.target_drive_idx, is_last)?;
            let len = slice.length.min(remaining_dest.len());
            let (dest, rest) = remaining_dest.split_at_mut(len);
            remaining_dest = rest;
            tasks.push((slice, part_arc, dest));
        }

        let io_bytes = self.io_bytes.clone();
        let mut total = 0;

        for (slice, part_arc, dest) in tasks {
            let part = part_arc.read();
            part.referenced.store(true, Ordering::Relaxed);
            let src_len = part.data.len();
            let offset_idx = slice.offset_in_part as usize;

            if offset_idx < src_len {
                let copy_len = (src_len - offset_idx).min(dest.len());
                dest[..copy_len].copy_from_slice(&part.data[offset_idx..offset_idx + copy_len]);
                if copy_len < dest.len() {
                    dest[copy_len..].fill(0);
                }
                if let Some(counter) = io_bytes.get(slice.target_drive_idx) {
                    counter.fetch_add(copy_len as u64, Ordering::Relaxed);
                }
                total += copy_len;
            } else {
                dest.fill(0);
            }
        }

        Ok(total as u32)
    }

    fn read_directory(
        &self,
        ctx: &Self::FileContext,
        _pat: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !ctx.is_dir {
            return Err(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST));
        }

        let dir_buf = ctx.dir_buffer.lock();

        if marker.is_none()
            && let Ok(dir_lock) = dir_buf.acquire(true, None)
        {
            let mut dot = DirInfo::<255>::new();
            {
                let info = dot.file_info_mut();
                info.file_attributes = 0x10;
                info.index_number = hash_path_id(&ctx.rel_path());
                info.hard_links = 1;
            }
            let _ = dot.set_name(".");
            let _ = dir_lock.write(&mut dot);

            let mut dotdot = DirInfo::<255>::new();
            {
                let info = dotdot.file_info_mut();
                info.file_attributes = 0x10;
                info.index_number = hash_path_id(&format!("{}..", ctx.rel_path()));
                info.hard_links = 1;
            }
            let _ = dotdot.set_name("..");
            let _ = dir_lock.write(&mut dotdot);

            let rel = ctx.rel_path();
            let mut items: HashMap<String, (String, bool, u64, Option<FileTimes>)> = HashMap::new();

            for m in self.members.iter() {
                if let Ok(entries) = fs::read_dir(m.join(&rel)) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.starts_with(".symstripe") || name.contains(".part") {
                            continue;
                        }

                        if name.ends_with(".symstripe_meta") {
                            let clean = name
                                .strip_suffix(".symstripe_meta")
                                .unwrap_or(&name)
                                .to_string();
                            let key = if rel.is_empty() {
                                clean.to_lowercase()
                            } else {
                                format!("{}\\{}", rel, clean).to_lowercase()
                            };
                            let size = self.read_meta_size(&key).unwrap_or(0);
                            let ft = entry.metadata().ok().map(|m| get_metadata_times(&m));
                            items.insert(clean.to_lowercase(), (clean, false, size, ft));
                        } else if let Ok(meta) = entry.metadata() {
                            let key = name.to_lowercase();
                            items.entry(key).or_insert_with(|| {
                                let is_dir = meta.is_dir();
                                let size = if is_dir { 0 } else { meta.len() };
                                let ft = Some(get_metadata_times(&meta));
                                (name, is_dir, size, ft)
                            });
                        }
                    }
                }
            }

            let mut sorted: Vec<_> = items.into_values().collect();
            sorted.sort_by_key(|a| a.0.to_lowercase());

            for (name, is_dir, size, ft_opt) in sorted {
                let mut dir_info = DirInfo::<255>::new();
                {
                    let info = dir_info.file_info_mut();
                    info.file_size = size;
                    info.allocation_size = if is_dir { 0 } else { (size + 4095) & !4095 };
                    info.file_attributes = if is_dir { 0x10 } else { 0x80 };
                    info.hard_links = 1;
                    info.index_number = hash_path_id(&format!("{}\\{}", rel, name));

                    if let Some(ft) = ft_opt {
                        info.creation_time = ft.creation_time;
                        info.last_access_time = ft.last_access_time;
                        info.last_write_time = ft.last_write_time;
                        info.change_time = ft.change_time;
                    }
                }
                let _ = dir_info.set_name(&name);
                let _ = dir_lock.write(&mut dir_info);
            }
        }

        Ok(dir_buf.read(marker, buffer))
    }

    fn cleanup(&self, ctx: &Self::FileContext, _name: Option<&U16CStr>, flags: u32) {
        let should_delete = (flags & 0x01 != 0) || *ctx.delete_on_close.lock();

        if should_delete {
            *ctx.delete_on_close.lock() = true;
            let rel = ctx.rel_path();
            self.meta_sizes.write().remove(&rel.to_lowercase());

            if ctx.is_dir {
                for m in self.members.iter() {
                    let d1 = m.join(&rel);
                    let d2 = m.join(".symstripe_store").join(&rel);
                    for dir in [&d1, &d2] {
                        if dir.exists() {
                            for entry in walkdir::WalkDir::new(dir)
                                .into_iter()
                                .filter_map(|e| e.ok())
                            {
                                if let Ok(m) = entry.metadata()
                                    && m.is_file()
                                {
                                    self.sub_used_bytes(m.len());
                                }
                            }
                            let _ = fs::remove_dir_all(dir);
                        }
                    }
                }
            } else if let Some(node) = &ctx.node {
                *node.is_modified.lock() = false;
                node.open_files.lock().clear();
                self.file_nodes.write().remove(&rel.to_lowercase());

                let current_hash = node.file_hash.load(Ordering::Relaxed);
                {
                    let mut inner = self.global_cache.inner.lock();
                    let mut freed_bytes = 0usize;
                    inner.entries.retain(|k, v| {
                        if k.file_hash == current_hash {
                            freed_bytes += v.read().data.len();
                            false
                        } else {
                            true
                        }
                    });
                    inner.current_bytes = inner.current_bytes.saturating_sub(freed_bytes);
                }

                if node.is_striped {
                    let meta_p = self.meta_path(&rel);

                    let mut file_size = *node.file_size.lock();
                    if file_size == 0
                        && let Ok(s) = fs::read_to_string(&meta_p)
                    {
                        file_size = s.trim().parse::<u64>().unwrap_or(0);
                    }

                    if let Ok(m) = fs::metadata(&meta_p) {
                        self.sub_used_bytes(m.len());
                    }
                    let _ = fs::remove_file(&meta_p);

                    let stripe_size = self.stripe_config.stripe_size_bytes;
                    let num_parts =
                        (file_size + stripe_size.saturating_sub(1)) / stripe_size.max(1);

                    for part_index in 0..=(num_parts + 2) {
                        let target_drive_idx = self
                            .stripe_config
                            .map_part_to_drive(current_hash, part_index);
                        for ext in ["lz4", "zst", "raw"] {
                            let (store_p, legacy_p) =
                                self.resolve_part_paths(target_drive_idx, &rel, part_index, ext);

                            for p in [store_p, legacy_p] {
                                if let Ok(m) = fs::metadata(&p) {
                                    self.sub_used_bytes(m.len());
                                    let _ = fs::remove_file(&p);
                                }
                            }
                        }
                    }
                } else {
                    let mut total_del = 0;
                    for &member_idx in &node.present_on {
                        let target_p = self.members[member_idx].join(&rel);
                        if let Ok(m) = fs::metadata(&target_p) {
                            total_del += m.len();
                        }
                        let _ = fs::remove_file(&target_p);
                    }
                    self.sub_used_bytes(total_del);

                    if node.present_on.len() == 1 && node.present_on[0] != 0 {
                        self.jbod_map.write().remove(&rel.to_lowercase());
                        self.save_jbod_catalog();
                    }
                }
            }
            self.log(format!("[VFS PURGE] Deleted '{}'", rel));
        } else if let Some(node) = &ctx.node
            && *node.is_modified.lock()
        {
            self.flush_node_dirty_parts(node);
            let size = *node.file_size.lock();
            if node.is_striped && size > 0 {
                self.save_meta_size(&node.rel_path(), size);
            }
        }
    }

    fn flush(
        &self,
        ctx: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if let Some(ctx) = ctx {
            if *ctx.delete_on_close.lock() {
                return Ok(());
            }
            if let Some(node) = &ctx.node {
                self.flush_node_dirty_parts(node);
                let size = *node.file_size.lock();
                if node.is_striped && size > 0 {
                    self.save_meta_size(&node.rel_path(), size);
                }
            }
        }
        Ok(())
    }

    fn close(&self, ctx: Self::FileContext) {
        if *ctx.delete_on_close.lock() {
            return;
        }

        if let Some(node) = &ctx.node {
            let remaining = node.open_handles.fetch_sub(1, Ordering::Relaxed) - 1;
            if *node.is_modified.lock() {
                self.flush_node_dirty_parts(node);
                *node.is_modified.lock() = false;
                let size = *node.file_size.lock();
                if node.is_striped && size > 0 {
                    self.save_meta_size(&node.rel_path(), size);
                }
            }
            if remaining == 0 {
                node.open_files.lock().clear();
                self.file_nodes
                    .write()
                    .remove(&node.rel_path().to_lowercase());
            }
        }
    }

    fn get_file_info(&self, ctx: &Self::FileContext, info: &mut FileInfo) -> winfsp::Result<()> {
        let (size, is_striped, present_on, rel) = if let Some(node) = &ctx.node {
            (
                *node.file_size.lock(),
                node.is_striped,
                node.present_on.clone(),
                node.rel_path(),
            )
        } else {
            (0, false, vec![], ctx.rel_path())
        };

        *info = FileInfo::default();
        info.file_size = size;
        info.allocation_size = if ctx.is_dir { 0 } else { (size + 4095) & !4095 };
        info.file_attributes = if ctx.is_dir { 0x10 } else { 0x80 };
        info.hard_links = 1;
        info.index_number = hash_path_id(&rel);

        let check_p = if is_striped {
            self.meta_path(&rel)
        } else if !present_on.is_empty() {
            self.members[present_on[0]].join(&rel)
        } else {
            PathBuf::new()
        };

        if check_p.exists()
            && let Ok(m) = fs::metadata(&check_p)
        {
            let ft = get_metadata_times(&m);
            info.creation_time = ft.creation_time;
            info.last_access_time = ft.last_access_time;
            info.last_write_time = ft.last_write_time;
            info.change_time = ft.change_time;
        }
        Ok(())
    }

    fn overwrite(
        &self,
        ctx: &Self::FileContext,
        attr: u32,
        replace: bool,
        _alloc: u64,
        _buf: Option<&[u8]>,
        info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let node = ctx
            .node
            .as_ref()
            .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))?;
        *node.is_modified.lock() = true;
        *node.file_size.lock() = 0;
        let rel = node.rel_path();

        if !node.is_striped {
            let mut size_diff = 0i64;
            for &member_idx in &node.present_on {
                let p = self.members[member_idx].join(&rel);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let old_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);

                let _spindle_guard = self.spindle_throttles[member_idx].acquire();
                let _ = OpenOptions::new().write(true).truncate(true).open(&p);

                size_diff -= old_len as i64;
            }
            self.sub_used_bytes((-size_diff) as u64);
        } else {
            self.save_meta_size(&rel, 0);
        }

        let now_ft = get_current_filetime();
        *info = FileInfo::default();
        info.file_size = 0;
        info.allocation_size = 0;
        info.file_attributes = if replace {
            if attr == 0 { 0x80 } else { attr }
        } else {
            info.file_attributes | attr
        };
        info.hard_links = 1;
        info.index_number = hash_path_id(&rel);
        info.last_write_time = now_ft;
        info.change_time = now_ft;
        Ok(())
    }

    fn set_file_size(
        &self,
        ctx: &Self::FileContext,
        new_size: u64,
        _set_alloc: bool,
        info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let node = ctx
            .node
            .as_ref()
            .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))?;
        *node.is_modified.lock() = true;
        *node.file_size.lock() = new_size;
        let rel = node.rel_path();

        if !node.is_striped {
            let mut size_diff = 0i64;
            for &member_idx in &node.present_on {
                let p = self.members[member_idx].join(&rel);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let old_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);

                let _spindle_guard = self.spindle_throttles[member_idx].acquire();
                if let Ok(f) = OpenOptions::new().write(true).open(&p) {
                    let _ = f.set_len(new_size);
                    let new_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(new_size);
                    size_diff += (new_len as i64) - (old_len as i64);
                }
            }
            if size_diff > 0 {
                self.add_used_bytes(size_diff as u64);
            } else if size_diff < 0 {
                self.sub_used_bytes((-size_diff) as u64);
            }
        } else {
            self.save_meta_size(&rel, new_size);
        }

        info.file_size = new_size;
        info.allocation_size = (new_size + 4095) & !4095;
        info.file_attributes = 0x80;
        info.hard_links = 1;
        info.index_number = hash_path_id(&rel);
        Ok(())
    }

    fn set_basic_info(
        &self,
        ctx: &Self::FileContext,
        attr: u32,
        _c: u64,
        _a: u64,
        _w: u64,
        _ch: u64,
        info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        info.file_attributes = if attr != 0 && attr != u32::MAX {
            attr
        } else if ctx.is_dir {
            0x10
        } else {
            0x80
        };
        info.hard_links = 1;
        info.index_number = hash_path_id(&ctx.rel_path());
        Ok(())
    }

    fn set_delete(&self, ctx: &Self::FileContext, _f: &U16CStr, del: bool) -> winfsp::Result<()> {
        *ctx.delete_on_close.lock() = del;
        Ok(())
    }

    fn rename(
        &self,
        ctx: &Self::FileContext,
        _old: &U16CStr,
        new_name: &U16CStr,
        _replace: bool,
    ) -> winfsp::Result<()> {
        let new_rel = Self::normalize_path(new_name);
        let old_rel = ctx.rel_path();

        if let Some(node) = &ctx.node {
            node.open_files.lock().clear();

            if *node.is_modified.lock() {
                self.flush_node_dirty_parts(node);
                let size = *node.file_size.lock();
                if node.is_striped && size > 0 {
                    self.save_meta_size(&old_rel, size);
                }
                *node.is_modified.lock() = false;
            }

            let old_hash = node.file_hash.load(Ordering::Relaxed);
            let new_hash = hash_path_id(&new_rel);

            {
                let mut inner = self.global_cache.inner.lock();
                let mut freed_bytes = 0usize;
                inner.entries.retain(|k, v| {
                    if k.file_hash == old_hash {
                        freed_bytes += v.read().data.len();
                        false
                    } else {
                        true
                    }
                });
                inner.current_bytes = inner.current_bytes.saturating_sub(freed_bytes);
            }

            if node.is_striped {
                let old_m = self.meta_path(&old_rel);
                let new_m = self.meta_path(&new_rel);
                if let Some(parent) = new_m.parent() {
                    let _ = fs::create_dir_all(parent);
                }

                let mut file_size = *node.file_size.lock();
                if file_size == 0
                    && let Ok(s) = fs::read_to_string(&old_m)
                {
                    file_size = s.trim().parse::<u64>().unwrap_or(0);
                }

                if old_m.exists() {
                    let _ = fs::rename(&old_m, &new_m);
                }

                let stripe_size = self.stripe_config.stripe_size_bytes;
                let num_parts = (file_size + stripe_size.saturating_sub(1)) / stripe_size.max(1);

                for part_index in 0..=(num_parts + 2) {
                    let old_drive_idx = self.stripe_config.map_part_to_drive(old_hash, part_index);
                    let new_drive_idx = self.stripe_config.map_part_to_drive(new_hash, part_index);

                    for ext in ["lz4", "zst", "raw"] {
                        let (old_store, old_legacy) =
                            self.resolve_part_paths(old_drive_idx, &old_rel, part_index, ext);
                        let (new_store, _new_legacy) =
                            self.resolve_part_paths(new_drive_idx, &new_rel, part_index, ext);

                        for p in [old_store, old_legacy] {
                            if p.exists() {
                                if let Some(parent) = new_store.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                if fs::rename(&p, &new_store).is_err()
                                    && fs::copy(&p, &new_store).is_ok()
                                {
                                    let _ = fs::remove_file(&p);
                                }
                            }
                        }
                    }
                }

                let mut meta = self.meta_sizes.write();
                if let Some(sz) = meta.remove(&old_rel.to_lowercase()) {
                    meta.insert(new_rel.to_lowercase(), sz);
                }
            } else {
                for &member_idx in &node.present_on {
                    let _spindle_guard = self.spindle_throttles[member_idx].acquire();
                    let old_f = self.members[member_idx].join(&old_rel);
                    let new_f = self.members[member_idx].join(&new_rel);
                    if let Some(parent) = new_f.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    if old_f.exists() {
                        let _ = fs::rename(old_f, new_f);
                    }
                }
            }

            *node.rel_path.write() = new_rel.clone();
            node.file_hash.store(new_hash, Ordering::Relaxed);

            {
                let mut map = self.file_nodes.write();
                if let Some(existing_node) = map.remove(&old_rel.to_lowercase()) {
                    map.insert(new_rel.to_lowercase(), existing_node);
                }
            }
        }
        *ctx.rel_path.write() = new_rel;
        Ok(())
    }
}

fn context_node(ctx: &PoolFileHandle) -> Result<&Arc<FileNode>, FspError> {
    ctx.node
        .as_ref()
        .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))
}

struct FileTimes {
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
}

fn get_metadata_times(meta: &fs::Metadata) -> FileTimes {
    FileTimes {
        creation_time: meta.creation_time(),
        last_access_time: meta.last_access_time(),
        last_write_time: meta.last_write_time(),
        change_time: meta.last_write_time(),
    }
}

fn get_current_filetime() -> u64 {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_secs() + 11644473600) * 10_000_000 + (dur.subsec_nanos() as u64 / 100)
}
