use std::ffi::{OsStr, c_void};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use winreg::RegKey;
use winreg::enums::*;

pub type Handle = *mut c_void;
pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

pub const FILE_SHARE_READ: u32 = 0x00000001;
pub const FILE_SHARE_WRITE: u32 = 0x00000002;
pub const OPEN_EXISTING: u32 = 3;
pub const CREATE_ALWAYS: u32 = 2;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x00000080;
pub const GENERIC_READ: u32 = 0x80000000;
pub const GENERIC_WRITE: u32 = 0x40000000;

pub const FILE_FLAG_NO_BUFFERING: u32 = 0x20000000;
pub const FILE_FLAG_WRITE_THROUGH: u32 = 0x80000000;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;

pub const ERROR_IO_PENDING: u32 = 997;

pub const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D1400;
pub const IOCTL_STORAGE_PREDICT_FAILURE: u32 = 0x002D1100;

pub const HKEY_LOCAL_MACHINE: *mut c_void = (0x80000002_u32 as i32 as isize) as *mut c_void;
pub const KEY_READ: u32 = 0x20019;

pub const PROGRESS_CONTINUE: u32 = 0;
pub const PROGRESS_CANCEL: u32 = 1;

pub const PROCESS_POWER_THROTTLING: u32 = 4;
pub const PROCESS_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;
pub const PROCESS_POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;
pub const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x00008000;

pub type PVectoredExceptionHandler = unsafe extern "system" fn(pointers: *mut c_void) -> i32;

#[repr(C)]
#[allow(clippy::upper_case_acronyms)]
pub struct OVERLAPPED {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub h_event: Handle,
}

#[allow(non_camel_case_types)]
pub type LPPROGRESS_ROUTINE = Option<
    unsafe extern "system" fn(
        total_file_size: i64,
        total_bytes_transferred: i64,
        stream_size: i64,
        stream_bytes_transferred: i64,
        stream_number: u32,
        callback_reason: u32,
        source_file: Handle,
        destination_file: Handle,
        data: *mut c_void,
    ) -> u32,
>;

#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[repr(C)]
struct ProcessPowerThrottlingState {
    version: u32,
    control_mask: u32,
    state_mask: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetLogicalDrives() -> u32;
    pub fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> i32;
    pub fn GetVolumePathNameW(
        lpszFileName: *const u16,
        lpszVolumePathName: *mut u16,
        cchBufferLength: u32,
    ) -> i32;
    pub fn GetVolumeNameForVolumeMountPointW(
        lpszVolumeMountPoint: *const u16,
        lpszVolumeName: *mut u16,
        cchBufferLength: u32,
    ) -> i32;
    pub fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: *mut c_void,
    ) -> Handle;
    pub fn WriteFile(
        hFile: Handle,
        lpBuffer: *const c_void,
        nNumberOfBytesToWrite: u32,
        lpNumberOfBytesWritten: *mut u32,
        lpOverlapped: *mut c_void,
    ) -> i32;
    pub fn ReadFile(
        hFile: Handle,
        lpBuffer: *mut c_void,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: *mut u32,
        lpOverlapped: *mut OVERLAPPED,
    ) -> i32;
    pub fn GetFileSizeEx(hFile: Handle, lpFileSize: *mut i64) -> i32;
    pub fn CreateIoCompletionPort(
        FileHandle: Handle,
        ExistingCompletionPort: Handle,
        CompletionKey: usize,
        NumberOfConcurrentThreads: u32,
    ) -> Handle;
    pub fn GetQueuedCompletionStatus(
        CompletionPort: Handle,
        lpNumberOfBytesTransferred: *mut u32,
        lpCompletionKey: *mut usize,
        lpOverlapped: *mut *mut OVERLAPPED,
        dwMilliseconds: u32,
    ) -> i32;
    pub fn CloseHandle(hObject: Handle) -> i32;
    pub fn DeviceIoControl(
        hDevice: Handle,
        dwIoControlCode: u32,
        lpInBuffer: *const c_void,
        nInBufferSize: u32,
        lpOutBuffer: *mut c_void,
        nOutBufferSize: u32,
        lpBytesReturned: *mut u32,
        lpOverlapped: *mut c_void,
    ) -> i32;
    pub fn CreateSymbolicLinkW(
        lpSymlinkFileName: *const u16,
        lpTargetFileName: *const u16,
        dwFlags: u32,
    ) -> u8;
    pub fn DeleteFileW(lpFileName: *const u16) -> i32;
    pub fn GetLastError() -> u32;
    pub fn SetUnhandledExceptionFilter(
        lpTopLevelExceptionFilter: Option<PVectoredExceptionHandler>,
    ) -> *mut c_void;
    fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
    fn CopyFileExW(
        lpExistingFileName: *const u16,
        lpNewFileName: *const u16,
        lpProgressRoutine: LPPROGRESS_ROUTINE,
        lpData: *mut c_void,
        pbCancel: *const i32,
        dwCopyFlags: u32,
    ) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn SetPriorityClass(hProcess: Handle, dwPriorityClass: u32) -> i32;
    fn SetProcessInformation(
        hProcess: Handle,
        ProcessInformationClass: u32,
        ProcessInformation: *const c_void,
        ProcessInformationSize: u32,
    ) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sd_str: *const u16,
        rev: u32,
        sd: *mut *mut c_void,
        sz: *mut u32,
    ) -> i32;
    pub fn LocalFree(mem: *mut c_void) -> *mut c_void;
    fn RegOpenKeyExW(
        hKey: *mut c_void,
        lpSubKey: *const u16,
        ulOptions: u32,
        samDesired: u32,
        phkResult: *mut *mut c_void,
    ) -> i32;
    fn RegQueryValueExW(
        hKey: *mut c_void,
        lpValueName: *const u16,
        lpReserved: *mut u32,
        lpType: *mut u32,
        lpData: *mut u8,
        lpcbData: *mut u32,
    ) -> i32;
    fn RegCloseKey(hKey: *mut c_void) -> i32;
}

