use crate::stripe_math::StripeConfig;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use widestring::U16CStr;
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

const STATUS_INVALID_HANDLE: i32 = 0xC0000008_u32 as i32;
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC0000022_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;
const STATUS_NO_MORE_FILES: i32 = 0x80000006_u32 as i32;
const STATUS_NO_SUCH_FILE: i32 = 0xC000000F_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC0000023_u32 as i32;

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

#[derive(Clone, Debug)]
pub struct PoolFileRecord {
    pub is_dir: bool,
    pub file_size: u64,
    #[allow(dead_code)]
    pub original_name: String,
    pub rel_path: String,
    pub direct_path: Option<PathBuf>,
    pub is_striped: bool,
}

pub struct PoolFileHandle {
    pub is_dir: bool,
    pub record: PoolFileRecord,
    pub direct_file: Mutex<Option<File>>,
    pub part_files: Mutex<HashMap<u64, File>>,
    pub dir_buffer: Mutex<DirBuffer>,
    pub delete_on_close: Mutex<bool>,
}

pub struct SymStripePoolFs {
    pub members: Vec<PathBuf>,
    pub policy: i32,
    pub stripe_config: StripeConfig,
    pub custom_size_gb: u64,
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

    /// Selects the physical drive member with the most available free space
    fn pick_best_member(&self) -> PathBuf {
        let mut best = &self.members[0];
        let mut max_free = 0;
        for m in &self.members {
            if let Some(free) = crate::win32::get_free_disk_space_bytes(m)
                && free > max_free
            {
                max_free = free;
                best = m;
            }
        }
        best.clone()
    }

