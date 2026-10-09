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

// Native Windows kernel error codes (NTSTATUS)
const STATUS_INVALID_HANDLE: i32 = 0xC0000008_u32 as i32;
const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC0000010_u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC0000022_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034_u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185_u32 as i32;

/// Represents an indexed physical file or folder mapped into the virtual union namespace.
#[derive(Clone, Debug)]
pub struct VirtualFileRecord {
    pub host_physical_path: PathBuf,
    pub is_dir: bool,
    pub file_size: u64,
}

/// Execution context for an opened virtual file or directory handle.
pub struct VfsFileHandle {
    pub file: Mutex<Option<File>>,
    pub is_dir: bool,
    pub record: VirtualFileRecord,
    pub dir_buffer: Mutex<DirBuffer>,
}

pub struct SymStripeUnionFs {
    /// Normalized virtual path table (e.g., "\content\paks\pakchunk0-windows.ucas" -> physical PathBuf)
    pub virtual_table: RwLock<HashMap<String, VirtualFileRecord>>,
}

impl SymStripeUnionFs {
    pub fn new() -> Self {
        Self {
            virtual_table: RwLock::new(HashMap::new()),
        }
    }

    /// Normalizes raw UTF-16 WinFsp path into a lowercase root-anchored lookup key.
    fn normalize_path(raw_path: &U16CStr) -> String {
        let string_path = raw_path.to_string_lossy();
        if string_path == "\\" || string_path.is_empty() {
            return "\\".to_string();
        }
        format!("\\{}", string_path.trim_start_matches('\\').to_lowercase())
    }

    /// Recursively indexes Secondary target drives first, then overlays the Primary drive
    /// so local files always take precedence over relocated copies.
    pub fn build_union_table(&self, primary_root: &Path, secondary_roots: &[PathBuf]) {
        let mut table = self.virtual_table.write().unwrap();
        table.clear();

        // Register root directory
        table.insert(
            "\\".to_string(),
            VirtualFileRecord {
                host_physical_path: primary_root.to_path_buf(),
                is_dir: true,
                file_size: 0,
            },
        );

        // 1. Index secondary drives (where large .ucas / asset containers were relocated)
        for target in secondary_roots {
            for entry in walkdir::WalkDir::new(target)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if let Ok(rel) = entry.path().strip_prefix(target)
                    && let Ok(meta) = fs::metadata(entry.path())
                {
                    let key = format!("\\{}", rel.to_string_lossy().to_lowercase());
                    table.insert(
                        key,
                        VirtualFileRecord {
                            host_physical_path: entry.path().to_path_buf(),
                            is_dir: meta.is_dir(),
                            file_size: meta.len(),
                        },
                    );
                }
            }
        }

        // 2. Overlay primary source directory (local files take highest priority)
        for entry in walkdir::WalkDir::new(primary_root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if let Ok(rel) = entry.path().strip_prefix(primary_root)
                && let Ok(meta) = fs::metadata(entry.path())
            {
                let key = format!("\\{}", rel.to_string_lossy().to_lowercase());
                table.insert(
                    key,
                    VirtualFileRecord {
                        host_physical_path: entry.path().to_path_buf(),
                        is_dir: meta.is_dir(),
                        file_size: meta.len(),
                    },
                );
            }
        }
    }
}

// Implementation of WinFsp 0.13 FileSystemContext trait
impl FileSystemContext for SymStripeUnionFs {
    type FileContext = VfsFileHandle;

    fn get_security_by_name(
        &self,
        _file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        // Report genuine standard file attributes with NO REPARSE POINTS.
        // This eliminates DirectStorage BypassIO reparse rejections and prevents IoDispatcher memory-mapping crashes.
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

        let file_handle = if !record.is_dir {
            Some(
                File::open(&record.host_physical_path)
                    .map_err(|_| FspError::NTSTATUS(STATUS_ACCESS_DENIED))?,
            )
        } else {
            None
        };

        Ok(VfsFileHandle {
            file: Mutex::new(file_handle),
            is_dir: record.is_dir,
            record,
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

        let mut guard = context.file.lock().unwrap();
        if let Some(file) = guard.as_mut() {
            file.seek(SeekFrom::Start(offset))
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let bytes_read = file
                .read(buffer)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            Ok(bytes_read as u32)
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_HANDLE))
        }
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        out_volume_info.total_size = 4 * 1024 * 1024 * 1024 * 1024; // 4 TB virtual capacity
        out_volume_info.free_size = 1024 * 1024 * 1024 * 1024;
        out_volume_info.set_volume_label("SymStripe VFS");
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

/// Active WinFsp mounting session handle.
pub struct VfsMountSession {
    _host: FileSystemHost<SymStripeUnionFs>,
}

/// Mounts the virtual union filesystem at the specified target (drive letter such as "Z:" or empty directory).
pub fn mount_union_vfs(
    mount_target: &str,
    primary_dir: &Path,
    secondary_dirs: &[PathBuf],
) -> Result<VfsMountSession, String> {
    // 1. Initialize WinFsp kernel runtime
    let _init_token = winfsp::winfsp_init().map_err(|e| {
        format!(
            "WinFsp runtime initialization failed ({:?}). Ensure WinFsp is installed via official MSI installer.",
            e
        )
    })?;

    // 2. Build multi-drive union namespace table
    let union_fs = SymStripeUnionFs::new();
    union_fs.build_union_table(primary_dir, secondary_dirs);

    let mut volume_params = VolumeParams::new();
    volume_params.filesystem_name("SymStripe");
    volume_params.read_only_volume(true); // Protect game assets from accidental modification

    // 3. Create host and start kernel dispatch loop
    let mut host = FileSystemHost::new(volume_params, union_fs)
        .map_err(|e| format!("Failed to create FileSystemHost: {:?}", e))?;

    host.mount(mount_target)
        .map_err(|e| format!("Failed to mount VFS to '{}': {:?}", mount_target, e))?;

    FileSystemHost::<SymStripeUnionFs>::start(&mut host)
        .map_err(|e| format!("Failed to start WinFsp dispatcher: {:?}", e))?;

    Ok(VfsMountSession { _host: host })
}