pub fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn get_default_security_descriptor() -> &'static [u8] {
    static SD: OnceLock<Vec<u8>> = OnceLock::new();
    SD.get_or_init(|| {
        let sddl = to_wide_null("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)");
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

pub fn get_volume_guid_path(path: &Path) -> Option<PathBuf> {
    let path_str = path.to_str()?;
    if path_str.starts_with(r"\\?\Volume{") {
        return Some(path.to_path_buf());
    }

    let wide_path = to_wide_null(path_str);
    let mut mount_point = [0u16; 512];

    let ok_path = unsafe {
        GetVolumePathNameW(
            wide_path.as_ptr(),
            mount_point.as_mut_ptr(),
            mount_point.len() as u32,
        )
    };
    if ok_path == 0 {
        return None;
    }

    let mut volume_guid = [0u16; 128];
    let ok_guid = unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount_point.as_ptr(),
            volume_guid.as_mut_ptr(),
            volume_guid.len() as u32,
        )
    };
    if ok_guid == 0 {
        return None;
    }

    let mount_point_len = mount_point.iter().position(|&c| c == 0)?;
    let mount_point_str = String::from_utf16_lossy(&mount_point[..mount_point_len]);

    let guid_len = volume_guid.iter().position(|&c| c == 0)?;
    let guid_str = String::from_utf16_lossy(&volume_guid[..guid_len]);

    let rel = path_str
        .strip_prefix(&mount_point_str)
        .or_else(|| path_str.strip_prefix(mount_point_str.trim_end_matches('\\')))?;

    let clean_rel = rel.trim_start_matches('\\');
    let full_guid_path = if clean_rel.is_empty() {
        guid_str
    } else {
        format!("{}\\{}", guid_str.trim_end_matches('\\'), clean_rel)
    };

    Some(PathBuf::from(full_guid_path))
}

pub fn get_path_bus_type(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    let letter = path_str.chars().next().filter(|c| c.is_ascii_alphabetic());

    let Some(c) = letter else {
        return "Storage".to_string();
    };

    let drive_root = format!(r"\\.\{}:", c);
    let wide_path = to_wide_null(&drive_root);

    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return "Storage".to_string();
    }

    let query = StoragePropertyQuery {
        property_id: 0,
        query_type: 0,
        additional_parameters: [0],
    };

    let mut out_buf = [0u8; 1024];
    let mut bytes_returned = 0u32;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const c_void,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            out_buf.as_mut_ptr() as *mut c_void,
            out_buf.len() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    unsafe { CloseHandle(handle) };

    if ok != 0 && bytes_returned >= std::mem::size_of::<StorageDeviceDescriptorHeader>() as u32 {
        let header = unsafe { &*(out_buf.as_ptr() as *const StorageDeviceDescriptorHeader) };
        map_bus_type(header.bus_type).to_string()
    } else {
        "Storage".to_string()
    }
}

pub fn boost_process_priority() {
    unsafe {
        let process = GetCurrentProcess();
        SetPriorityClass(process, ABOVE_NORMAL_PRIORITY_CLASS);

        let throttling = ProcessPowerThrottlingState {
            version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: 0,
        };

        SetProcessInformation(
            process,
            PROCESS_POWER_THROTTLING,
            &throttling as *const _ as *const c_void,
            std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
        );
    }
}

pub fn get_system_ram_mb() -> u64 {
    let mut mem = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
        mem.total_phys / (1024 * 1024)
    } else {
        16384
    }
}