    pub fn index_pool_members(&self) {
        let mut table = self.virtual_table.write().unwrap();
        let mut children_map: HashMap<String, Vec<PoolFileRecord>> = HashMap::new();
        table.clear();

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
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let _name_str = file_name.to_string_lossy();
        let key = Self::normalize_path(file_name);

        let table = self.virtual_table.read().unwrap();
        let is_dir = if let Some(record) = table.get(&key) {
            record.is_dir
        } else if key == "\\" {
            true
        } else {
            return Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND));
        };

        let sd = get_default_security_descriptor();
        let sd_len = sd.len() as u64;

        if let Some(buf) = security_descriptor {
            if buf.len() < sd.len() {
                return Err(FspError::NTSTATUS(STATUS_BUFFER_TOO_SMALL));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(sd.as_ptr(), buf.as_mut_ptr() as *mut u8, sd.len());
            }
        }

        let attrs = if is_dir { 0x10 } else { 0x80 };

        Ok(FileSecurity {
            attributes: attrs,
            reparse: false,
            sz_security_descriptor: sd_len,
        })
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        let sd = get_default_security_descriptor();
        if let Some(buf) = security_descriptor {
            if buf.len() < sd.len() {
                return Err(FspError::NTSTATUS(STATUS_BUFFER_TOO_SMALL));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(sd.as_ptr(), buf.as_mut_ptr() as *mut u8, sd.len());
            }
        }
        Ok(sd.len() as u64)
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let key = Self::normalize_path(file_name);
        let table = self.virtual_table.read().unwrap();
        let record = table
            .get(&key)
            .ok_or(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?
            .clone();

        // Populate OpenFileInfo so WinFsp kernel knows whether it's a directory
        {
            let info = file_info.as_mut();
            *info = FileInfo::default();
            info.file_size = record.file_size;
            info.allocation_size = if record.is_dir {
                0
            } else {
                (record.file_size + 4095) & !4095
            };
            info.file_attributes = if record.is_dir { 0x10 } else { 0x80 };
            info.hard_links = 1;
            info.index_number = 1;
        }

        let file_handle = if !record.is_dir && !record.is_striped {
            if let Some(path) = &record.direct_path {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .or_else(|_| File::open(path))
                    .ok()
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
            part_files: Mutex::new(HashMap::new()),
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
        let key = Self::normalize_path(file_name);
        let is_dir = (create_options & 1 != 0) || (file_attributes & 0x10 != 0);

        let best_member = self.pick_best_member();
        let rel_trimmed = key.trim_start_matches('\\');
        let physical_path = best_member.join(rel_trimmed);

        if let Some(parent) = physical_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let direct_file = if is_dir {
            fs::create_dir_all(&physical_path)
                .map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?;
            None
        } else {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&physical_path)
                .map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?;
            Some(f)
        };

        let file_name_clean = Path::new(rel_trimmed)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let record = PoolFileRecord {
            is_dir,
            file_size: 0,
            original_name: file_name_clean,
            rel_path: rel_trimmed.to_string(),
            direct_path: Some(physical_path),
            is_striped: false,
        };

        // Populate OpenFileInfo
        {
            let info = file_info.as_mut();
            *info = FileInfo::default();
            info.file_size = 0;
            info.allocation_size = 0;
            info.file_attributes = if is_dir { 0x10 } else { 0x80 };
            info.hard_links = 1;
            info.index_number = 1;
        }

        // Register in virtual tables
        {
            let mut table = self.virtual_table.write().unwrap();
            table.insert(key.clone(), record.clone());

            let parent_key = match Path::new(rel_trimmed).parent() {
                Some(p) if p.as_os_str().is_empty() => "\\".to_string(),
                Some(p) => format!("\\{}", p.to_string_lossy().to_lowercase()),
                None => "\\".to_string(),
            };

            let mut children_map = self.dir_children.write().unwrap();
            children_map
                .entry(parent_key)
                .or_default()
                .push(record.clone());
        }

        Ok(PoolFileHandle {
            is_dir,
            record,
            direct_file: Mutex::new(direct_file),
            part_files: Mutex::new(HashMap::new()),
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
        if context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST));
        }

        let mut guard = context.direct_file.lock().unwrap();
        if let Some(file) = guard.as_mut() {
            let write_offset = if write_to_eof {
                file.seek(SeekFrom::End(0))
                    .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            } else {
                offset
            };

            file.seek_write(buffer, write_offset)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

            let new_len = file
                .metadata()
                .map(|m| m.len())
                .unwrap_or(write_offset + buffer.len() as u64);

            // Update file metadata
            file_info.file_size = new_len;
            file_info.allocation_size = (new_len + 4095) & !4095;
            file_info.file_attributes = 0x80;
            file_info.hard_links = 1;

            let key = format!("\\{}", context.record.rel_path.to_lowercase());
            if let Some(rec) = self.virtual_table.write().unwrap().get_mut(&key) {
                rec.file_size = new_len;
            }

            Ok(buffer.len() as u32)
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_HANDLE))
        }
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST));
        }

        let mut guard = context.direct_file.lock().unwrap();
        if let Some(file) = guard.as_mut() {
            file.set_len(new_size)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            file_info.file_size = new_size;
            file_info.allocation_size = (new_size + 4095) & !4095;
            file_info.file_attributes = 0x80;

            let key = format!("\\{}", context.record.rel_path.to_lowercase());
            if let Some(rec) = self.virtual_table.write().unwrap().get_mut(&key) {
                rec.file_size = new_size;
            }
            Ok(())
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_HANDLE))
        }
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        file_info.file_attributes = if file_attributes != 0 && file_attributes != u32::MAX {
            file_attributes
        } else if context.is_dir {
            0x10
        } else {
            0x80
        };
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        *context.delete_on_close.lock().unwrap() = delete_file;
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let should_delete = (flags & 0x01 != 0) || *context.delete_on_close.lock().unwrap();
        if should_delete {
            // Drop handle before physical delete
            *context.direct_file.lock().unwrap() = None;

            if let Some(p) = &context.record.direct_path {
                if context.is_dir {
                    let _ = fs::remove_dir(p);
                } else {
                    let _ = fs::remove_file(p);
                }
            }

            let key = format!("\\{}", context.record.rel_path.to_lowercase());
            self.virtual_table.write().unwrap().remove(&key);

            let parent_key = match Path::new(&context.record.rel_path).parent() {
                Some(p) if p.as_os_str().is_empty() => "\\".to_string(),
                Some(p) => format!("\\{}", p.to_string_lossy().to_lowercase()),
                None => "\\".to_string(),
            };

            let mut children_map = self.dir_children.write().unwrap();
            if let Some(children) = children_map.get_mut(&parent_key) {
                children.retain(|c| c.rel_path != context.record.rel_path);
            }
        }
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let new_key = Self::normalize_path(new_file_name);
        let new_rel = new_key.trim_start_matches('\\').to_string();

        if let Some(old_p) = &context.record.direct_path {
            let new_p = old_p
                .parent()
                .unwrap_or(old_p)
                .join(Path::new(&new_rel).file_name().unwrap_or_default());
            fs::rename(old_p, &new_p).map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?;
        }

        let old_key = format!("\\{}", context.record.rel_path.to_lowercase());
        let mut table = self.virtual_table.write().unwrap();
        if let Some(mut rec) = table.remove(&old_key) {
            rec.rel_path = new_rel;
            table.insert(new_key, rec);
        }

        Ok(())
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if let Some(ctx) = context {
            let mut guard = ctx.direct_file.lock().unwrap();
            if let Some(f) = guard.as_mut() {
                let _ = f.sync_all();
            }
        }
        Ok(())
    }

    fn close(&self, context: Self::FileContext) {
        drop(context);
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        *file_info = FileInfo::default();
        file_info.file_size = context.record.file_size;
        file_info.allocation_size = if context.is_dir {
            0
        } else {
            (context.record.file_size + 4095) & !4095
        };
        file_info.file_attributes = if context.is_dir { 0x10 } else { 0x80 };
        file_info.hard_links = 1;
        file_info.index_number = 1;

        let now_ft = 133500000000000000u64;
        file_info.creation_time = now_ft;
        file_info.last_access_time = now_ft;
        file_info.last_write_time = now_ft;
        file_info.change_time = now_ft;

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

        let slices = self.stripe_config.map_io_slices(offset, buffer.len());
        let mut part_cache = context.part_files.lock().unwrap();
        let mut total_bytes = 0;
        let mut buffer_offset = 0;

        for slice in slices {
            let target_member = &self.members[slice.target_drive_idx];
            let part_filename = format!("{}.part{}", context.record.rel_path, slice.part_index);
            let part_path = target_member.join(part_filename);

            if let Entry::Vacant(e) = part_cache.entry(slice.part_index)
                && let Ok(f) = File::open(&part_path)
            {
                e.insert(f);
            }

            if let Some(part_file) = part_cache.get(&slice.part_index) {
                let dest_slice = &mut buffer[buffer_offset..buffer_offset + slice.length];

                if let Ok(n) = part_file.seek_read(dest_slice, slice.offset_in_part) {
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
            let total = self.custom_size_gb * 1024 * 1024 * 1024;
            let free = total.saturating_sub(used_bytes);
            (total, free)
        } else {
            let mut member_free_spaces = Vec::new();
            for member in &self.members {
                if let Some(free) = crate::win32::get_free_disk_space_bytes(member) {
                    member_free_spaces.push(free);
                }
            }

            let physical_free = if member_free_spaces.is_empty() {
                1024 * 1024 * 1024 * 1024
            } else if self.policy == 1 {
                let min_free = member_free_spaces.iter().copied().min().unwrap_or(0);
                min_free * (self.members.len() as u64)
            } else {
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
        let is_first_query = marker.is_none();

        if !context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST));
        }

        let dir_buf = context.dir_buffer.lock().unwrap();

        if let Ok(dir_lock) = dir_buf.acquire(is_first_query, None) {
            let key = context.record.rel_path.to_lowercase();
            let lookup_key = if key.is_empty() {
                "\\".to_string()
            } else {
                format!("\\{}", key)
            };

            let children_guard = self.dir_children.read().unwrap();

            if let Some(children) = children_guard.get(&lookup_key) {
                for child in children {
                    let mut dir_info = DirInfo::<255>::new();

                    {
                        let info = dir_info.file_info_mut();
                        info.file_size = child.file_size;
                        info.allocation_size = (child.file_size + 4095) & !4095;
                        info.file_attributes = if child.is_dir { 0x10 } else { 0x80 };
                    }

                    let _ = dir_info.set_name(&child.original_name);
                    let _ = dir_lock.write(&mut dir_info);
                }
            }
        }

        let bytes_read = dir_buf.read(marker, buffer);

        if bytes_read <= 2 {
            if is_first_query {
                return Err(FspError::NTSTATUS(STATUS_NO_SUCH_FILE));
            } else {
                return Err(FspError::NTSTATUS(STATUS_NO_MORE_FILES));
            }
        }
        Ok(bytes_read)
    }
}

