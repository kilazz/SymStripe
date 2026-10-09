use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

const FILE_FLAG_NO_BUFFERING: u32 = 0x20000000;
const FILE_FLAG_WRITE_THROUGH: u32 = 0x80000000;

pub struct BenchmarkResult {
    pub single_speed_mbs: f64,
    pub parallel_speed_mbs: f64,
    pub boost_percentage: f64,
    pub drives_tested: usize,
}

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

unsafe impl Send for AlignedBuffer {}

fn open_unbuffered_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path)
        .or_else(|_| File::open(path))
}

pub fn run_benchmark_test(src_dir: &Path, targets: &[PathBuf]) -> Result<BenchmarkResult, String> {
    if targets.is_empty() {
        return Err("No target drives configured for benchmark.".into());
    }

    let mut all_paths = vec![PathBuf::from(src_dir).join(".bench_test_0.tmp")];
    for (idx, target) in targets.iter().enumerate() {
        all_paths.push(target.join(format!(".bench_test_{}.tmp", idx + 1)));
    }

    let payload_bytes = 128 * 1024 * 1024; // 128 MB

    let mut write_buffer = AlignedBuffer::new(payload_bytes, 4096);
    write_buffer.as_slice_mut().fill(0xAA);

    // Create unbuffered test files on ALL drives in the array
    for (i, path) in all_paths.iter().enumerate() {
        let write_opt = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_WRITE_THROUGH)
            .open(path);

        if let Ok(mut f) = write_opt {
            let _ = f.write_all(write_buffer.as_slice_mut());
            let _ = f.sync_all();
        } else {
            for p in &all_paths[..i] {
                let _ = fs::remove_file(p);
            }
            return Err(format!("Failed to write test block to Drive {}.", i + 1));
        }
    }

    drop(write_buffer);

    // 1. Single-Drive Read (Drive 1 only)
    let start_single = Instant::now();
    let mut single_buf = AlignedBuffer::new(payload_bytes, 4096);

    if let Ok(mut fa) = open_unbuffered_file(&all_paths[0]) {
        let _ = fa.read_exact(single_buf.as_slice_mut());
    } else {
        for p in &all_paths {
            let _ = fs::remove_file(p);
        }
        return Err("Failed to read from Primary Drive.".into());
    }

    let single_duration = start_single.elapsed().as_secs_f64();
    let single_speed_mbs = 128.0 / single_duration.max(0.001);
    drop(single_buf);

    // 2. Parallel Multi-Drive Concurrent Read across ALL drives!
    let start_parallel = Instant::now();
    let mut handles = Vec::new();

    for path in &all_paths {
        let p = path.clone();
        let handle = thread::spawn(move || {
            let mut buf = AlignedBuffer::new(payload_bytes, 4096);
            if let Ok(mut f) = open_unbuffered_file(&p) {
                let _ = f.read_exact(buf.as_slice_mut());
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.join();
    }

    let parallel_duration = start_parallel.elapsed().as_secs_f64();
    let total_mb_read = 128.0 * (all_paths.len() as f64);
    let parallel_speed_mbs = total_mb_read / parallel_duration.max(0.001);

    for p in &all_paths {
        let _ = fs::remove_file(p);
    }

    let boost_percentage =
        ((parallel_speed_mbs - single_speed_mbs) / single_speed_mbs.max(1.0)) * 100.0;

    Ok(BenchmarkResult {
        single_speed_mbs,
        parallel_speed_mbs,
        boost_percentage,
        drives_tested: all_paths.len(),
    })
}
