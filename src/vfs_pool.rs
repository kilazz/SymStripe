use crate::stripe_math::StripeConfig;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use widestring::U16CStr;
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};

const STATUS_INVALID_HANDLE: i32 = 0xC0000008_u32 as i32;
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC0000022_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;

/// Metadata record for a virtual file entry inside the storage pool
#[derive(Clone, Debug)]
pub struct PoolFileRecord {
    pub is_dir: bool,
    pub file_size: u64,
    #[allow(dead_code)]
    pub original_name: String,
    pub rel_path: String,
    /// If non-striped (JBOD / small file): physical path to the file
    pub direct_path: Option<PathBuf>,
    /// If striped (RAID-0): stripe block configuration
    pub is_striped: bool,
}

pub struct PoolFileHandle {
    pub is_dir: bool,
    pub record: PoolFileRecord,
    pub direct_file: Mutex<Option<File>>,
    pub dir_buffer: Mutex<DirBuffer>,
}

pub struct SymStripePoolFs {
    pub members: Vec<PathBuf>,
    pub policy: i32, // 0 = Capacity (JBOD), 1 = Speed (RAID-0)
    pub stripe_config: StripeConfig,
    pub custom_size_gb: u64, // 0 = Auto detect, >0 = Custom volume quota in GB
    pub virtual_table: RwLock<HashMap<String, PoolFileRecord>>,
    pub dir_children: RwLock<HashMap<String, Vec<PoolFileRecord>>>,
}

impl SymStripePoolFs {
    pub fn new(
        members: Vec<PathBuf>,
        policy: i32,
        stripe_size_bytes: u64,
        custom_size_gb: u64,
    ) -> Self {
        let num_drives = members.len().max(1);
        Self {
            members,
            policy,
            stripe_config: StripeConfig::new(stripe_size_bytes, num_drives),
            custom_size_gb,
            virtual_table: RwLock::new(HashMap::new()),
            dir_children: RwLock::new(HashMap::new()),
        }
    }

    fn normalize_path(raw_path: &U16CStr) -> String {
        let string_path = raw_path.to_string_lossy();
        if string_path == "\\" || string_path.is_empty() {
            return "\\".to_string();
        }
        format!("\\{}", string_path.trim_start_matches('\\').to_lowercase())
    }

    /// Scans all pool member directories and builds the merged namespace tree
    pub fn index_pool_members(&self) {
        let mut table = self.virtual_table.write().unwrap();
        let mut children_map: HashMap<String, Vec<PoolFileRecord>> = HashMap::new();
        table.clear();

        // Register virtual root
        table.insert(
            "\\".to_string(),
            PoolFileRecord {
                is_dir: true,
                file_size: 0,
                original_name: String::new(),
                rel_path: String::new(),
                direct_path: None,
                is_striped: false,
            },
        );

        for member_root in &self.members {
            for entry in walkdir::WalkDir::new(member_root)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if let Ok(rel) = path.strip_prefix(member_root) {
                    let rel_str = rel.to_string_lossy().to_string();
                    if rel_str.is_empty() {
                        continue;
                    }

                    // Check for RAID-0 metadata descriptor
                    if rel_str.ends_with(".symstripe_meta") {
                        let base_rel = rel_str.trim_end_matches(".symstripe_meta");
                        let key = format!("\\{}", base_rel.to_lowercase());
                        if let Ok(meta_json) = fs::read_to_string(path)
                            && let Ok(size) = meta_json.trim().parse::<u64>()
                        {
                            let file_name = Path::new(base_rel)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();

                            let record = PoolFileRecord {
                                is_dir: false,
                                file_size: size,
                                original_name: file_name,
                                rel_path: base_rel.to_string(),
                                direct_path: None,
                                is_striped: true,
                            };
                            table.insert(key.clone(), record.clone());

                            let parent_key = match Path::new(base_rel).parent() {
                                Some(p) if p.as_os_str().is_empty() => "\\".to_string(),
                                Some(p) => format!("\\{}", p.to_string_lossy().to_lowercase()),
                                None => "\\".to_string(),
                            };
                            children_map.entry(parent_key).or_default().push(record);
                        }
                        continue;
                    }

                    // Hide raw striped .part files from directory listings
                    if rel_str.contains(".part") {
                        continue;
                    }

                    let key = format!("\\{}", rel_str.to_lowercase());
                    if !table.contains_key(&key)
                        && let Ok(meta) = fs::metadata(path)
                    {
                        let is_dir = meta.is_dir();
                        let file_size = if is_dir { 0 } else { meta.len() };
                        let file_name = rel
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();

                        let record = PoolFileRecord {
                            is_dir,
                            file_size,
                            original_name: file_name,
                            rel_path: rel_str.clone(),
                            direct_path: Some(path.to_path_buf()),
                            is_striped: false,
                        };

                        table.insert(key.clone(), record.clone());

                        let parent_key = match rel.parent() {
                            Some(p) if p.as_os_str().is_empty() => "\\".to_string(),
                            Some(p) => format!("\\{}", p.to_string_lossy().to_lowercase()),
                            None => "\\".to_string(),
                        };
                        children_map.entry(parent_key).or_default().push(record);
                    }
                }
            }
        }

        let mut children_guard = self.dir_children.write().unwrap();
        *children_guard = children_map;
    }
}

impl FileSystemContext for SymStripePoolFs {
    type FileContext = PoolFileHandle;

