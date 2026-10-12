use crate::stripe_math::StripeConfig;
use crate::win32::{check_winfsp_available, get_system_ram_mb};
use crate::zstd_atg::ZstdAtgCompressor;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::os::windows::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use widestring::U16CStr;
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC0000022_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC0000023_u32 as i32;

// Threshold for switching from sequential thread execution to Rayon parallel work-stealing (2 MB)
const PARALLEL_IO_THRESHOLD_BYTES: usize = 2 * 1024 * 1024;

pub type VfsLogCallback = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
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

fn hash_path_id(rel_path: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rel_path.to_lowercase().hash(&mut hasher);
    hasher.finish()
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        StringSecurityDescriptor: *const u16,
        StringSDRevision: u32,
        SecurityDescriptor: *mut *mut c_void,
        SecurityDescriptorSize: *mut u32,
    ) -> i32;
    fn LocalFree(hMem: *mut c_void) -> *mut c_void;
}

fn get_default_security_descriptor() -> &'static [u8] {
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

struct FileTimes {
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
}

fn get_metadata_times(meta: &fs::Metadata) -> FileTimes {
    let ct = meta.creation_time();
    let lat = meta.last_access_time();
    let lwt = meta.last_write_time();
    FileTimes {
        creation_time: ct,
        last_access_time: lat,
        last_write_time: lwt,
        change_time: lwt,
    }
}

fn get_current_filetime() -> u64 {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_secs() + 11644473600) * 10_000_000 + (dur.subsec_nanos() as u64 / 100)
}

/// In-Memory Accelerated Cache Part
pub struct CachedPart {
    pub data: Vec<u8>,
    pub dirty: bool,
    pub last_access: std::time::Instant,
    pub last_modified: std::time::Instant,
    pub codec: VfsCodec,
}

/// File Control Block (FCB) — global file node shared across all open handles
pub struct FileNode {
    pub rel_path: RwLock<String>,
    pub is_striped: bool,
    pub member_idx: usize,
    pub file_codec: VfsCodec,
    pub file_size: Mutex<u64>,
    pub direct_file: RwLock<Option<Arc<File>>>,
    pub cached_parts: Mutex<HashMap<u64, Arc<RwLock<CachedPart>>>>,
    pub is_modified: Mutex<bool>,
    pub open_handles: AtomicUsize,
}

impl FileNode {
    pub fn rel_path(&self) -> String {
        self.rel_path.read().clone()
    }

    pub fn get_or_open_direct_file(
        &self,
        member_root: &Path,
        for_write: bool,
    ) -> Result<Arc<File>, FspError> {
        {
            let guard = self.direct_file.read();
            if let Some(f) = guard.as_ref() {
                return Ok(f.clone());
            }
        }

        let mut guard = self.direct_file.write();
        if let Some(f) = guard.as_ref() {
            return Ok(f.clone());
        }

        let rel = self.rel_path();
        let p = member_root.join(&rel);
        let mut opts = OpenOptions::new();
        opts.read(true);
        if for_write {
            opts.write(true).create(true).truncate(false);
        }

        let file = opts
            .open(&p)
            .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        let arc_file = Arc::new(file);
        *guard = Some(arc_file.clone());
        Ok(arc_file)
    }
}

