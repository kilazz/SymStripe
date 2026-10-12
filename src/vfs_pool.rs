use crate::stripe_math::StripeConfig;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use widestring::U16CStr;
use winfsp::FspError;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

// WinFsp / NTSTATUS Error Codes
const STATUS_INVALID_HANDLE: i32 = 0xC0000008_u32 as i32;
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC0000022_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC0000023_u32 as i32;

pub type VfsLogCallback = Arc<dyn Fn(String) + Send + Sync>;

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

/// Generates a default Windows Security Descriptor (SDDL) allowing full access for LocalSystem, Administrators, and Everyone.
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

/// Extracts native file times from OS metadata.
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

/// Converts system time into Windows FileTime format (100-nanosecond intervals since January 1, 1601).
fn get_current_filetime() -> u64 {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_secs() + 11644473600) * 10_000_000 + (dur.subsec_nanos() as u64 / 100)
}

/// Represents an active file/directory context handle in the Virtual File System.
pub struct PoolFileHandle {
    pub is_dir: bool,
    pub rel_path: String,
    pub is_striped: bool,
    pub file_size: Mutex<u64>,
    pub direct_file: Mutex<Option<File>>,
    pub part_files: Mutex<HashMap<u64, File>>,
    pub dir_buffer: Mutex<DirBuffer>,
    pub delete_on_close: Mutex<bool>,
    pub is_modified: Mutex<bool>,
}

/// Core FileSystem context implementing Master-Drive Passthrough and RAID-0 Striping.
#[derive(Clone)]
pub struct SymStripePoolFs {
    pub members: Arc<Vec<PathBuf>>,
    pub policy: i32,
    pub stripe_config: StripeConfig,
    pub custom_size_gb: u64,
    pub logger: Option<VfsLogCallback>,
}

impl SymStripePoolFs {
    pub fn new(
        members: Vec<PathBuf>,
        policy: i32,
        stripe_size_bytes: u64,
        custom_size_gb: u64,
        logger: Option<VfsLogCallback>,
    ) -> Self {
        let num_drives = members.len().max(1);
        Self {
            members: Arc::new(members),
            policy,
            stripe_config: StripeConfig::new(stripe_size_bytes, num_drives),
            custom_size_gb,
            logger,
        }
    }

    /// Sends trace logs to the GUI terminal panel.
    fn log(&self, msg: String) {
        if let Some(cb) = &self.logger {
            cb(msg);
        }
    }

    /// Normalizes raw paths from WinFsp into clean relative paths.
    fn normalize_path(raw_path: &U16CStr) -> String {
        let string_path = raw_path.to_string_lossy();
        let trimmed = string_path.trim_start_matches('\\');
        trimmed.to_string()
    }

    /// Returns the primary master storage drive (Members[0]), which acts as the physical table of contents.
    fn master(&self) -> &Path {
        &self.members[0]
    }

    /// Returns the path to the `.symstripe_meta` tracking file for a striped asset.
    fn meta_path(&self, rel_path: &str) -> PathBuf {
        self.master().join(format!("{}.symstripe_meta", rel_path))
    }