    fn get_security_by_name(
        &self,
        _file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        Ok(FileSecurity {
            attributes: 0x80, // FILE_ATTRIBUTE_NORMAL
            reparse: false,
            sz_security_descriptor: 0,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        _file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let key = Self::normalize_path(file_name);
        let table = self.virtual_table.read().unwrap();
        let record = table
            .get(&key)
            .ok_or(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?
            .clone();

        let file_handle = if !record.is_dir && !record.is_striped {
            if let Some(path) = &record.direct_path {
                Some(File::open(path).map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?)
            } else {
                None
            }
        } else {
            None
        };

        Ok(PoolFileHandle {
            is_dir: record.is_dir,
            record,
            direct_file: Mutex::new(file_handle),
            dir_buffer: Mutex::new(DirBuffer::new()),
        })
    }

    fn close(&self, context: Self::FileContext) {
        drop(context);
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        file_info.file_size = context.record.file_size;
        file_info.allocation_size = (context.record.file_size + 4095) & !4095;
        file_info.file_attributes = if context.is_dir { 0x10 } else { 0x80 };
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST));
        }

        // 1. Direct unstriped read (JBOD / normal file)
        if !context.record.is_striped {
            let mut guard = context.direct_file.lock().unwrap();
            if let Some(file) = guard.as_mut() {
                file.seek(SeekFrom::Start(offset))
                    .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let bytes_read = file
                    .read(buffer)
                    .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                return Ok(bytes_read as u32);
            } else {
                return Err(FspError::NTSTATUS(STATUS_INVALID_HANDLE));
            }
        }

        // 2. Extent-Striped RAID-0 read across multiple pool drives
        let slices = self.stripe_config.map_io_slices(offset, buffer.len());
        let mut total_bytes = 0;
        let mut buffer_offset = 0;

        for slice in slices {
            let target_member = &self.members[slice.target_drive_idx];
            let part_filename = format!("{}.part{}", context.record.rel_path, slice.part_index);
            let part_path = target_member.join(part_filename);

            if let Ok(mut part_file) = File::open(&part_path)
                && part_file
                    .seek(SeekFrom::Start(slice.offset_in_part))
                    .is_ok()
            {
                let dest_slice = &mut buffer[buffer_offset..buffer_offset + slice.length];
                if let Ok(n) = part_file.read(dest_slice) {
                    total_bytes += n;
                    buffer_offset += n;
                }
            }
        }

        Ok(total_bytes as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let table = self.virtual_table.read().unwrap();
        let used_bytes: u64 = table
            .values()
            .filter(|r| !r.is_dir)
            .map(|r| r.file_size)
            .sum();

        let (total_bytes, free_bytes) = if self.custom_size_gb > 0 {
            // User-defined fixed volume quota in GB
            let total = self.custom_size_gb * 1024 * 1024 * 1024;
            let free = total.saturating_sub(used_bytes);
            (total, free)
        } else {
            // Auto-detect based on physical drives free space
            let mut member_free_spaces = Vec::new();
            for member in &self.members {
                if let Some(free) = crate::win32::get_free_disk_space_bytes(member) {
                    member_free_spaces.push(free);
                }
            }

            let physical_free = if member_free_spaces.is_empty() {
                1024 * 1024 * 1024 * 1024 // 1 TB default fallback
            } else if self.policy == 1 {
                // Speed (RAID-0): limited by smallest member capacity * num_drives
                let min_free = member_free_spaces.iter().copied().min().unwrap_or(0);
                min_free * (self.members.len() as u64)
            } else {
                // Capacity (JBOD): sum of all free space across all members
                member_free_spaces.iter().copied().sum()
            };

            (physical_free + used_bytes, physical_free)
        };

        out_volume_info.total_size = total_bytes;
        out_volume_info.free_size = free_bytes;
        out_volume_info.set_volume_label("SymStripe Pool");
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST));
        }

        let dir_buf = context.dir_buffer.lock().unwrap();
        Ok(dir_buf.read(marker, buffer))
    }
}

pub struct VfsPoolSession {
    _host: FileSystemHost<SymStripePoolFs>,
}

/// Mounts the Virtual Storage Pool onto the specified drive letter (e.g., "Z:")
pub fn mount_storage_pool(
    mount_target: &str,
    members: &[PathBuf],
    policy: i32,
    stripe_size_bytes: u64,
    custom_size_gb: u64,
) -> Result<VfsPoolSession, String> {
    if members.is_empty() {
        return Err("Cannot mount pool: no storage member folders specified.".into());
    }

    let _init_token = winfsp::winfsp_init().map_err(|e| {
        format!(
            "WinFsp initialization failed ({:?}). Ensure WinFsp runtime is installed.",
            e
        )
    })?;

    let pool_fs = SymStripePoolFs::new(members.to_vec(), policy, stripe_size_bytes, custom_size_gb);
    pool_fs.index_pool_members();

    let mut volume_params = VolumeParams::new();
    volume_params.filesystem_name("SymStripePool");
    volume_params.read_only_volume(true); // Mount as safe streaming volume for games

    let mut host = FileSystemHost::new(volume_params, pool_fs)
        .map_err(|e| format!("Failed to create FileSystemHost: {:?}", e))?;

    host.mount(mount_target)
        .map_err(|e| format!("Failed to mount pool to '{}': {:?}", mount_target, e))?;

    FileSystemHost::<SymStripePoolFs>::start(&mut host)
        .map_err(|e| format!("Failed to start WinFsp dispatcher: {:?}", e))?;

    Ok(VfsPoolSession { _host: host })
}