/// Context Control Block (CCB) — per-open-handle context for WinFsp
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
    pub usage_cache: Arc<Mutex<(std::time::Instant, u64)>>,
    pub jbod_map: Arc<RwLock<HashMap<String, usize>>>,

    // FCB Registry: single global node map to prevent cache desynchronization
    pub file_nodes: Arc<RwLock<HashMap<String, Arc<FileNode>>>>,
    pub max_cache_parts: usize,

    // Telemetry Trackers
    pub io_bytes: Arc<Vec<AtomicU64>>,
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
        logger: Option<VfsLogCallback>,
    ) -> Self {
        let num_drives = members.len().max(1);
        let io_bytes = Arc::new(
            (0..num_drives)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>(),
        );

        let ram_mb = get_system_ram_mb();
        let cache_budget_bytes = if ram_mb >= 32768 {
            1024 * 1024 * 1024
        } else if ram_mb >= 16384 {
            512 * 1024 * 1024
        } else {
            256 * 1024 * 1024
        };

        let max_cache_parts =
            ((cache_budget_bytes / stripe_size_bytes.max(1)).clamp(16, 512)) as usize;

        let fs = Self {
            members: Arc::new(members),
            policy,
            stripe_config: StripeConfig::new(stripe_size_bytes, num_drives),
            custom_size_gb,
            pool_codec: VfsCodec::from_i32(codec_idx),
            tiered_storage,
            logger,
            usage_cache: Arc::new(Mutex::new((
                std::time::Instant::now() - std::time::Duration::from_secs(10),
                0,
            ))),
            jbod_map: Arc::new(RwLock::new(HashMap::new())),
            file_nodes: Arc::new(RwLock::new(HashMap::new())),
            max_cache_parts,
            io_bytes,
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
        let string_path = raw_path.to_string_lossy();
        string_path.trim_start_matches('\\').to_string()
    }

    fn master(&self) -> &Path {
        &self.members[0]
    }

    fn meta_path(&self, rel_path: &str) -> PathBuf {
        self.master().join(format!("{}.symstripe_meta", rel_path))
    }

    fn read_meta_size(&self, rel_path: &str) -> Option<u64> {
        fs::read_to_string(self.meta_path(rel_path))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    fn save_meta_size(&self, rel_path: &str, size: u64) {
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
                is_striped,
                member_idx,
                file_codec,
                file_size: Mutex::new(initial_size),
                direct_file: RwLock::new(None),
                cached_parts: Mutex::new(HashMap::new()),
                is_modified: Mutex::new(false),
                open_handles: AtomicUsize::new(0),
            })
        });

        node.open_handles.fetch_add(1, Ordering::Relaxed);
        node.clone()
    }

    fn calculate_pool_used_bytes(&self) -> u64 {
        let mut total_used: u64 = 0;
        for member in self.members.iter() {
            for entry in walkdir::WalkDir::new(member)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    total_used += meta.len();
                }
            }
        }
        total_used
    }

    fn evict_part_to_disk(
        &self,
        rel_path: &str,
        part_index: u64,
        part_arc: Arc<RwLock<CachedPart>>,
    ) {
        let mut part = part_arc.write();
        if part.dirty {
            let target_drive_idx = (part_index as usize) % self.members.len();
            let ext = match part.codec {
                VfsCodec::Lz4 => "lz4",
                VfsCodec::Zstd => "zst",
                VfsCodec::Store => "raw",
            };

            let part_filename = format!("{}.part{}.{}", rel_path, part_index, ext);
            let base_path = self.members[target_drive_idx].join(part_filename);
            if let Some(parent) = base_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let compressed = match part.codec {
                VfsCodec::Lz4 => lz4_flex::compress_prepend_size(&part.data),
                VfsCodec::Zstd => ZstdAtgCompressor::compress_atg(&part.data, 3)
                    .unwrap_or_else(|_| part.data.clone()),
                VfsCodec::Store => part.data.clone(),
            };

            self.raw_bytes_written
                .fetch_add(part.data.len() as u64, Ordering::Relaxed);
            self.compressed_bytes_written
                .fetch_add(compressed.len() as u64, Ordering::Relaxed);
            let _ = fs::write(&base_path, compressed);
            part.dirty = false;
        }
    }

    pub fn flush_node_dirty_parts(&self, node: &FileNode) {
        let rel = node.rel_path();
        let parts: Vec<(u64, Arc<RwLock<CachedPart>>)> = node
            .cached_parts
            .lock()
            .iter()
            .map(|(&idx, arc)| (idx, arc.clone()))
            .collect();

        for (part_index, part_arc) in parts {
            self.evict_part_to_disk(&rel, part_index, part_arc);
        }
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
        node: &FileNode,
        part_index: u64,
        target_drive_idx: usize,
    ) -> Result<Arc<RwLock<CachedPart>>, FspError> {
        let mut cache = node.cached_parts.lock();

        if let Some(part) = cache.get(&part_index) {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            part.write().last_access = std::time::Instant::now();
            return Ok(part.clone());
        }

        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        if cache.len() >= self.max_cache_parts {
            let mut lru_key = 0;
            let mut oldest = std::time::Instant::now();
            for (&k, v) in cache.iter() {
                let access = v.read().last_access;
                if access < oldest {
                    oldest = access;
                    lru_key = k;
                }
            }
            if let Some(evicted) = cache.remove(&lru_key) {
                self.evict_part_to_disk(&node.rel_path(), lru_key, evicted);
            }
        }

        let rel = node.rel_path();
        let lz4_path =
            self.members[target_drive_idx].join(format!("{}.part{}.lz4", rel, part_index));
        let zst_path =
            self.members[target_drive_idx].join(format!("{}.part{}.zst", rel, part_index));
        let raw_path =
            self.members[target_drive_idx].join(format!("{}.part{}.raw", rel, part_index));

        let mut data = Vec::new();
        let mut loaded_codec = node.file_codec;

        if lz4_path.exists() {
            if let Ok(comp) = fs::read(&lz4_path) {
                data = lz4_flex::decompress_size_prepended(&comp).unwrap_or_default();
                loaded_codec = VfsCodec::Lz4;
            }
        } else if zst_path.exists() {
            if let Ok(comp) = fs::read(&zst_path) {
                data = ZstdAtgCompressor::decompress_atg(&comp).unwrap_or_default();
                loaded_codec = VfsCodec::Zstd;
            }
        } else if raw_path.exists() {
            data = fs::read(&raw_path).unwrap_or_default();
            loaded_codec = VfsCodec::Store;
        }

        let now = std::time::Instant::now();
        let part = Arc::new(RwLock::new(CachedPart {
            data,
            dirty: false,
            last_access: now,
            last_modified: now,
            codec: loaded_codec,
        }));
        cache.insert(part_index, part.clone());
        Ok(part)
    }
}

