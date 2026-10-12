use crate::config::get_config_dir;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::Write;
use std::panic;

// Win32 Top-Level Exception Filter return codes
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

type PVectoredExceptionHandler = unsafe extern "system" fn(pointers: *mut ExceptionPointers) -> i32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetUnhandledExceptionFilter(
        lpTopLevelExceptionFilter: Option<PVectoredExceptionHandler>,
    ) -> *mut c_void;
}

pub fn setup_crash_handler() {
    // 1. Rust Standard Panic Hook
    panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let panic_msg = format!(
            "RUST PANIC DETECTED:\n{}\n\nBACKTRACE:\n{}",
            info, backtrace
        );
        write_crash_report("RUST_PANIC", &panic_msg);
    }));

    // 2. Windows Native Structured Exception Handling (SEH)
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
    }
}

unsafe extern "system" fn unhandled_exception_filter(pointers: *mut ExceptionPointers) -> i32 {
    unsafe {
        if pointers.is_null() || (*pointers).exception_record.is_null() {
            return EXCEPTION_EXECUTE_HANDLER;
        }

        let rec = &*(*pointers).exception_record;
        let code = rec.exception_code;
        let addr = rec.exception_address;

        let code_str = match code {
            0xC0000005 => {
                "STATUS_ACCESS_VIOLATION (0xC0000005) - Invalid Memory Read/Write Pointer"
            }
            0xC0000409 => "STATUS_STACK_BUFFER_OVERRUN (0xC0000409) - Stack Corruption / FastFail",
            0xC000001D => "STATUS_ILLEGAL_INSTRUCTION (0xC000001D) - Unsupported CPU Instruction",
            0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO (0xC0000094)",
            _ => "UNKNOWN_NATIVE_WINDOWS_EXCEPTION",
        };

        let details = format!(
            "Exception Code    : 0x{:08X} ({})\n\
             Exception Address : {:p}\n\
             Thread Parameters : {:?}",
            code, code_str, addr, rec.exception_information
        );

        write_crash_report("WINDOWS_NATIVE_SEH_EXCEPTION", &details);

        // Return EXCEPTION_EXECUTE_HANDLER to prevent recursive invocation loops and cleanly terminate
        EXCEPTION_EXECUTE_HANDLER
    }
}

fn write_crash_report(crash_type: &str, details: &str) {
    let crash_dir = get_config_dir().join("crashes");
    let _ = fs::create_dir_all(&crash_dir);
    let crash_file = crash_dir.join("symstripe_crash.log");

    if let Ok(mut f) = File::create(&crash_file) {
        let sys_info = format!(
            "==================================================\n\
             SYMSTRIPE HARDWARE CRASH REPORT\n\
             Crash Type : {}\n\
             OS         : Windows NT (x86_64)\n\
             Timestamp  : {:?}\n\
             ==================================================\n\n\
             DETAILS:\n{}\n",
            crash_type,
            std::time::SystemTime::now(),
            details
        );
        let _ = f.write_all(sys_info.as_bytes());
        let _ = f.flush();
    }
}
