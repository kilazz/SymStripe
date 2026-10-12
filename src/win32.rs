use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

const FILE_SHARE_READ: u32 = 0x00000001;
const FILE_SHARE_WRITE: u32 = 0x00000002;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x00000080;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D1400;
const IOCTL_STORAGE_PREDICT_FAILURE: u32 = 0x002D1100;

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
}

pub fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn get_free_disk_space_bytes(path: &Path) -> Option<u64> {
    let path_str = path.to_str()?;
    let wide = to_wide_null(path_str);
    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;

    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_avail, &mut total, &mut total_free) };

    if ok != 0 { Some(free_avail) } else { None }
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

/// Instant native Win32 device query bypassing PowerShell entirely.
pub fn query_smart_disks() -> Vec<SmartDiskInfo> {
    let mut results = Vec::new();

    // Query physical drive devices PhysicalDrive0 through PhysicalDrive16
    for drive_idx in 0..16 {
        let drive_path = format!(r"\\.\PhysicalDrive{}", drive_idx);
        let wide_path = to_wide_null(&drive_path);

        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                0, // 0 gives metadata query access without requiring elevated debug privileges
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
            property_id: 0, // StorageDeviceProperty
            query_type: 0,  // PropertyStandardQuery
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

            // Query S.M.A.R.T. predictive failure health
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