pub struct VfsPoolSession {
    _host: FileSystemHost<SymStripePoolFs>,
}

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
    volume_params.filesystem_name("NTFS");
    volume_params.volume_serial_number(0x594D5354);

    let mut host = FileSystemHost::new(volume_params, pool_fs)
        .map_err(|e| format!("Failed to create FileSystemHost: {:?}", e))?;

    host.mount(mount_target)
        .map_err(|e| format!("Failed to mount pool to '{}': {:?}", mount_target, e))?;

    FileSystemHost::<SymStripePoolFs>::start(&mut host)
        .map_err(|e| format!("Failed to start WinFsp dispatcher: {:?}", e))?;

    Ok(VfsPoolSession { _host: host })
}

#[allow(dead_code)]
pub fn ingest_file_into_pool(
    source_file: &Path,
    pool_members: &[PathBuf],
    stripe_size_bytes: u64,
    rel_path: &str,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    let mut src = File::open(source_file).map_err(|e| format!("Failed to open source: {}", e))?;
    let file_size = src.metadata().map_err(|e| e.to_string())?.len();

    let num_drives = pool_members.len();
    if num_drives == 0 {
        return Err("No pool members available.".into());
    }

    let mut buffer = vec![0u8; stripe_size_bytes as usize];
    let mut current_offset = 0u64;
    let mut stripe_idx = 0;

    while current_offset < file_size {
        let bytes_to_read = ((file_size - current_offset).min(stripe_size_bytes)) as usize;
        let slice = &mut buffer[..bytes_to_read];

        src.read_exact(slice)
            .map_err(|e| format!("Read error: {}", e))?;

        let target_drive = stripe_idx % num_drives;
        let member_root = &pool_members[target_drive];

        let part_rel = format!("{}.part{}", rel_path, stripe_idx);
        let part_path = member_root.join(&part_rel);
        if let Some(parent) = part_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut out_file = File::create(&part_path).map_err(|e| format!("Write error: {}", e))?;
        out_file
            .write_all(slice)
            .map_err(|e| format!("Write error: {}", e))?;

        current_offset += bytes_to_read as u64;
        stripe_idx += 1;

        on_progress(current_offset, file_size);
    }

    let meta_rel = format!("{}.symstripe_meta", rel_path);
    let meta_path = pool_members[0].join(&meta_rel);
    if let Some(parent) = meta_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&meta_path, file_size.to_string()).map_err(|e| format!("Meta write error: {}", e))?;

    Ok(())
}