    /// Reads the true aggregated file size from the `.symstripe_meta` descriptor file.
    fn read_meta_size(&self, rel_path: &str) -> Option<u64> {
        let p = self.meta_path(rel_path);
        fs::read_to_string(p)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    /// Persists the aggregated file size into `.symstripe_meta`.
    fn save_meta_size(&self, rel_path: &str, size: u64) {
        let p = self.meta_path(rel_path);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(p, size.to_string());
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
        let rel_path = Self::normalize_path(file_name);
        let master_p = self.master().join(&rel_path);

        let is_dir = if rel_path.is_empty() || master_p.is_dir() {
            true
        } else if master_p.is_file() || self.meta_path(&rel_path).exists() {
            false
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
        let rel_path = Self::normalize_path(file_name);
        let master_p = self.master().join(&rel_path);

        let (is_dir, is_striped, file_size, direct_handle) =
            if rel_path.is_empty() || master_p.is_dir() {
                (true, false, 0, None)
            } else if let Some(size) = self.read_meta_size(&rel_path) {
                (false, true, size, None)
            } else if master_p.is_file() {
                let meta = fs::metadata(&master_p)
                    .map_err(|_| FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&master_p)
                    .ok();
                (false, false, meta.len(), f)
            } else {
                return Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND));
            };

        {
            let info = file_info.as_mut();
            *info = FileInfo::default();
            info.file_size = file_size;
            info.allocation_size = if is_dir {
                0
            } else {
                (file_size + 4095) & !4095
            };
            info.file_attributes = if is_dir { 0x10 } else { 0x80 };
            info.hard_links = 0;
            info.index_number = 0;

            if let Ok(m) = fs::metadata(&master_p) {
                let ft = get_metadata_times(&m);
                info.creation_time = ft.creation_time;
                info.last_access_time = ft.last_access_time;
                info.last_write_time = ft.last_write_time;
                info.change_time = ft.change_time;
            }
        }

        Ok(PoolFileHandle {
            is_dir,
            rel_path,
            is_striped,
            file_size: Mutex::new(file_size),
            direct_file: Mutex::new(direct_handle),
            part_files: Mutex::new(HashMap::new()),
            dir_buffer: Mutex::new(DirBuffer::new()),
            delete_on_close: Mutex::new(false),
            is_modified: Mutex::new(false),
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

        let (is_striped, direct_handle) = if is_dir {
            for m in self.members.iter() {
                let _ = fs::create_dir_all(m.join(&rel_path));
            }
            (false, None)
        } else if self.policy == 1 {
            self.save_meta_size(&rel_path, 0);
            (true, None)
        } else {
            let p = self.master().join(&rel_path);
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&p)
                .ok();
            (false, f)
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
            info.hard_links = 0;
            info.index_number = 0;
        }

        self.log(format!(
            "[VFS CREATE] {} '{}'",
            if is_dir { "DIR" } else { "FILE" },
            rel_path
        ));

        Ok(PoolFileHandle {
            is_dir,
            rel_path,
            is_striped,
            file_size: Mutex::new(0),
            direct_file: Mutex::new(direct_handle),
            part_files: Mutex::new(HashMap::new()),
            dir_buffer: Mutex::new(DirBuffer::new()),
            delete_on_close: Mutex::new(false),
            is_modified: Mutex::new(true),
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

        *context.is_modified.lock().unwrap() = true;

        let mut current_size = context.file_size.lock().unwrap();
        let write_offset = if write_to_eof { *current_size } else { offset };

        if !context.is_striped {
            let mut guard = context.direct_file.lock().unwrap();
            if let Some(f) = guard.as_mut() {
                f.seek_write(buffer, write_offset)
                    .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            } else {
                return Err(FspError::NTSTATUS(STATUS_INVALID_HANDLE));
            }
        } else {
            let slices = self.stripe_config.map_io_slices(write_offset, buffer.len());
            let mut part_cache = context.part_files.lock().unwrap();
            let mut buffer_offset = 0;

            for slice in slices {
                let target_member = &self.members[slice.target_drive_idx];
                let part_filename = format!("{}.part{}", context.rel_path, slice.part_index);
                let part_path = target_member.join(part_filename);

                if let Some(parent) = part_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }

                if let Entry::Vacant(e) = part_cache.entry(slice.part_index)
                    && let Ok(f) = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&part_path)
                {
                    e.insert(f);
                }

                let part_file = part_cache
                    .get_mut(&slice.part_index)
                    .ok_or(FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

                let src_slice = &buffer[buffer_offset..buffer_offset + slice.length];
                part_file
                    .seek_write(src_slice, slice.offset_in_part)
                    .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

                buffer_offset += slice.length;
            }
        }

        let new_len = write_offset + buffer.len() as u64;
        if new_len > *current_size {
            *current_size = new_len;
        }

        file_info.file_size = *current_size;
        file_info.allocation_size = (*current_size + 4095) & !4095;
        file_info.file_attributes = 0x80;
        file_info.hard_links = 0;
        file_info.index_number = 0;

        Ok(buffer.len() as u32)
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

        *context.is_modified.lock().unwrap() = true;
        let mut current_size = context.file_size.lock().unwrap();
        *current_size = new_size;

        if !context.is_striped {
            if let Some(f) = context.direct_file.lock().unwrap().as_mut() {
                let _ = f.set_len(new_size);
            }
        } else {
            self.save_meta_size(&context.rel_path, new_size);
        }

        file_info.file_size = new_size;
        file_info.allocation_size = (new_size + 4095) & !4095;
        file_info.file_attributes = 0x80;
        file_info.hard_links = 0;
        file_info.index_number = 0;

        Ok(())
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
            *context.direct_file.lock().unwrap() = None;
            context.part_files.lock().unwrap().clear();

            if context.is_dir {
                for m in self.members.iter() {
                    let _ = fs::remove_dir_all(m.join(&context.rel_path));
                }
            } else if context.is_striped {
                let _ = fs::remove_file(self.meta_path(&context.rel_path));

                let parent_opt = Path::new(&context.rel_path).parent();
                let file_name = Path::new(&context.rel_path)
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
                let _ = fs::remove_file(self.master().join(&context.rel_path));
            }

            self.log(format!("[VFS PURGE] Deleted '{}'", context.rel_path));
        }
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let new_rel = Self::normalize_path(new_file_name);
        let old_rel = &context.rel_path;

        if context.is_dir {
            for m in self.members.iter() {
                let old_d = m.join(old_rel);
                let new_d = m.join(&new_rel);
                if old_d.exists() {
                    if let Some(parent) = new_d.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::rename(old_d, new_d);
                }
            }
        } else if context.is_striped {
            context.part_files.lock().unwrap().clear();

            let old_m = self.meta_path(old_rel);
            let new_m = self.meta_path(&new_rel);
            if old_m.exists() {
                if let Some(parent) = new_m.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::rename(old_m, new_m);
            }

            let old_fname = Path::new(old_rel)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let new_fname = Path::new(&new_rel)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let prefix = format!("{}.part", old_fname);

            for m in self.members.iter() {
                let old_dir = m.join(Path::new(old_rel).parent().unwrap_or(Path::new("")));
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
            let old_f = self.master().join(old_rel);
            let new_f = self.master().join(&new_rel);
            if let Some(parent) = new_f.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::rename(old_f, new_f).map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?;
        }

        self.log(format!("[VFS RENAME] '{}' -> '{}'", old_rel, new_rel));
        Ok(())
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if let Some(ctx) = context {
            if *ctx.delete_on_close.lock().unwrap() {
                return Ok(());
            }

            let mut guard = ctx.direct_file.lock().unwrap();
            if let Some(f) = guard.as_mut() {
                let _ = f.sync_all();
            }

            let mut part_cache = ctx.part_files.lock().unwrap();
            for part_file in part_cache.values_mut() {
                let _ = part_file.sync_all();
            }

            let size = *ctx.file_size.lock().unwrap();
            if ctx.is_striped && size > 0 {
                self.save_meta_size(&ctx.rel_path, size);
            }
        }
        Ok(())
    }

    fn close(&self, context: Self::FileContext) {
        if *context.delete_on_close.lock().unwrap() {
            return;
        }

        let was_modified = *context.is_modified.lock().unwrap();
        if was_modified {
            let mut part_cache = context.part_files.lock().unwrap();
            for part_file in part_cache.values_mut() {
                let _ = part_file.sync_all();
            }

            let size = *context.file_size.lock().unwrap();
            if context.is_striped && size > 0 {
                self.save_meta_size(&context.rel_path, size);
                let mb = size as f64 / (1024.0 * 1024.0);
                self.log(format!(
                    "[VFS COMMIT] '{}' saved | {:.2} MB",
                    context.rel_path, mb
                ));
            }
        }
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let master_p = self.master().join(&context.rel_path);
        let size = *context.file_size.lock().unwrap();

        *file_info = FileInfo::default();
        file_info.file_size = size;
        file_info.allocation_size = if context.is_dir {
            0
        } else {
            (size + 4095) & !4095
        };
        file_info.file_attributes = if context.is_dir { 0x10 } else { 0x80 };
        file_info.hard_links = 0;
        file_info.index_number = 0;

        let check_path = if context.is_striped {
            self.meta_path(&context.rel_path)
        } else {
            master_p
        };
        if let Ok(m) = fs::metadata(check_path) {
            let ft = get_metadata_times(&m);
            file_info.creation_time = ft.creation_time;
            file_info.last_access_time = ft.last_access_time;
            file_info.last_write_time = ft.last_write_time;
            file_info.change_time = ft.change_time;
        }

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

        if !context.is_striped {
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
            let part_filename = format!("{}.part{}", context.rel_path, slice.part_index);
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
        let (total_bytes, free_bytes) = if self.custom_size_gb > 0 {
            let total = self.custom_size_gb * 1024 * 1024 * 1024;
            (total, total)
        } else {
            let mut member_free_spaces = Vec::new();
            for member in self.members.iter() {
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
            (physical_free, physical_free)
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

        if marker.is_none()
            && let Ok(dir_lock) = dir_buf.acquire(true, None)
        {
            let mut dot = DirInfo::<255>::new();
            {
                let info = dot.file_info_mut();
                info.file_attributes = 0x10;
                info.index_number = 0;
                info.hard_links = 0;
            }
            let _ = dot.set_name(".");
            let _ = dir_lock.write(&mut dot);

            let mut dotdot = DirInfo::<255>::new();
            {
                let info = dotdot.file_info_mut();
                info.file_attributes = 0x10;
                info.index_number = 0;
                info.hard_links = 0;
            }
            let _ = dotdot.set_name("..");
            let _ = dir_lock.write(&mut dotdot);

            let physical_dir = self.master().join(&context.rel_path);

            if let Ok(entries) = fs::read_dir(&physical_dir) {
                let mut items = Vec::new();

                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();

                    if name.contains(".part") {
                        continue;
                    }

                    if name.ends_with(".symstripe_meta") {
                        let clean_name = name.trim_end_matches(".symstripe_meta").to_string();
                        let size = fs::read_to_string(entry.path())
                            .ok()
                            .and_then(|s| s.trim().parse::<u64>().ok())
                            .unwrap_or(0);
                        let ft = entry.metadata().ok().map(|m| get_metadata_times(&m));
                        items.push((clean_name, false, size, ft));
                    } else if let Ok(meta) = entry.metadata() {
                        let is_dir = meta.is_dir();
                        let size = if is_dir { 0 } else { meta.len() };
                        let ft = Some(get_metadata_times(&meta));
                        items.push((name, is_dir, size, ft));
                    }
                }

                items.sort_by_key(|a| a.0.to_lowercase());

                for (name, is_dir, size, ft_opt) in items {
                    let mut dir_info = DirInfo::<255>::new();
                    {
                        let info = dir_info.file_info_mut();
                        info.file_size = size;
                        info.allocation_size = if is_dir { 0 } else { (size + 4095) & !4095 };
                        info.file_attributes = if is_dir { 0x10 } else { 0x80 };
                        info.hard_links = 0;
                        info.index_number = 0;

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
        }

        Ok(dir_buf.read(marker, buffer))
    }
}

pub struct VfsPoolSession {
    _host: FileSystemHost<SymStripePoolFs>,
}

impl VfsPoolSession {
    pub fn rescan(&self) {}
}

pub fn mount_storage_pool(
    mount_target: &str,
    members: &[PathBuf],
    policy: i32,
    stripe_size_bytes: u64,
    custom_size_gb: u64,
    logger: Option<VfsLogCallback>,
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

    let pool_fs = SymStripePoolFs::new(
        members.to_vec(),
        policy,
        stripe_size_bytes,
        custom_size_gb,
        logger,
    );

    let mut volume_params = VolumeParams::new();
    volume_params.filesystem_name("NTFS");
    volume_params.volume_serial_number(0x594D5354);
    volume_params.case_preserved_names(true);
    volume_params.case_sensitive_search(false);
    volume_params.unicode_on_disk(true);
    volume_params.flush_and_purge_on_cleanup(true);
    volume_params.file_info_timeout(1000);
    volume_params.post_cleanup_when_modified_only(true);

    let mut host = FileSystemHost::new(volume_params, pool_fs)
        .map_err(|e| format!("Failed to create FileSystemHost: {:?}", e))?;

    host.mount(mount_target)
        .map_err(|e| format!("Failed to mount pool to '{}': {:?}", mount_target, e))?;

    FileSystemHost::<SymStripePoolFs>::start(&mut host)
        .map_err(|e| format!("Failed to start WinFsp dispatcher: {:?}", e))?;

    Ok(VfsPoolSession { _host: host })
}
