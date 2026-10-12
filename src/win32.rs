use std::ffi::{OsStr, c_void};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use winreg::RegKey;
use winreg::enums::*;

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

#[allow(dead_code)]
const GENERIC_WRITE: u32 = 0x40000000;
const FILE_SHARE_READ: u32 = 0x00000001;
const FILE_SHARE_WRITE: u32 = 0x00000002;
#[allow(dead_code)]
const FILE_SHARE_DELETE: u32 = 0x00000004;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x00000080;
#[allow(dead_code)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
#[allow(dead_code)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D1400;
const IOCTL_STORAGE_PREDICT_FAILURE: u32 = 0x002D1100;

#[allow(dead_code)]
const FSCTL_SET_REPARSE_POINT: u32 = 0x000900A4;
#[allow(dead_code)]
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA0000003;

// HKEY_LOCAL_MACHINE: ((HKEY)(ULONG_PTR)((LONG)0x80000002))
const HKEY_LOCAL_MACHINE: *mut c_void = (0x80000002_u32 as i32 as isize) as *mut c_void;
const KEY_READ: u32 = 0x20019;

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

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> i32;
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: *mut c_void,
    ) -> Handle;
    fn CloseHandle(hObject: Handle) -> i32;
    fn DeviceIoControl(
        hDevice: Handle,
        dwIoControlCode: u32,
        lpInBuffer: *const c_void,
        nInBufferSize: u32,
        lpOutBuffer: *mut c_void,
        nOutBufferSize: u32,
        lpBytesReturned: *mut u32,
        lpOverlapped: *mut c_void,
    ) -> i32;
    fn CreateSymbolicLinkW(
        lpSymlinkFileName: *const u16,
        lpTargetFileName: *const u16,
        dwFlags: u32,
    ) -> u8;
    fn DeleteFileW(lpFileName: *const u16) -> i32;
    fn GetLastError() -> u32;
    fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
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

    // 0x2 = SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE
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

#[allow(dead_code)]
pub fn create_directory_junction(junction_path: &Path, target_dir: &Path) -> Result<(), String> {
    let target_abs = match fs::canonicalize(target_dir) {
        Ok(p) => p,
        Err(_) => target_dir.to_path_buf(),
    };

    let target_str = target_abs.to_string_lossy().to_string();
    let clean_target = target_str.strip_prefix(r"\\?\").unwrap_or(&target_str);

    let substitute_name = format!(r"\??\{}", clean_target);
    let print_name = clean_target.to_string();

    let substitute_w = to_wide_null(&substitute_name);
    let sub_bytes = (substitute_w.len() - 1) * 2;

    let print_w = to_wide_null(&print_name);
    let print_bytes = (print_w.len() - 1) * 2;

    if !junction_path.exists() {
        fs::create_dir_all(junction_path)
            .map_err(|e| format!("Failed to create junction folder: {e}"))?;
    }

    let wide_junction = to_wide_null(&junction_path.to_string_lossy());

    let handle = unsafe {
        CreateFileW(
            wide_junction.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(format!(
            "Failed to open junction directory handle (Win32 Error: {})",
            err
        ));
    }

    let path_buffer_len_bytes = (substitute_w.len() + print_w.len()) * 2;
    let reparse_data_len = 8 + path_buffer_len_bytes;
    let total_size = 8 + reparse_data_len;

    let mut buf = vec![0u8; total_size];

    buf[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_ne_bytes());
    buf[4..6].copy_from_slice(&(reparse_data_len as u16).to_ne_bytes());
    buf[6..8].copy_from_slice(&0u16.to_ne_bytes());

    buf[8..10].copy_from_slice(&0u16.to_ne_bytes());
    buf[10..12].copy_from_slice(&(sub_bytes as u16).to_ne_bytes());

    let print_offset = (substitute_w.len() * 2) as u16;
    buf[12..14].copy_from_slice(&print_offset.to_ne_bytes());
    buf[14..16].copy_from_slice(&(print_bytes as u16).to_ne_bytes());

    let path_start = 16;
    let sub_slice = unsafe {
        std::slice::from_raw_parts(substitute_w.as_ptr() as *const u8, substitute_w.len() * 2)
    };
    buf[path_start..path_start + sub_slice.len()].copy_from_slice(sub_slice);

    let print_start = path_start + sub_slice.len();
    let print_slice =
        unsafe { std::slice::from_raw_parts(print_w.as_ptr() as *const u8, print_w.len() * 2) };
    buf[print_start..print_start + print_slice.len()].copy_from_slice(print_slice);

    let mut bytes_returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr() as *const c_void,
            total_size as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    let err = if ok == 0 {
        unsafe { GetLastError() }
    } else {
        0
    };
    unsafe { CloseHandle(handle) };

    if ok != 0 {
        Ok(())
    } else {
        let _ = fs::remove_dir(junction_path);
        Err(format!(
            "DeviceIoControl(FSCTL_SET_REPARSE_POINT) failed with Win32 Error: {}",
            err
        ))
    }
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
