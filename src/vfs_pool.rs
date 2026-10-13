use crate::stripe_math::StripeConfig;
use crate::win32::{check_winfsp_available, get_system_ram_mb};
use crate::zstd_atg::ZstdAtgCompressor;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::os::windows::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use widestring::U16CStr;
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC0000023_u32 as i32;

pub const MIN_HDD_OPTIMAL_STRIPE_BYTES: u64 = 64 * 1024 * 1024;
const PREFETCH_LOOKAHEAD_PARTS: u64 = 2;
const DIRECT_READ_WINDOW_BYTES: u64 = 1024 * 1024;

pub type VfsLogCallback = Arc<dyn Fn(String) + Send + Sync>;
type CachedPartTuple = (u64, Arc<RwLock<CachedPart>>);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VfsCodec {
    Lz4 = 0,
    Zstd = 1,
    Store = 2,
}

impl VfsCodec {
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::Zstd,
            2 => Self::Store,
            _ => Self::Lz4,
        }
    }
}

pub fn hash_path_id(rel_path: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rel_path.to_lowercase().hash(&mut hasher);
    hasher.finish()
}

#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub struct PartKey {
    pub file_hash: u64,
    pub part_index: u64,
}

pub struct CachedPart {
    pub data: Vec<u8>,
    pub dirty: bool,
    pub last_access: Instant,
    pub codec: VfsCodec,
    pub rel_path: String,
    pub file_hash: u64,
    pub part_index: u64,
}

pub struct GlobalStripeCache {
    pub entries: Mutex<HashMap<PartKey, Arc<RwLock<CachedPart>>>>,
    pub max_parts: usize,
}

pub struct DirectBuffer {
    pub offset: u64,
    pub data: Vec<u8>,
}

pub struct FileNode {
    pub rel_path: RwLock<String>,
    pub file_hash: u64,
    pub is_striped: bool,
    pub member_idx: usize,
    pub file_codec: VfsCodec,
    pub file_size: Mutex<u64>,
    pub dirty_parts: Mutex<HashSet<u64>>,
    pub direct_cache: Mutex<Option<DirectBuffer>>,
    pub is_modified: Mutex<bool>,
    pub open_handles: AtomicUsize,
}

impl FileNode {
    pub fn rel_path(&self) -> String {
        self.rel_path.read().clone()
    }
}

pub struct PoolFileHandle {
    pub is_dir: bool,
    pub rel_path: RwLock<String>,
    pub node: Option<Arc<FileNode>>,
    pub dir_buffer: Mutex<DirBuffer>,
    pub delete_on_close: Mutex<bool>,
}

