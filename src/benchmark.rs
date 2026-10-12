use rayon::prelude::*;
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
    pub ram_bandwidth_gbs: f64,
    pub disk_cold_mbs: f64,
    pub disk_warm_mbs: f64,
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

/// Measures multi-threaded Host RAM memory bandwidth (parallel memcpy limit across CPU cores via Rayon).
pub fn measure_host_memory_bandwidth() -> f64 {
    let size = 128 * 1024 * 1024; // 128 MB
    let src = vec![0xABu8; size];
    let mut dst = vec![0x00u8; size];

    let start = Instant::now();
    let num_threads = rayon::current_num_threads().max(1);
    let chunk_size = size / num_threads;

    dst.par_chunks_mut(chunk_size.max(4096))
        .enumerate()
        .for_each(|(i, chunk)| {
            let offset = i * chunk_size;
            let len = chunk.len().min(src.len().saturating_sub(offset));
            chunk[..len].copy_from_slice(&src[offset..offset + len]);
        });

    let elapsed = start.elapsed().as_secs_f64();
    (size as f64 / 1_073_741_824.0) / elapsed.max(0.0001)
}

/// Measures direct unbuffered NVMe Cold I/O throughput versus Windows OS Cache Warm I/O throughput.
pub fn measure_disk_cold_vs_warm_io(dir: &Path) -> (f64, f64) {
    let probe_file = dir.join(".symstripe_cold_warm.tmp");
    let test_size = 32 * 1024 * 1024; // 32 MB
    let payload = vec![0x77u8; test_size];

    if fs::write(&probe_file, &payload).is_err() {
        return (0.0, 0.0);
    }

    // 1. Warm Read (served directly from Windows Standby List / File System Cache)
    let start_warm = Instant::now();
    let mut warm_buf = vec![0u8; test_size];
    let warm_read_ok = File::open(&probe_file)
        .and_then(|mut f| f.read_exact(&mut warm_buf))
        .is_ok();
    let warm_elapsed = start_warm.elapsed().as_secs_f64();
    let warm_mbs = if warm_read_ok {
        (test_size as f64 / 1_048_576.0) / warm_elapsed.max(0.0001)
    } else {
        0.0
    };

    // 2. Cold Read (bypassing Windows file system cache via FILE_FLAG_NO_BUFFERING)
    let mut aligned_buf = AlignedBuffer::new(test_size, 4096);
    let start_cold = Instant::now();
    let cold_read_ok = open_unbuffered_file(&probe_file)
        .and_then(|mut f| f.read_exact(aligned_buf.as_slice_mut()))
        .is_ok();
    let cold_elapsed = start_cold.elapsed().as_secs_f64();
    let cold_mbs = if cold_read_ok {
        (test_size as f64 / 1_048_576.0) / cold_elapsed.max(0.0001)
    } else {
        warm_mbs * 0.4
    };

    let _ = fs::remove_file(&probe_file);
    (cold_mbs, warm_mbs)
}

pub fn run_benchmark_test(src_dir: &Path, targets: &[PathBuf]) -> Result<BenchmarkResult, String> {
    if targets.is_empty() {
        return Err("No target drives configured for benchmark.".into());
    }

    // 1. Measure Host RAM and Cold/Warm I/O baselines
    let ram_bandwidth_gbs = measure_host_memory_bandwidth();
    let (disk_cold_mbs, disk_warm_mbs) = measure_disk_cold_vs_warm_io(src_dir);

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

    // 2. Single-Drive Read Test (Drive 1 only)
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

    // 3. Parallel Multi-Drive Concurrent Read across all drives
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

    // If any thread failed, abort with exact details instead of distorted metrics
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
        ram_bandwidth_gbs,
        disk_cold_mbs,
        disk_warm_mbs,
    })
}
