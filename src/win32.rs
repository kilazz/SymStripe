use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
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

pub fn query_smart_disks() -> Vec<SmartDiskInfo> {
    let mut results = Vec::new();
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-PhysicalDisk | Select-Object FriendlyName, MediaType, HealthStatus | ConvertTo-Csv -NoTypeInformation",
        ])
        .output();

    if let Ok(out) = output {
        let csv = String::from_utf8_lossy(&out.stdout);
        for line in csv.lines().skip(1) {
            let parts: Vec<&str> = line.split(',').map(|s| s.trim_matches('"')).collect();
            if parts.len() >= 3 {
                let name = parts[0].to_string();
                let media = parts[1].to_string();
                let health = parts[2].to_string();
                let is_healthy = health.eq_ignore_ascii_case("Healthy");

                results.push(SmartDiskInfo {
                    name,
                    media_type: media,
                    health_status: health,
                    is_healthy,
                });
            }
        }
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
