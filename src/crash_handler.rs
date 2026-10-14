use crate::config::get_config_dir;
use crate::win32::{
    CREATE_ALWAYS, CloseHandle, CreateFileW, FILE_ATTRIBUTE_NORMAL, GENERIC_WRITE,
    INVALID_HANDLE_VALUE, SetUnhandledExceptionFilter, WriteFile,
};
use std::ffi::c_void;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::panic;
use std::sync::OnceLock;

const EXCEPTION_EXECUTE_HANDLER: i32 = 1;

#[repr(C)]
struct ExceptionRecord {
    exception_code: u32,
    exception_flags: u32,
    exception_record: *mut ExceptionRecord,
    exception_address: *mut c_void,
    number_parameters: u32,
    exception_information: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    exception_record: *mut ExceptionRecord,
    context_record: *mut c_void,
}

// Pre-allocated static UTF-16 wide path to prevent any heap allocations during SEH handling
static PREALLOCATED_CRASH_LOG_PATH: OnceLock<Vec<u16>> = OnceLock::new();

pub fn setup_crash_handler() {
    // Pre-create crashes directory and cache the wide path while heap is healthy
    let crash_dir = get_config_dir().join("crashes");
    let _ = fs::create_dir_all(&crash_dir);
    let crash_file = crash_dir.join("symstripe_crash.log");

    let mut wide_path: Vec<u16> = crash_file.as_os_str().encode_wide().collect();
    wide_path.push(0);
    let _ = PREALLOCATED_CRASH_LOG_PATH.set(wide_path);

    // Rust Standard Panic Hook
    panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let panic_msg = format!(
            "RUST PANIC DETECTED:\n{}\n\nBACKTRACE:\n{}",
            info, backtrace
        );
        write_crash_report_fallback(&panic_msg);
    }));

    // Windows Native Structured Exception Handling (SEH)
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
    }
}

/// Zero-allocation stack buffer formatter for emergency crash dumping
struct StackWriter<'a> {
    buf: &'a mut [u8],
    cursor: usize,
}

impl<'a> StackWriter<'a> {
    #[inline]
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, cursor: 0 }
    }

    #[inline]
    fn write_bytes(&mut self, slice: &[u8]) {
        let len = slice.len().min(self.buf.len().saturating_sub(self.cursor));
        self.buf[self.cursor..self.cursor + len].copy_from_slice(&slice[..len]);
        self.cursor += len;
    }

    #[inline]
    fn write_hex(&mut self, mut val: u64, digits: usize) {
        let mut hex_tmp = [b'0'; 16];
        for i in (0..digits).rev() {
            let d = (val & 0xF) as u8;
            hex_tmp[i] = if d < 10 { b'0' + d } else { b'A' + (d - 10) };
            val >>= 4;
        }
        self.write_bytes(&hex_tmp[..digits]);
    }
}

/// Fully zero-allocation SEH top-level exception filter
unsafe extern "system" fn unhandled_exception_filter(raw_pointers: *mut c_void) -> i32 {
    unsafe {
        if raw_pointers.is_null() {
            return EXCEPTION_EXECUTE_HANDLER;
        }

        let pointers = raw_pointers as *mut ExceptionPointers;
        if (*pointers).exception_record.is_null() {
            return EXCEPTION_EXECUTE_HANDLER;
        }

        let Some(wide_path) = PREALLOCATED_CRASH_LOG_PATH.get() else {
            return EXCEPTION_EXECUTE_HANDLER;
        };

        let file_handle = CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );

        if file_handle == INVALID_HANDLE_VALUE {
            return EXCEPTION_EXECUTE_HANDLER;
        }

        let rec = &*(*pointers).exception_record;
        let code = rec.exception_code;
        let addr = rec.exception_address as usize;

        // Stack-only buffer: strictly zero heap allocations
        let mut raw_buf = [0u8; 1024];
        let mut writer = StackWriter::new(&mut raw_buf);

        writer.write_bytes(b"==================================================\r\n");
        writer.write_bytes(b"SYMSTRIPE EMERGENCY HARDWARE CRASH REPORT (SEH)\r\n");
        writer.write_bytes(b"Exception Code    : 0x");
        writer.write_hex(code as u64, 8);

        match code {
            0xC0000005 => writer.write_bytes(b" (STATUS_ACCESS_VIOLATION)\r\n"),
            0xC0000409 => writer.write_bytes(b" (STATUS_STACK_BUFFER_OVERRUN / FASTFAIL)\r\n"),
            0xC000001D => writer.write_bytes(b" (STATUS_ILLEGAL_INSTRUCTION)\r\n"),
            0xC0000094 => writer.write_bytes(b" (STATUS_INTEGER_DIVIDE_BY_ZERO)\r\n"),
            _ => writer.write_bytes(b" (UNKNOWN_NATIVE_EXCEPTION)\r\n"),
        }

        writer.write_bytes(b"Exception Address : 0x");
        writer.write_hex(addr as u64, 16);
        writer.write_bytes(b"\r\n==================================================\r\n");

        let mut written = 0u32;
        WriteFile(
            file_handle,
            writer.buf.as_ptr() as *const c_void,
            writer.cursor as u32,
            &mut written,
            std::ptr::null_mut(),
        );
        CloseHandle(file_handle);

        EXCEPTION_EXECUTE_HANDLER
    }
}

fn write_crash_report_fallback(details: &str) {
    let crash_dir = get_config_dir().join("crashes");
    let _ = fs::create_dir_all(&crash_dir);
    let crash_file = crash_dir.join("symstripe_crash.log");

    if let Ok(mut f) = fs::File::create(&crash_file) {
        use std::io::Write;
        let _ = write!(
            f,
            "==================================================\r\n\
             SYMSTRIPE HARDWARE CRASH REPORT (PANIC)\r\n\
             Timestamp  : {:?}\r\n\
             ==================================================\r\n\r\n\
             DETAILS:\r\n{}\r\n",
            std::time::SystemTime::now(),
            details
        );
    }
}
