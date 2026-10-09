use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

// Win32 file flags to bypass Windows RAM caching
const FILE_FLAG_NO_BUFFERING: u32 = 0x20000000;
const FILE_FLAG_WRITE_THROUGH: u32 = 0x80000000;

pub struct BenchmarkResult {
    pub single_speed_mbs: f64,
    pub parallel_speed_mbs: f64,
    pub boost_percentage: f64,
}

/// RAII wrapper for a 4K sector-aligned memory buffer required by FILE_FLAG_NO_BUFFERING
struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("Invalid memory layout");
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr, self.layout);
        }
    }
}

// Allows sending aligned buffers across thread boundaries
unsafe impl Send for AlignedBuffer {}

/// Opens a file for unbuffered reading, falling back to standard read if unsupported
fn open_unbuffered_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path)
        .or_else(|_| File::open(path))
}

pub fn run_benchmark_test(src_dir: &Path, target_dir: &Path) -> Result<BenchmarkResult, String> {
    let file_a_path = PathBuf::from(src_dir).join(".bench_test_A.tmp");
    let file_b_path = PathBuf::from(target_dir).join(".bench_test_B.tmp");

    let payload_bytes = 128 * 1024 * 1024; // 128 MB (sector-aligned: multiple of 4096)

    // Prepare 4K-aligned payload buffer
    let mut write_buffer = AlignedBuffer::new(payload_bytes, 4096);
    write_buffer.as_slice_mut().fill(0xAA);

    // 1. Write unbuffered test file to Primary Drive (E:)
    let write_opt_a = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(&file_a_path);

    if let Ok(mut fa) = write_opt_a {
        let _ = fa.write_all(write_buffer.as_slice_mut());
        let _ = fa.sync_all();
    } else {
        return Err("Failed to write benchmark test block to Primary Drive.".into());
    }

    // 2. Write unbuffered test file to Target Drive (F:)
    let write_opt_b = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(&file_b_path);

    if let Ok(mut fb) = write_opt_b {
        let _ = fb.write_all(write_buffer.as_slice_mut());
        let _ = fb.sync_all();
    } else {
        let _ = fs::remove_file(&file_a_path);
        return Err("Failed to write benchmark test block to Target Drive.".into());
    }

    // Drop the write buffer before reading
    drop(write_buffer);

    // ------------------------------------------------------------------------
    // Step 1: Single-Drive Physical Read Test (Drive 1 only)
    // ------------------------------------------------------------------------
    let start_single = Instant::now();
    let mut single_buf = AlignedBuffer::new(payload_bytes, 4096);

    if let Ok(mut fa) = open_unbuffered_file(&file_a_path) {
        let _ = fa.read_exact(single_buf.as_slice_mut());
    } else {
        let _ = fs::remove_file(&file_a_path);
        let _ = fs::remove_file(&file_b_path);
        return Err("Failed to read from Primary Drive.".into());
    }

    let single_duration = start_single.elapsed().as_secs_f64();
    let single_speed_mbs = 128.0 / single_duration.max(0.001);

    drop(single_buf);

    // ------------------------------------------------------------------------
    // Step 2: Parallel Dual-Drive Physical Read Test (Simultaneous I/O)
    // ------------------------------------------------------------------------
    let p1 = file_a_path.clone();
    let p2 = file_b_path.clone();

    let start_parallel = Instant::now();

    let handle1 = thread::spawn(move || {
        let mut buf = AlignedBuffer::new(payload_bytes, 4096);
        if let Ok(mut f) = open_unbuffered_file(&p1) {
            let _ = f.read_exact(buf.as_slice_mut());
        }
    });

    let handle2 = thread::spawn(move || {
        let mut buf = AlignedBuffer::new(payload_bytes, 4096);
        if let Ok(mut f) = open_unbuffered_file(&p2) {
            let _ = f.read_exact(buf.as_slice_mut());
        }
    });

    let _ = handle1.join();
    let _ = handle2.join();

    let parallel_duration = start_parallel.elapsed().as_secs_f64();
    let parallel_speed_mbs = 256.0 / parallel_duration.max(0.001);

    // Clean up temporary benchmark test files
    let _ = fs::remove_file(&file_a_path);
    let _ = fs::remove_file(&file_b_path);

    let boost_percentage =
        ((parallel_speed_mbs - single_speed_mbs) / single_speed_mbs.max(1.0)) * 100.0;

    Ok(BenchmarkResult {
        single_speed_mbs,
        parallel_speed_mbs,
        boost_percentage,
    })
}
