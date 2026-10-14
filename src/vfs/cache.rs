use super::node::{PartKey, VfsCodec};
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub struct CachedPart {
    pub data: Vec<u8>,
    pub dirty: bool,
    pub referenced: AtomicBool,
    pub codec: VfsCodec,
    pub rel_path: String,
    pub file_hash: u64,
    pub part_index: u64,
}

pub struct CacheInner {
    pub entries: HashMap<PartKey, Arc<RwLock<CachedPart>>>,
    pub clock_ring: Vec<PartKey>,
    pub clock_hand: usize,
}

pub struct GlobalStripeCache {
    pub inner: Mutex<CacheInner>,
    pub max_parts: usize,
}