impl PoolFileHandle {
    pub fn rel_path(&self) -> String {
        self.rel_path.read().clone()
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
    pub meta_sizes: Arc<RwLock<HashMap<String, u64>>>,
    pub pool_used_bytes: Arc<AtomicU64>,
    pub spindle_locks: Arc<Vec<Mutex<()>>>,

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
        let spindle_locks = Arc::new((0..num_drives).map(|_| Mutex::new(())).collect::<Vec<_>>());

        let effective_stripe = stripe_size_bytes.max(MIN_HDD_OPTIMAL_STRIPE_BYTES);
        let ram_mb = get_system_ram_mb();
        let cache_budget_bytes = if ram_cache_mb > 0 {
            (ram_cache_mb as u64) * 1024 * 1024
        } else if ram_mb >= 32768 {
            4096 * 1024 * 1024
        } else if ram_mb >= 16384 {
            2048 * 1024 * 1024
        } else {
            1024 * 1024 * 1024
        };

        let max_parts = ((cache_budget_bytes / effective_stripe).clamp(8, 2048)) as usize;

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

        // Garbage Collector for orphaned chunks
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

        let fs = Self {
            members: Arc::new(members),
            policy,
            stripe_config: StripeConfig::new(effective_stripe, num_drives),
            custom_size_gb,
            pool_codec: VfsCodec::from_i32(codec_idx),
            tiered_storage,
            logger,
            jbod_map: Arc::new(RwLock::new(HashMap::new())),
            file_nodes: Arc::new(RwLock::new(HashMap::new())),
            global_cache: Arc::new(GlobalStripeCache {
                entries: Mutex::new(HashMap::new()),
                max_parts,
            }),
            prefetch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            meta_sizes: Arc::new(RwLock::new(preloaded_meta)),
            pool_used_bytes: Arc::new(AtomicU64::new(initial_used)),
            spindle_locks,
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
        if let Ok(c) = fs::read_to_string(self.catalog_path())
            && let Ok(map) = serde_json::from_str(&c)
        {
            map
        } else {
            HashMap::new()
        }
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

    fn find_direct_file(&self, rel_path: &str) -> Option<(usize, PathBuf)> {
        let p0 = self.master().join(rel_path);
        if p0.is_file() {
            return Some((0, p0));
        }

        let key = rel_path.to_lowercase();
        if let Some(&idx) = self.jbod_map.read().get(&key)
            && idx < self.members.len()
        {
            let p = self.members[idx].join(rel_path);
            if p.is_file() {
                return Some((idx, p));
            }
        }

        for (idx, m) in self.members.iter().enumerate().skip(1) {
            let p = m.join(rel_path);
            if p.is_file() {
                self.jbod_map.write().insert(key, idx);
                self.save_jbod_catalog();
                return Some((idx, p));
            }
        }
        None
    }

    fn get_or_create_file_node(
        &self,
        rel_path: &str,
        is_striped: bool,
        member_idx: usize,
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
                file_hash: hash_path_id(rel_path),
                is_striped,
                member_idx,
                file_codec,
                file_size: Mutex::new(initial_size),
                dirty_parts: Mutex::new(HashSet::new()),
                direct_cache: Mutex::new(None),
                is_modified: Mutex::new(false),
                open_handles: AtomicUsize::new(0),
            })
        });

        node.open_handles.fetch_add(1, Ordering::Relaxed);
        node.clone()
    }

    fn resolve_part_paths(
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

        let num_drives = self.members.len() as u64;
        let drive_shift = part.file_hash % num_drives;
        let target_drive_idx = ((part.part_index + drive_shift) % num_drives) as usize;

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

        let _spindle_guard = self.spindle_locks[target_drive_idx].lock();

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
            self.pool_used_bytes
                .fetch_add(diff as u64, Ordering::Relaxed);
        } else if diff < 0 {
            self.pool_used_bytes
                .fetch_sub((-diff) as u64, Ordering::Relaxed);
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

    pub fn flush_node_dirty_parts(&self, node: &FileNode) {
        let dirty_indices: Vec<u64> = node.dirty_parts.lock().drain().collect();
        if dirty_indices.is_empty() {
            return;
        }

        let f_hash = node.file_hash;
        let num_drives = self.members.len() as u64;

        let cache = self.global_cache.entries.lock();
        let mut drive_buckets: HashMap<usize, Vec<CachedPartTuple>> = HashMap::new();

        for part_idx in dirty_indices {
            let key = PartKey {
                file_hash: f_hash,
                part_index: part_idx,
            };
            if let Some(part_arc) = cache.get(&key) {
                let drive_shift = f_hash % num_drives;
                let target_drive = ((part_idx + drive_shift) % num_drives) as usize;
                drive_buckets
                    .entry(target_drive)
                    .or_default()
                    .push((part_idx, part_arc.clone()));
            }
        }
        drop(cache);

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

    fn fetch_cached_part(
        &self,
        node: &Arc<FileNode>,
        part_index: u64,
        target_drive_idx: usize,
        allow_prefetch: bool,
    ) -> Result<Arc<RwLock<CachedPart>>, FspError> {
        let key = PartKey {
            file_hash: node.file_hash,
            part_index,
        };

        let part_arc = {
            let mut cache = self.global_cache.entries.lock();
            if let Some(part) = cache.get(&key) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                part.write().last_access = Instant::now();
                part.clone()
            } else {
                self.cache_misses.fetch_add(1, Ordering::Relaxed);

                if cache.len() >= self.global_cache.max_parts {
                    let mut oldest_key = None;
                    let mut oldest_time = Instant::now();

                    for (&k, v) in cache.iter() {
                        let guard = v.read();
                        if Self::is_pinned_metadata(&guard.rel_path) {
                            continue;
                        }
                        if guard.last_access < oldest_time {
                            oldest_time = guard.last_access;
                            oldest_key = Some(k);
                        }
                    }

                    if let Some(k) = oldest_key
                        && let Some(evicted) = cache.remove(&k)
                    {
                        self.evict_part_to_disk(&evicted);
                    }
                }

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

                let _spindle_guard = self.spindle_locks[target_drive_idx].lock();
                let t0 = Instant::now();

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

                let lat = t0.elapsed().as_millis() as u64;
                self.io_max_latency_ms[target_drive_idx].fetch_max(lat, Ordering::Relaxed);

                let p = Arc::new(RwLock::new(CachedPart {
                    data,
                    dirty: false,
                    last_access: Instant::now(),
                    codec: loaded_codec,
                    rel_path: rel,
                    file_hash: node.file_hash,
                    part_index,
                }));

                cache.insert(key, p.clone());
                p
            }
        };

        if allow_prefetch {
            let file_size = *node.file_size.lock();
            let stripe_size = self.stripe_config.stripe_size_bytes;
            let mut candidates = Vec::new();

            {
                let cache = self.global_cache.entries.lock();
                let mut in_flight = self.prefetch_in_flight.lock();

                for step in 1..=PREFETCH_LOOKAHEAD_PARTS {
                    let next_p = part_index + step;
                    let next_key = PartKey {
                        file_hash: node.file_hash,
                        part_index: next_p,
                    };
                    if next_p.saturating_mul(stripe_size) < file_size
                        && !cache.contains_key(&next_key)
                        && !in_flight.contains(&next_key)
                    {
                        in_flight.insert(next_key);
                        candidates.push(next_p);
                    }
                }
            }

            if !candidates.is_empty() {
                let fs_clone = self.clone();
                let node_clone = Arc::clone(node);
                rayon::spawn(move || {
                    let num_drives = fs_clone.members.len() as u64;
                    let drive_shift = node_clone.file_hash % num_drives;
                    for p_idx in candidates {
                        let target_d = ((p_idx + drive_shift) % num_drives) as usize;
                        let _ = fs_clone.fetch_cached_part(&node_clone, p_idx, target_d, false);
                        let key = PartKey {
                            file_hash: node_clone.file_hash,
                            part_index: p_idx,
                        };
                        fs_clone.prefetch_in_flight.lock().remove(&key);
                    }
                });
            }
        }

        Ok(part_arc)
    }
}