pub fn get_disk_space(path: &Path) -> Option<(u64, u64)> {
    let path_str = path.to_str()?;
    let wide = to_wide_null(path_str);
    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;

    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_avail, &mut total, &mut total_free) };

    if ok != 0 {
        Some((free_avail, total))
    } else {
        None
    }
}

pub fn get_free_disk_space_bytes(path: &Path) -> Option<u64> {
    get_disk_space(path).map(|(free, _)| free)
}

pub fn is_developer_mode_enabled() -> bool {
    let subkey = to_wide_null(r"SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock");
    let val_name = to_wide_null("AllowDevelopmentWithoutDevLicense");
    let mut hkey = std::ptr::null_mut();

    let res = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if res != 0 || hkey.is_null() {
        return false;
    }

    let mut data: u32 = 0;
    let mut data_size = std::mem::size_of::<u32>() as u32;
    let mut type_val: u32 = 0;

    let query_res = unsafe {
        RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            &mut type_val,
            &mut data as *mut u32 as *mut u8,
            &mut data_size,
        )
    };

    unsafe { RegCloseKey(hkey) };
    query_res == 0 && data == 1
}

pub fn test_symlink_privilege() -> Result<(), (u32, bool)> {
    let temp_dir = std::env::temp_dir();
    let target = temp_dir.join(".symstripe_probe_target.tmp");
    let link = temp_dir.join(".symstripe_probe_link.tmp");

    let _ = fs::write(&target, b"test");

    let wide_target = to_wide_null(&target.to_string_lossy());
    let wide_link = to_wide_null(&link.to_string_lossy());

    let success = unsafe { CreateSymbolicLinkW(wide_link.as_ptr(), wide_target.as_ptr(), 0x2) };

    let _ = fs::remove_file(&target);

    if success != 0 {
        let _ = unsafe { DeleteFileW(wide_link.as_ptr()) };
        Ok(())
    } else {
        let err = unsafe { GetLastError() };
        let dev_mode = is_developer_mode_enabled();
        Err((err, dev_mode))
    }
}

unsafe extern "system" fn copy_progress_bridge<F>(
    total_file_size: i64,
    total_bytes_transferred: i64,
    _stream_size: i64,
    _stream_bytes_transferred: i64,
    _stream_number: u32,
    _callback_reason: u32,
    _source_file: Handle,
    _destination_file: Handle,
    data: *mut c_void,
) -> u32
where
    F: FnMut(u64, u64) -> bool,
{
    if data.is_null() {
        return PROGRESS_CONTINUE;
    }

    let callback = unsafe { &mut *(data as *mut F) };
    let transferred = total_bytes_transferred.max(0) as u64;
    let total = total_file_size.max(0) as u64;

    if callback(transferred, total) {
        PROGRESS_CONTINUE
    } else {
        PROGRESS_CANCEL
    }
}

pub fn copy_file_with_progress<F>(src: &Path, dst: &Path, mut progress_fn: F) -> Result<u64, String>
where
    F: FnMut(u64, u64) -> bool,
{
    let src_wide = to_wide_null(&src.to_string_lossy());
    let dst_wide = to_wide_null(&dst.to_string_lossy());

    let data_ptr = &mut progress_fn as *mut F as *mut c_void;

    let res = unsafe {
        CopyFileExW(
            src_wide.as_ptr(),
            dst_wide.as_ptr(),
            Some(copy_progress_bridge::<F>),
            data_ptr,
            std::ptr::null(),
            0,
        )
    };

    if res != 0 {
        let size = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        Ok(size)
    } else {
        let err = unsafe { GetLastError() };
        Err(format!("CopyFileExW failed with Win32 Error: {}", err))
    }
}

pub fn check_winfsp_available() -> Result<(), String> {
    for key_path in [r"SOFTWARE\WinFsp", r"SOFTWARE\WOW6432Node\WinFsp"] {
        let wide_key = to_wide_null(key_path);
        let mut hkey = std::ptr::null_mut();
        let res = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                wide_key.as_ptr(),
                0,
                KEY_READ,
                &mut hkey,
            )
        };
        if res == 0 && !hkey.is_null() {
            unsafe { RegCloseKey(hkey) };
            return Ok(());
        }
    }

    let standard_paths = [
        r"C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll",
        r"C:\Program Files\WinFsp\bin\winfsp-x64.dll",
        r"C:\Windows\System32\winfsp-x64.dll",
    ];

    for p in standard_paths {
        if Path::new(p).exists() {
            return Ok(());
        }
    }

    Err("WinFsp runtime is not detected on this system!\n\n\
        Virtual Storage Pool (VFS RAID-0) requires WinFsp.\n\
        Please download and install WinFsp from https://winfsp.dev/ \n\
        (or via Windows Terminal: 'winget install WinFsp.WinFsp') and restart SymStripe."
        .to_string())
}