impl FileSystemContext for SymStripePoolFs {
    type FileContext = PoolFileHandle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        sd: Option<&mut [c_void]>,
        _rp: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let rel_path = Self::normalize_path(file_name);
        let is_dir = if rel_path.is_empty()
            || self.members.iter().any(|m| m.join(&rel_path).is_dir())
        {
            true
        } else if self.find_direct_file(&rel_path).is_some() || self.meta_path(&rel_path).exists() {
            false
        } else {
            return Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND));
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
        _context: &Self::FileContext,
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

            let check_path = if is_striped {
                self.meta_path(&rel_path)
            } else {
                self.members[member_idx].join(&rel_path)
            };

            if let Ok(m) = fs::metadata(check_path) {
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
        let is_cold = lower_rel.ends_with(".bk2")
            || lower_rel.ends_with(".mp4")
            || lower_rel.ends_with(".wav")
            || lower_rel.ends_with(".ogg")
            || lower_rel.ends_with(".pak")
            || lower_rel.ends_with(".wem")
            || lower_rel.ends_with(".pck");
        let is_hot = lower_rel.ends_with(".exe")
            || lower_rel.ends_with(".dll")
            || lower_rel.ends_with(".sys")
            || lower_rel.ends_with(".ini")
            || lower_rel.ends_with(".cfg");

        let (is_striped, member_idx, file_codec) = if is_dir {
            for m in self.members.iter() {
                let _ = fs::create_dir_all(m.join(&rel_path));
            }
            (false, 0, self.pool_codec)
        } else if self.tiered_storage && is_cold {
            (false, self.members.len() - 1, VfsCodec::Zstd)
        } else if self.tiered_storage && is_hot {
            (false, 0, VfsCodec::Lz4)
        } else if self.policy == 1 {
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

    fn overwrite(
        &self,
        context: &Self::FileContext,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let node = context
            .node
            .as_ref()
            .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))?;

        *node.is_modified.lock() = true;
        *node.file_size.lock() = 0;

        let rel = node.rel_path();
        if !node.is_striped {
            let f = node.get_or_open_direct_file(&self.members[node.member_idx], true)?;
            let _ = f.set_len(0);
        } else {
            self.save_meta_size(&rel, 0);
            node.cached_parts.lock().clear();

            let parent_opt = Path::new(&rel).parent();
            let file_name = Path::new(&rel)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let prefix = format!("{}.part", file_name);

            for m in self.members.iter() {
                let dir = match parent_opt {
                    Some(p) if !p.as_os_str().is_empty() => m.join(p),
                    _ => m.to_path_buf(),
                };
                if let Ok(entries) = fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let n = entry.file_name().to_string_lossy().to_string();
                        if n.starts_with(&prefix) {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }

        let now_ft = get_current_filetime();
        *file_info = FileInfo::default();
        file_info.file_size = 0;
        file_info.allocation_size = 0;
        file_info.file_attributes = if replace_file_attributes {
            if file_attributes == 0 {
                0x80
            } else {
                file_attributes
            }
        } else {
            file_info.file_attributes | file_attributes
        };
        file_info.hard_links = 1;
        file_info.index_number = hash_path_id(&rel);
        file_info.last_write_time = now_ft;
        file_info.change_time = now_ft;

        self.log(format!("[VFS OVERWRITE] '{}' truncated to 0 bytes", rel));
        Ok(())
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
            let member_root = &self.members[node.member_idx];
            let file = node.get_or_open_direct_file(member_root, true)?;
            file.seek_write(buffer, write_offset)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            if let Some(counter) = self.io_bytes.get(node.member_idx) {
                counter.fetch_add(buffer.len() as u64, Ordering::Relaxed);
            }
        } else {
            let buffer_len = buffer.len();
            let slices = self.stripe_config.map_io_slices(write_offset, buffer_len);
            let mut tasks = Vec::with_capacity(slices.len());
            let mut remaining_buf = buffer;

            for slice in slices {
                let part_arc =
                    self.fetch_cached_part(node, slice.part_index, slice.target_drive_idx)?;
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
                part.last_modified = std::time::Instant::now();
                part.codec = codec;

                if let Some(counter) = io_bytes.get(slice.target_drive_idx) {
                    counter.fetch_add(src_slice.len() as u64, Ordering::Relaxed);
                }
            };

            if tasks.len() > 1 && buffer_len >= PARALLEL_IO_THRESHOLD_BYTES {
                tasks
                    .into_par_iter()
                    .for_each(|(slice, part_arc, src_slice)| {
                        write_action(&slice, &part_arc, src_slice);
                    });
            } else {
                for (slice, part_arc, src_slice) in tasks {
                    write_action(&slice, &part_arc, src_slice);
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

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let node = context
            .node
            .as_ref()
            .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))?;

        *node.is_modified.lock() = true;
        let mut current_size = node.file_size.lock();
        let old_size = *current_size;
        *current_size = new_size;

        let rel = node.rel_path();
        if !node.is_striped {
            let member_root = &self.members[node.member_idx];
            let file = node.get_or_open_direct_file(member_root, true)?;
            let _ = file.set_len(new_size);
        } else {
            self.save_meta_size(&rel, new_size);

            if new_size < old_size {
                let stripe_size = self.stripe_config.stripe_size_bytes;
                let num_drives = self.members.len();

                let old_max_part = old_size.saturating_sub(1) / stripe_size;
                let new_max_part = if new_size == 0 {
                    None
                } else {
                    Some((new_size - 1) / stripe_size)
                };

                let start_del_part = match new_max_part {
                    Some(p) => p + 1,
                    None => 0,
                };

                let mut part_cache = node.cached_parts.lock();

                for part_idx in start_del_part..=old_max_part {
                    part_cache.remove(&part_idx);
                    let target_drive_idx = (part_idx as usize) % num_drives;
                    for ext in ["lz4", "zst", "raw"] {
                        let part_filename = format!("{}.part{}.{}", rel, part_idx, ext);
                        let part_path = self.members[target_drive_idx].join(part_filename);
                        let _ = fs::remove_file(part_path);
                    }
                }

                if let Some(boundary_idx) = new_max_part {
                    let rem_len = ((new_size - 1) % stripe_size) + 1;
                    if let Some(part_arc) = part_cache.get(&boundary_idx) {
                        part_arc.write().data.truncate(rem_len as usize);
                        part_arc.write().dirty = true;
                    } else {
                        let target_drive_idx = (boundary_idx as usize) % num_drives;
                        let lz4_path = self.members[target_drive_idx]
                            .join(format!("{}.part{}.lz4", rel, boundary_idx));
                        let zst_path = self.members[target_drive_idx]
                            .join(format!("{}.part{}.zst", rel, boundary_idx));
                        let raw_path = self.members[target_drive_idx]
                            .join(format!("{}.part{}.raw", rel, boundary_idx));

                        if lz4_path.exists()
                            && let Ok(comp) = fs::read(&lz4_path)
                            && let Ok(mut dec) = lz4_flex::decompress_size_prepended(&comp)
                        {
                            dec.truncate(rem_len as usize);
                            let new_comp = lz4_flex::compress_prepend_size(&dec);
                            let _ = fs::write(&lz4_path, new_comp);
                        } else if zst_path.exists()
                            && let Ok(comp) = fs::read(&zst_path)
                            && let Ok(mut dec) = ZstdAtgCompressor::decompress_atg(&comp)
                        {
                            dec.truncate(rem_len as usize);
                            if let Ok(new_comp) = ZstdAtgCompressor::compress_atg(&dec, 3) {
                                let _ = fs::write(&zst_path, new_comp);
                            }
                        } else if raw_path.exists()
                            && let Ok(f) = OpenOptions::new().write(true).open(&raw_path)
                        {
                            let _ = f.set_len(rem_len);
                        }
                    }
                }
            }
        }

        file_info.file_size = new_size;
        file_info.allocation_size = (new_size + 4095) & !4095;
        file_info.file_attributes = 0x80;
        file_info.hard_links = 1;
        file_info.index_number = hash_path_id(&rel);

        Ok(())
    }

    fn set_basic_info(
        &self,
        ctx: &Self::FileContext,
        attr: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        file_info.file_attributes = if attr != 0 && attr != u32::MAX {
            attr
        } else if ctx.is_dir {
            0x10
        } else {
            0x80
        };
        file_info.hard_links = 1;
        file_info.index_number = hash_path_id(&ctx.rel_path());
        Ok(())
    }

    fn set_delete(
        &self,
        ctx: &Self::FileContext,
        _file_name: &U16CStr,
        del: bool,
    ) -> winfsp::Result<()> {
        *ctx.delete_on_close.lock() = del;
        Ok(())
    }

    fn cleanup(&self, ctx: &Self::FileContext, _name: Option<&U16CStr>, flags: u32) {
        let should_delete = (flags & 0x01 != 0) || *ctx.delete_on_close.lock();

        if should_delete {
            *ctx.delete_on_close.lock() = true;

            let rel = ctx.rel_path();
            if ctx.is_dir {
                for m in self.members.iter() {
                    let _ = fs::remove_dir_all(m.join(&rel));
                }
            } else if let Some(node) = &ctx.node {
                *node.is_modified.lock() = false;
                *node.direct_file.write() = None;
                node.cached_parts.lock().clear();

                self.file_nodes.write().remove(&rel.to_lowercase());

                if node.is_striped {
                    let _ = fs::remove_file(self.meta_path(&rel));

                    let parent_opt = Path::new(&rel).parent();
                    let file_name = Path::new(&rel)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    let prefix = format!("{}.part", file_name);

                    for m in self.members.iter() {
                        let dir = match parent_opt {
                            Some(p) if !p.as_os_str().is_empty() => m.join(p),
                            _ => m.to_path_buf(),
                        };
                        if let Ok(entries) = fs::read_dir(dir) {
                            for entry in entries.flatten() {
                                let n = entry.file_name().to_string_lossy().to_string();
                                if n.starts_with(&prefix) {
                                    let _ = fs::remove_file(entry.path());
                                }
                            }
                        }
                    }
                } else {
                    let _ = fs::remove_file(self.members[node.member_idx].join(&rel));
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

    fn rename(
        &self,
        ctx: &Self::FileContext,
        _old_name: &U16CStr,
        new_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let new_rel = Self::normalize_path(new_name);
        let old_rel = ctx.rel_path();

        if ctx.is_dir {
            for m in self.members.iter() {
                let old_d = m.join(&old_rel);
                let new_d = m.join(&new_rel);
                if old_d.exists() {
                    if let Some(parent) = new_d.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::rename(old_d, new_d);
                }
            }
        } else if let Some(node) = &ctx.node {
            node.cached_parts.lock().clear();

            if node.is_striped {
                let old_m = self.meta_path(&old_rel);
                let new_m = self.meta_path(&new_rel);
                if old_m.exists() {
                    if let Some(parent) = new_m.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::rename(old_m, new_m);
                }

                let old_fname = Path::new(&old_rel)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let new_fname = Path::new(&new_rel)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let prefix = format!("{}.part", old_fname);

                for m in self.members.iter() {
                    let old_dir = m.join(Path::new(&old_rel).parent().unwrap_or(Path::new("")));
                    let new_dir = m.join(Path::new(&new_rel).parent().unwrap_or(Path::new("")));
                    let _ = fs::create_dir_all(&new_dir);

                    if let Ok(entries) = fs::read_dir(old_dir) {
                        for entry in entries.flatten() {
                            let n = entry.file_name().to_string_lossy().to_string();
                            if n.starts_with(&prefix) {
                                let suffix = &n[old_fname.len()..];
                                let target = new_dir.join(format!("{}{}", new_fname, suffix));
                                if fs::rename(entry.path(), target).is_err() && !replace_if_exists {
                                    return Err(FspError::NTSTATUS(STATUS_ACCESS_DENIED));
                                }
                            }
                        }
                    }
                }
            } else {
                let old_f = self.members[node.member_idx].join(&old_rel);
                let new_f = self.members[node.member_idx].join(&new_rel);
                if let Some(parent) = new_f.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::rename(old_f, new_f).map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?;

                if node.member_idx != 0 {
                    let mut map = self.jbod_map.write();
                    map.remove(&old_rel.to_lowercase());
                    map.insert(new_rel.to_lowercase(), node.member_idx);
                    drop(map);
                    self.save_jbod_catalog();
                }
            }

            {
                let mut map = self.file_nodes.write();
                if let Some(n) = map.remove(&old_rel.to_lowercase()) {
                    *n.rel_path.write() = new_rel.clone();
                    map.insert(new_rel.to_lowercase(), n);
                }
            }
        }

        *ctx.rel_path.write() = new_rel.clone();
        self.log(format!("[VFS RENAME] '{}' -> '{}'", old_rel, new_rel));
        Ok(())
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
                if let Some(f) = node.direct_file.read().as_ref() {
                    let _ = f.sync_all();
                }

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

            let was_modified = *node.is_modified.lock();
            if was_modified {
                self.flush_node_dirty_parts(node);
                *node.is_modified.lock() = false;

                let size = *node.file_size.lock();
                if node.is_striped && size > 0 {
                    self.save_meta_size(&node.rel_path(), size);
                    let mb = size as f64 / (1024.0 * 1024.0);
                    self.log(format!(
                        "[VFS COMMIT] '{}' saved | {:.2} MB",
                        node.rel_path(),
                        mb
                    ));
                }
            }

            if remaining == 0 {
                let rel = node.rel_path();
                self.file_nodes.write().remove(&rel.to_lowercase());
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

        let check_path = if is_striped {
            self.meta_path(&rel)
        } else {
            self.members[member_idx].join(&rel)
        };

        if let Ok(m) = fs::metadata(check_path) {
            let ft = get_metadata_times(&m);
            info.creation_time = ft.creation_time;
            info.last_access_time = ft.last_access_time;
            info.last_write_time = ft.last_write_time;
            info.change_time = ft.change_time;
        }

        Ok(())
    }

    fn read(&self, ctx: &Self::FileContext, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        let node = context_node(ctx)?;

        // CRITICAL EOF & BOUNDS CHECK: Never allow reads beyond the true file size
        let current_file_size = *node.file_size.lock();
        if offset >= current_file_size {
            return Ok(0); // Normal EOF indication to Win32 ReadFile
        }

        let max_possible_read = (current_file_size - offset) as usize;
        let bytes_to_read = max_possible_read.min(buffer.len());
        if bytes_to_read == 0 {
            return Ok(0);
        }

        let target_slice = &mut buffer[..bytes_to_read];

        if !node.is_striped {
            let member_root = &self.members[node.member_idx];
            let file = node.get_or_open_direct_file(member_root, false)?;
            let bytes_read = file
                .seek_read(target_slice, offset)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            if let Some(counter) = self.io_bytes.get(node.member_idx) {
                counter.fetch_add(bytes_read as u64, Ordering::Relaxed);
            }

            return Ok(bytes_read as u32);
        }

        let slices = self.stripe_config.map_io_slices(offset, bytes_to_read);
        let mut tasks = Vec::with_capacity(slices.len());
        let mut remaining_dest = target_slice;

        for slice in slices {
            let part_arc =
                self.fetch_cached_part(node, slice.part_index, slice.target_drive_idx)?;
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

        let total_read_bytes = if tasks.len() > 1 && bytes_to_read >= PARALLEL_IO_THRESHOLD_BYTES {
            let results: Result<Vec<usize>, FspError> = tasks
                .into_par_iter()
                .map(|(slice, part_arc, dest)| Ok(read_action(&slice, &part_arc, dest)))
                .collect();
            results?.into_iter().sum::<usize>()
        } else {
            let mut total = 0;
            for (slice, part_arc, dest) in tasks {
                total += read_action(&slice, &part_arc, dest);
            }
            total
        };

        Ok(total_read_bytes as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total_bytes, free_bytes) = if self.custom_size_gb > 0 {
            let total = self.custom_size_gb * 1024 * 1024 * 1024;
            let used = {
                let mut cache = self.usage_cache.lock();
                if cache.0.elapsed() > std::time::Duration::from_secs(2) {
                    let calc = self.calculate_pool_used_bytes();
                    *cache = (std::time::Instant::now(), calc);
                    calc
                } else {
                    cache.1
                }
            };
            (total, total.saturating_sub(used))
        } else if self.policy == 1 {
            let mut min_free = u64::MAX;
            let mut min_total = u64::MAX;
            for m in self.members.iter() {
                if let Some((free, total)) = crate::win32::get_disk_space(m) {
                    min_free = min_free.min(free);
                    min_total = min_total.min(total);
                }
            }
            let n = self.members.len() as u64;
            let total = if min_total == u64::MAX {
                1024 * 1024 * 1024 * 1024
            } else {
                min_total * n
            };
            let free = if min_free == u64::MAX {
                total
            } else {
                min_free * n
            };
            (total, free)
        } else {
            let mut total_sum = 0u64;
            let mut free_sum = 0u64;
            for m in self.members.iter() {
                if let Some((free, total)) = crate::win32::get_disk_space(m) {
                    total_sum += total;
                    free_sum += free;
                }
            }
            if total_sum == 0 {
                total_sum = 1024 * 1024 * 1024 * 1024;
                free_sum = total_sum;
            }
            (total_sum, free_sum)
        };

        out_volume_info.total_size = total_bytes;
        out_volume_info.free_size = free_bytes;
        out_volume_info.set_volume_label("SymStripe RAM Pool");
        Ok(())
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
                        if name.contains(".part") || name.starts_with(".symstripe_jbod") {
                            continue;
                        }

                        if name.ends_with(".symstripe_meta") {
                            let clean = name.trim_end_matches(".symstripe_meta").to_string();
                            let key = clean.to_lowercase();
                            let size = fs::read_to_string(entry.path())
                                .ok()
                                .and_then(|s| s.trim().parse::<u64>().ok())
                                .unwrap_or(0);
                            let ft = entry.metadata().ok().map(|m| get_metadata_times(&m));
                            items.insert(key, (clean, false, size, ft));
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
}

fn context_node(ctx: &PoolFileHandle) -> Result<&Arc<FileNode>, FspError> {
    ctx.node
        .as_ref()
        .ok_or(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST))
}

pub struct VfsPoolSession {
    _host: FileSystemHost<SymStripePoolFs>,
    pub io_bytes: Arc<Vec<AtomicU64>>,
    pub member_paths: Vec<PathBuf>,
    pub cache_hits: Arc<AtomicU64>,
    pub cache_misses: Arc<AtomicU64>,
    pub raw_bytes: Arc<AtomicU64>,
    pub compressed_bytes: Arc<AtomicU64>,
    stop_flusher: Arc<AtomicBool>,
    flusher_handle: Option<std::thread::JoinHandle<()>>,
    fs_ref: SymStripePoolFs,
}

impl VfsPoolSession {
    pub fn rescan(&self) {}

    pub fn sample_and_reset_io(&self) -> Vec<u64> {
        self.io_bytes
            .iter()
            .map(|c| c.swap(0, Ordering::Relaxed))
            .collect()
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
    logger: Option<VfsLogCallback>,
) -> Result<VfsPoolSession, String> {
    if members.is_empty() {
        return Err("Cannot mount pool: no storage member folders specified.".into());
    }

    check_winfsp_available()?;

    let _init =
        winfsp::winfsp_init().map_err(|e| format!("WinFsp initialization failed: {:?}", e))?;

    let pool_fs = SymStripePoolFs::new(
        members.to_vec(),
        policy,
        stripe_size_bytes,
        custom_size_gb,
        codec_idx,
        tiered_storage,
        logger,
    );
    let io_bytes = pool_fs.io_bytes.clone();
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
    vp.post_cleanup_when_modified_only(true);

    let mut host = FileSystemHost::new(vp, pool_fs.clone())
        .map_err(|e| format!("FileSystemHost error: {:?}", e))?;
    host.mount(mount_target)
        .map_err(|e| format!("Failed to mount '{}': {:?}", mount_target, e))?;
    FileSystemHost::<SymStripePoolFs>::start(&mut host)
        .map_err(|e| format!("Failed to start dispatcher: {:?}", e))?;

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
        cache_hits,
        cache_misses,
        raw_bytes,
        compressed_bytes,
        stop_flusher,
        flusher_handle: Some(flusher_handle),
        fs_ref: pool_fs,
    })
}