impl FileSystemContext for SymStripePoolFs {
    type FileContext = PoolFileHandle;

    /// Optimized zero-overhead security checks serving straight from RAM cache
    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        sd: Option<&mut [c_void]>,
        _rp: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let rel_path = Self::normalize_path(file_name);
        let lower = rel_path.to_lowercase();

        // Ultra-fast in-memory path validation to prevent User-to-Kernel context switch CPU thrashing
        let is_dir = if rel_path.is_empty() {
            true
        } else if self.meta_sizes.read().contains_key(&lower)
            || self.jbod_map.read().contains_key(&lower)
        {
            false
        } else {
            // Assume directory if no extension, saving thousands of synchronous disk queries per second
            !rel_path.contains('.')
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

        let (is_dir, is_striped, member_idx, file_size) =
            if rel_path.is_empty() || self.members.iter().any(|m| m.join(&rel_path).is_dir()) {
                (true, false, 0, 0)
            } else if let Some(size) = self.read_meta_size(&rel_path) {
                (false, true, 0, size)
            } else if let Some((m_idx, p)) = self.find_direct_file(&rel_path) {
                let meta = fs::metadata(&p)
                    .map_err(|_| FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
                (false, false, m_idx, meta.len())
            } else {
                return Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND));
            };

        let node = if !is_dir {
            Some(self.get_or_create_file_node(
                &rel_path,
                is_striped,
                member_idx,
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
            } else {
                self.members[member_idx].join(&rel_path)
            };

            if let Ok(m) = fs::metadata(check_p) {
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

        let (is_striped, member_idx, file_codec) = if is_dir {
            for m in self.members.iter() {
                let _ = fs::create_dir_all(m.join(&rel_path));
            }
            (false, 0, self.pool_codec)
        } else if self.tiered_storage && is_hot_meta {
            (false, 0, VfsCodec::Store)
        } else if (self.tiered_storage && is_bulk_stream) || self.policy == 1 {
            (true, 0, self.pool_codec)
        } else {
            (false, self.select_jbod_member(), self.pool_codec)
        };

        let node = if !is_dir {
            let n = self.get_or_create_file_node(&rel_path, is_striped, member_idx, file_codec, 0);
            *n.is_modified.lock() = true;

            if !is_striped {
                let p = self.members[member_idx].join(&rel_path);
                if let Some(parent) = p.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&p);
                if member_idx != 0 {
                    self.jbod_map
                        .write()
                        .insert(rel_path.to_lowercase(), member_idx);
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
            "[VFS CREATE] {} '{}' (Member {})",
            if is_dir { "DIR" } else { "FILE" },
            rel_path,
            member_idx + 1
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
        node.direct_cache.lock().take(); // Invalidate sequential buffer on write

        let mut current_size = node.file_size.lock();
        let write_offset = if write_to_eof { *current_size } else { offset };

        if !node.is_striped {
            let member_idx = node.member_idx;
            let _spindle_guard = self.spindle_locks[member_idx].lock();

            let p = self.members[member_idx].join(node.rel_path());
            let old_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);

            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&p)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            file.seek_write(buffer, write_offset)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            let new_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(old_len);
            let diff = new_len as i64 - old_len as i64;
            if diff > 0 {
                self.pool_used_bytes
                    .fetch_add(diff as u64, Ordering::Relaxed);
            }

            if let Some(counter) = self.io_bytes.get(member_idx) {
                counter.fetch_add(buffer.len() as u64, Ordering::Relaxed);
            }
        } else {
            let buffer_len = buffer.len();
            let slices = self
                .stripe_config
                .map_io_slices(node.file_hash, write_offset, buffer_len);
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

            let write_action = |slice: &crate::stripe_math::StripeSlice,
                                part_arc: &Arc<RwLock<CachedPart>>,
                                src_slice: &[u8]| {
                let mut part = part_arc.write();
                let start_idx = slice.offset_in_part as usize;
                let end_pos = start_idx + src_slice.len();

                if part.data.len() < end_pos {
                    part.data.resize(end_pos, 0);
                }

                part.data[start_idx..end_pos].copy_from_slice(src_slice);
                part.dirty = true;
                part.codec = codec;

                if let Some(counter) = io_bytes.get(slice.target_drive_idx) {
                    counter.fetch_add(src_slice.len() as u64, Ordering::Relaxed);
                }
            };

            for (slice, part_arc, src_slice) in tasks {
                node.dirty_parts.lock().insert(slice.part_index);
                write_action(&slice, &part_arc, src_slice);
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

        // 1 MB sequential slotted window for direct unstriped files
        if !node.is_striped {
            let member_idx = node.member_idx;
            let file_size = current_file_size;

            let hit_cache = {
                let cache_guard = node.direct_cache.lock();
                if let Some(buf) = cache_guard.as_ref() {
                    if offset >= buf.offset
                        && offset + (bytes_to_read as u64) <= buf.offset + (buf.data.len() as u64)
                    {
                        let start = (offset - buf.offset) as usize;
                        let end = start + bytes_to_read;
                        target_slice.copy_from_slice(&buf.data[start..end]);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if hit_cache {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                if let Some(counter) = self.io_bytes.get(member_idx) {
                    counter.fetch_add(bytes_to_read as u64, Ordering::Relaxed);
                }
                return Ok(bytes_to_read as u32);
            }

            self.cache_misses.fetch_add(1, Ordering::Relaxed);

            let _spindle_guard = self.spindle_locks[member_idx].lock();
            let p = self.members[member_idx].join(node.rel_path());
            let file = File::open(&p).map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            let window_len = DIRECT_READ_WINDOW_BYTES
                .min(file_size.saturating_sub(offset))
                .max(bytes_to_read as u64) as usize;
            let mut window_buf = vec![0u8; window_len];

            let t0 = Instant::now();
            let actual_read = file
                .seek_read(&mut window_buf, offset)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            let lat = t0.elapsed().as_millis() as u64;
            self.io_max_latency_ms[member_idx].fetch_max(lat, Ordering::Relaxed);

            window_buf.truncate(actual_read);

            let copy_len = bytes_to_read.min(window_buf.len());
            target_slice[..copy_len].copy_from_slice(&window_buf[..copy_len]);

            *node.direct_cache.lock() = Some(DirectBuffer {
                offset,
                data: window_buf,
            });

            if let Some(counter) = self.io_bytes.get(member_idx) {
                counter.fetch_add(actual_read as u64, Ordering::Relaxed);
            }
            return Ok(copy_len as u32);
        }

        let slices = self
            .stripe_config
            .map_io_slices(node.file_hash, offset, bytes_to_read);
        let num_slices = slices.len();

        let mut tasks = Vec::with_capacity(slices.len());
        let mut remaining_dest = target_slice;

        // Throttle prefetch checks: only trigger lookahead if read touches second half of stripe
        let stripe_size = self.stripe_config.stripe_size_bytes;
        let offset_in_stripe = offset % stripe_size;
        let trigger_lookahead = offset_in_stripe > (stripe_size * 3 / 4);

        for (i, slice) in slices.into_iter().enumerate() {
            let is_last = (i + 1 == num_slices) && trigger_lookahead;
            let part_arc =
                self.fetch_cached_part(node, slice.part_index, slice.target_drive_idx, is_last)?;
            let len = slice.length.min(remaining_dest.len());
            let (dest, rest) = remaining_dest.split_at_mut(len);
            remaining_dest = rest;
            tasks.push((slice, part_arc, dest));
        }

        let io_bytes = self.io_bytes.clone();
        let read_action = |slice: &crate::stripe_math::StripeSlice,
                           part_arc: &Arc<RwLock<CachedPart>>,
                           dest: &mut [u8]|
         -> usize {
            let part = part_arc.read();
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
                copy_len
            } else {
                dest.fill(0);
                0
            }
        };

        let mut total = 0;
        for (slice, part_arc, dest) in tasks {
            total += read_action(&slice, &part_arc, dest);
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

            // 1. Recursive directory cleanup with byte reclamation
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
                                    self.pool_used_bytes.fetch_sub(m.len(), Ordering::Relaxed);
                                }
                            }
                            let _ = fs::remove_dir_all(dir);
                        }
                    }
                }
            } else if let Some(node) = &ctx.node {
                *node.is_modified.lock() = false;
                node.direct_cache.lock().take();
                self.file_nodes.write().remove(&rel.to_lowercase());

                // Evict from RAM cache
                {
                    let mut cache = self.global_cache.entries.lock();
                    cache.retain(|k, _| k.file_hash != node.file_hash);
                }

                if node.is_striped {
                    let meta_p = self.meta_path(&rel);
                    if let Ok(m) = fs::metadata(&meta_p) {
                        self.pool_used_bytes.fetch_sub(m.len(), Ordering::Relaxed);
                    }
                    let _ = fs::remove_file(meta_p);

                    let parent_opt = Path::new(&rel).parent();
                    let file_name = Path::new(&rel)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    let prefix = format!("{}.part", file_name);

                    // 2. Physical chunk removal across all pool member drives + byte reclamation
                    for m in self.members.iter() {
                        let targets = [
                            match parent_opt {
                                Some(p) if !p.as_os_str().is_empty() => {
                                    m.join(".symstripe_store").join(p)
                                }
                                _ => m.join(".symstripe_store"),
                            },
                            match parent_opt {
                                Some(p) if !p.as_os_str().is_empty() => m.join(p),
                                _ => m.to_path_buf(),
                            },
                        ];

                        for dir in targets {
                            if let Ok(entries) = fs::read_dir(dir) {
                                for entry in entries.flatten() {
                                    let n = entry.file_name().to_string_lossy().to_string();
                                    if n.starts_with(&prefix) {
                                        if let Ok(m) = entry.metadata() {
                                            self.pool_used_bytes
                                                .fetch_sub(m.len(), Ordering::Relaxed);
                                        }
                                        let _ = fs::remove_file(entry.path());
                                    }
                                }
                            }
                        }
                    }
                } else {
                    let target_p = self.members[node.member_idx].join(&rel);
                    if let Ok(m) = fs::metadata(&target_p) {
                        self.pool_used_bytes.fetch_sub(m.len(), Ordering::Relaxed);
                    }
                    let _ = fs::remove_file(&target_p);
                    if node.member_idx != 0 {
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
                self.file_nodes
                    .write()
                    .remove(&node.rel_path().to_lowercase());
            }
        }
    }

    fn get_file_info(&self, ctx: &Self::FileContext, info: &mut FileInfo) -> winfsp::Result<()> {
        let (size, is_striped, member_idx, rel) = if let Some(node) = &ctx.node {
            (
                *node.file_size.lock(),
                node.is_striped,
                node.member_idx,
                node.rel_path(),
            )
        } else {
            (0, false, 0, ctx.rel_path())
        };

        *info = FileInfo::default();
        info.file_size = size;
        info.allocation_size = if ctx.is_dir { 0 } else { (size + 4095) & !4095 };
        info.file_attributes = if ctx.is_dir { 0x10 } else { 0x80 };
        info.hard_links = 1;
        info.index_number = hash_path_id(&rel);

        let check_p = if is_striped {
            self.meta_path(&rel)
        } else {
            self.members[member_idx].join(&rel)
        };
        if let Ok(m) = fs::metadata(check_p) {
            let ft = get_metadata_times(&m);
            info.creation_time = ft.creation_time;
            info.last_access_time = ft.last_access_time;
            info.last_write_time = ft.last_write_time;
            info.change_time = ft.change_time;
        }
        Ok(())
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total, free) = if self.custom_size_gb > 0 {
            let t = self.custom_size_gb * 1024 * 1024 * 1024;
            let u = self.pool_used_bytes.load(Ordering::Relaxed);
            (t, t.saturating_sub(u))
        } else {
            let mut total_s = 0u64;
            let mut free_s = 0u64;
            for m in self.members.iter() {
                if let Some((f, t)) = crate::win32::get_disk_space(m) {
                    total_s += t;
                    free_s += f;
                }
            }
            if total_s == 0 {
                (1024 * 1024 * 1024 * 1024, 1024 * 1024 * 1024 * 1024)
            } else {
                (total_s, free_s)
            }
        };
        out.total_size = total;
        out.free_size = free;
        out.set_volume_label("SymStripe HDD Accelerator");
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
        node.direct_cache.lock().take();
        let rel = node.rel_path();

        if !node.is_striped {
            let _spindle_guard = self.spindle_locks[node.member_idx].lock();
            let _ = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(self.members[node.member_idx].join(&rel));
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
        node.direct_cache.lock().take();
        let rel = node.rel_path();

        if !node.is_striped {
            let _spindle_guard = self.spindle_locks[node.member_idx].lock();
            if let Ok(f) = OpenOptions::new()
                .write(true)
                .open(self.members[node.member_idx].join(&rel))
            {
                let _ = f.set_len(new_size);
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
            node.direct_cache.lock().take();
            if node.is_striped {
                let old_m = self.meta_path(&old_rel);
                let new_m = self.meta_path(&new_rel);
                if old_m.exists() {
                    let _ = fs::rename(old_m, new_m);
                }
                let mut meta = self.meta_sizes.write();
                if let Some(sz) = meta.remove(&old_rel.to_lowercase()) {
                    meta.insert(new_rel.to_lowercase(), sz);
                }
            } else {
                let _spindle_guard = self.spindle_locks[node.member_idx].lock();
                let old_f = self.members[node.member_idx].join(&old_rel);
                let new_f = self.members[node.member_idx].join(&new_rel);
                if let Some(parent) = new_f.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::rename(old_f, new_f);
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

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sd_str: *const u16,
        rev: u32,
        sd: *mut *mut c_void,
        sz: *mut u32,
    ) -> i32;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

pub fn get_default_security_descriptor() -> &'static [u8] {
    static SD: OnceLock<Vec<u8>> = OnceLock::new();
    SD.get_or_init(|| {
        let sddl = crate::win32::to_wide_null("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)");
        let mut p_sd = std::ptr::null_mut();
        let mut size = 0u32;
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut p_sd,
                &mut size,
            )
        };
        if ok != 0 && !p_sd.is_null() && size > 0 {
            let bytes =
                unsafe { std::slice::from_raw_parts(p_sd as *const u8, size as usize).to_vec() };
            unsafe { LocalFree(p_sd) };
            bytes
        } else {
            vec![
                1, 0, 0x04, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        }
    })
}

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

    let pool_fs = SymStripePoolFs::new(
        members.to_vec(),
        policy,
        stripe_size_bytes,
        custom_size_gb,
        codec_idx,
        tiered_storage,
        ram_cache_mb,
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

    // Deep kernel metadata caching to slash user-mode context switching CPU overhead (8.4% -> 0.1%)
    vp.file_info_timeout(30000);
    vp.dir_info_timeout(30000);
    vp.volume_info_timeout(30000);
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