pub struct SmartDiskInfo {
    pub name: String,
    pub media_type: String,
    pub health_status: String,
    pub is_healthy: bool,
}

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

#[repr(C)]
struct StorageDeviceDescriptorHeader {
    version: u32,
    size: u32,
    device_type: u8,
    device_type_modifier: u8,
    removable_media: u8,
    command_queueing: u8,
    vendor_id_offset: u32,
    product_id_offset: u32,
    product_revision_offset: u32,
    serial_number_offset: u32,
    bus_type: u32,
    raw_properties_length: u32,
}

#[repr(C)]
struct StoragePredictFailure {
    predict_failure: u32,
    vendor_specific: [u8; 512],
}

fn extract_ascii_string(buf: &[u8], offset: u32) -> String {
    if offset == 0 || (offset as usize) >= buf.len() {
        return String::new();
    }
    let start = offset as usize;
    let bytes = &buf[start..];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn map_bus_type(bus_type: u32) -> &'static str {
    match bus_type {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD Card",
        13 => "MMC",
        14 => "Virtual Disk",
        16 => "Storage Space",
        17 => "NVMe",
        _ => "Disk",
    }
}

pub fn query_smart_disks() -> Vec<SmartDiskInfo> {
    let mut results = Vec::new();

    for drive_idx in 0..16 {
        let drive_path = format!(r"\\.\PhysicalDrive{}", drive_idx);
        let wide_path = to_wide_null(&drive_path);

        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            continue;
        }

        let query = StoragePropertyQuery {
            property_id: 0,
            query_type: 0,
            additional_parameters: [0],
        };

        let mut out_buf = [0u8; 1024];
        let mut bytes_returned = 0u32;

        let ok_desc = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as *const c_void,
                std::mem::size_of::<StoragePropertyQuery>() as u32,
                out_buf.as_mut_ptr() as *mut c_void,
                out_buf.len() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };

        if ok_desc != 0
            && bytes_returned >= std::mem::size_of::<StorageDeviceDescriptorHeader>() as u32
        {
            let header = unsafe { &*(out_buf.as_ptr() as *const StorageDeviceDescriptorHeader) };

            let vendor = extract_ascii_string(&out_buf, header.vendor_id_offset);
            let product = extract_ascii_string(&out_buf, header.product_id_offset);

            let disk_name = if !vendor.is_empty() && !product.contains(&vendor) {
                format!("{} {}", vendor, product)
            } else if !product.is_empty() {
                product
            } else {
                format!("Physical Drive {}", drive_idx)
            };

            let media_type = map_bus_type(header.bus_type).to_string();

            let mut predict_failure = StoragePredictFailure {
                predict_failure: 0,
                vendor_specific: [0; 512],
            };
            let mut predict_bytes = 0u32;

            let ok_predict = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_PREDICT_FAILURE,
                    std::ptr::null(),
                    0,
                    &mut predict_failure as *mut _ as *mut c_void,
                    std::mem::size_of::<StoragePredictFailure>() as u32,
                    &mut predict_bytes,
                    std::ptr::null_mut(),
                )
            };

            let (is_healthy, health_status) = if ok_predict != 0 {
                if predict_failure.predict_failure == 0 {
                    (true, "Healthy".to_string())
                } else {
                    (false, "SMART Alert: Predicted Failure".to_string())
                }
            } else {
                (true, "Healthy".to_string())
            };

            results.push(SmartDiskInfo {
                name: disk_name,
                media_type,
                health_status,
                is_healthy,
            });
        }

        unsafe { CloseHandle(handle) };
    }

    results
}

pub fn get_logical_volumes() -> Vec<(String, f64, f64)> {
    let mut volumes = Vec::new();
    let drives_mask = unsafe { GetLogicalDrives() };

    for i in 0..26 {
        if (drives_mask & (1 << i)) != 0 {
            let letter = (b'A' + i) as char;
            let root = format!("{}:\\", letter);
            let wide_root = to_wide_null(&root);

            let mut free_avail: u64 = 0;
            let mut total: u64 = 0;
            let mut total_free: u64 = 0;

            let ok = unsafe {
                GetDiskFreeSpaceExW(
                    wide_root.as_ptr(),
                    &mut free_avail,
                    &mut total,
                    &mut total_free,
                )
            };

            if ok != 0 {
                let total_gb = total as f64 / 1e9;
                let free_gb = total_free as f64 / 1e9;
                volumes.push((root, free_gb, total_gb));
            }
        }
    }
    volumes
}

pub fn set_auto_start(enable: bool) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((key, _)) = hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run") {
        if enable {
            if let Ok(exe_path) = std::env::current_exe() {
                let _ = key.set_value("SymStripe", &exe_path.to_string_lossy().to_string());
            }
        } else {
            let _ = key.delete_value("SymStripe");
        }
    }
}
