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

fn cleanup_bench_files(paths: &[PathBuf]) {
    for p in paths {
        let _ = fs::remove_file(p);
    }
}

pub fn run_benchmark_test(src_dir: &Path, targets: &[PathBuf]) -> Result<BenchmarkResult, String> {
    if targets.is_empty() {
        return Err("No target drives configured for benchmark.".into());
    }

    let mut all_paths = vec![src_dir.join(".bench_test_0.tmp")];
    for (idx, target) in targets.iter().enumerate() {
        all_paths.push(target.join(format!(".bench_test_{}.tmp", idx + 1)));
    }

    let payload_bytes = 128 * 1024 * 1024; // 128 MB

    let mut write_buffer = AlignedBuffer::new(payload_bytes, 4096);
    write_buffer.as_slice_mut().fill(0xAA);

    // Create unbuffered test files on all drives in the array
    for (i, path) in all_paths.iter().enumerate() {
        let write_opt = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_WRITE_THROUGH)
            .open(path);

        if let Ok(mut f) = write_opt {
            if f.write_all(write_buffer.as_slice_mut()).is_err() || f.sync_all().is_err() {
                cleanup_bench_files(&all_paths[..=i]);
                return Err(format!("Failed to write test block to Drive {}.", i + 1));
            }
        } else {
            cleanup_bench_files(&all_paths[..i]);
            return Err(format!(
                "Failed to create benchmark test file on Drive {}.",
                i + 1
            ));
        }
    }

    drop(write_buffer);

    // 1. Single-Drive Read Test (Drive 1 only)
    let start_single = Instant::now();
    let mut single_buf = AlignedBuffer::new(payload_bytes, 4096);

    let single_read_bytes = match open_unbuffered_file(&all_paths[0]) {
        Ok(mut fa) => match fa.read_exact(single_buf.as_slice_mut()) {
            Ok(_) => payload_bytes,
            Err(e) => {
                cleanup_bench_files(&all_paths);
                return Err(format!("Failed reading from Primary Drive: {e}"));
            }
        },
        Err(e) => {
            cleanup_bench_files(&all_paths);
            return Err(format!("Failed to open Primary Drive test file: {e}"));
        }
    };

    let single_duration = start_single.elapsed().as_secs_f64();
    let single_speed_mbs =
        (single_read_bytes as f64 / (1024.0 * 1024.0)) / single_duration.max(0.001);
    drop(single_buf);

    // 2. Parallel Multi-Drive Concurrent Read across all drives
    let start_parallel = Instant::now();
    let mut handles = Vec::new();

    for (idx, path) in all_paths.iter().enumerate() {
        let p = path.clone();
        let handle = thread::spawn(move || -> Result<usize, String> {
            let mut buf = AlignedBuffer::new(payload_bytes, 4096);
            let mut f = open_unbuffered_file(&p)
                .map_err(|e| format!("Drive {} open error: {e}", idx + 1))?;
            f.read_exact(buf.as_slice_mut())
                .map_err(|e| format!("Drive {} read error: {e}", idx + 1))?;
            Ok(payload_bytes)
        });
        handles.push(handle);
    }

    let mut total_bytes_read: usize = 0;
    let mut errors: Vec<String> = Vec::new();

    for h in handles {
        match h.join() {
            Ok(Ok(bytes)) => total_bytes_read += bytes,
            Ok(Err(err_msg)) => errors.push(err_msg),
            Err(_) => errors.push("Thread panicked during disk read".into()),
        }
    }

    // Always clean up test files
    cleanup_bench_files(&all_paths);

    // If any thread failed, abort with exact details instead of returning distorted metrics
    if !errors.is_empty() {
        return Err(format!(
            "Parallel benchmark failed on {} drive(s): {}",
            errors.len(),
            errors.join("; ")
        ));
    }

    let parallel_duration = start_parallel.elapsed().as_secs_f64();
    let total_mb_read = total_bytes_read as f64 / (1024.0 * 1024.0);
    let parallel_speed_mbs = total_mb_read / parallel_duration.max(0.001);

    let boost_percentage =
        ((parallel_speed_mbs - single_speed_mbs) / single_speed_mbs.max(1.0)) * 100.0;

    Ok(BenchmarkResult {
        single_speed_mbs,
        parallel_speed_mbs,
        boost_percentage,
        drives_tested: all_paths.len(),
    })
}
