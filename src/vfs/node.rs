use parking_lot::{Mutex, RwLock};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use winfsp::filesystem::DirBuffer;

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

pub struct DirectBuffer {
    pub offset: u64,
    pub data: Vec<u8>,
}

pub struct FileNode {
    pub rel_path: RwLock<String>,
    pub file_hash: AtomicU64,
    pub is_striped: bool,
    pub present_on: Vec<usize>,
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
    pub node: Option<std::sync::Arc<FileNode>>,
    pub dir_buffer: Mutex<DirBuffer>,
    pub delete_on_close: Mutex<bool>,
}

impl PoolFileHandle {
    pub fn rel_path(&self) -> String {
        self.rel_path.read().clone()
    }
}
