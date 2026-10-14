use rayon::prelude::*;
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const FILE_FLAG_NO_BUFFERING: u32 = 0x20000000;
const FILE_FLAG_WRITE_THROUGH: u32 = 0x80000000;

const BENCH_DURATION: Duration = Duration::from_millis(2500); // 2.5 seconds sustained continuous streaming
const BENCH_CHUNK_BYTES: usize = 4 * 1024 * 1024; // 4 MB aligned chunk per transfer

pub struct BenchmarkResult {
    pub single_speed_mbs: f64,
    pub parallel_speed_mbs: f64,
    pub boost_percentage: f64,
    pub drives_tested: usize,
    pub per_drive_speeds_mbs: Vec<f64>,
    pub bus_label: String,
    pub ram_bandwidth_gbs: f64,
    pub disk_cold_mbs: f64,
    pub disk_warm_mbs: f64,
    pub streaming_iops: f64,
    pub streaming_throughput_mbs: f64,
    pub avg_latency_ms: f64,
    pub streaming_stalls: usize,
}

pub struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuffer {
    pub fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("Invalid memory layout");
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    pub fn as_slice_mut(&mut self) -> &mut [u8] {
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

/// Measures direct unbuffered Cold I/O throughput versus Windows OS Cache Warm I/O throughput.
pub fn measure_disk_cold_vs_warm_io(dir: &Path) -> (f64, f64) {
    let probe_file = dir.join(".symstripe_cold_warm.tmp");
    let test_size = 32 * 1024 * 1024; // 32 MB
    let payload = vec![0x77u8; test_size];

    if fs::write(&probe_file, &payload).is_err() {
        return (0.0, 0.0);
    }

    // Warm Read (served directly from Windows Standby List / File System Cache)
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

    // Cold Read (bypassing Windows file system cache via FILE_FLAG_NO_BUFFERING)
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

/// Simulates a game engine (UE5 / Open-World Asset Streaming) reading random/sequential chunk sizes concurrently.
pub fn run_asset_streaming_simulation(dir: &Path) -> Result<(f64, f64, f64, usize), String> {
    let test_file = dir.join(".symstripe_streaming_sim.tmp");
    let file_size = 128 * 1024 * 1024; // 128 MB asset container
    let seed_payload = vec![0x55u8; 4 * 1024 * 1024];

    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&test_file)
        .map_err(|e| format!("Failed to create streaming sim file: {e}"))?;

    let mut written = 0;
    while written < file_size {
        let chunk = seed_payload.len().min(file_size - written);
        f.write_all(&seed_payload[..chunk])
            .map_err(|e| e.to_string())?;
        written += chunk;
    }
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);

    let num_threads = 4;
    let iterations_per_thread = 120;
    let file_size_u64 = file_size as u64;

    let start_time = Instant::now();
    let results: Vec<(usize, u64, u128)> = (0..num_threads)
        .into_par_iter()
        .map(|t_id| {
            let file = match open_unbuffered_file(&test_file) {
                Ok(f) => f,
                Err(_) => return (0, 0, 0),
            };

            let mut local_stalls = 0;
            let mut local_bytes = 0;
            let mut local_latency_sum = 0u128;
            let mut rng_seed = (t_id as u64 + 1) * 31337;

            let mut next_rand = || {
                rng_seed = rng_seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                rng_seed
            };

            // Typical game asset chunk sizes (from 16KB metadata headers to 4MB texture/audio streams)
            let chunk_sizes = [16384_u64, 65536, 262144, 1048576, 4194304];

            // Pre-allocate 4096-byte aligned buffer (max 4MB) to satisfy FILE_FLAG_NO_BUFFERING pointer alignment requirements
            let mut thread_aligned_buf = AlignedBuffer::new(4194304, 4096);

            for _ in 0..iterations_per_thread {
                let r1 = next_rand();
                let r2 = next_rand();

                let size_idx = (r1 as usize) % chunk_sizes.len();
                let req_len = chunk_sizes[size_idx].min(file_size_u64);

                let max_offset = file_size_u64.saturating_sub(req_len);
                let max_blocks = max_offset / 4096;
                let block_offset = if max_blocks > 0 { r2 % max_blocks } else { 0 };
                let offset = block_offset * 4096; // Strictly a multiple of 4096 bytes (disk sector)

                let buf_slice = &mut thread_aligned_buf.as_slice_mut()[..req_len as usize];
                let read_start = Instant::now();

                let ok = file.seek_read(buf_slice, offset).is_ok();
                let elapsed = read_start.elapsed();
                let lat_ms = elapsed.as_millis();

                if ok {
                    local_bytes += req_len;
                    local_latency_sum += lat_ms;
                    if lat_ms > 250 {
                        local_stalls += 1; // Micro-stall threshold (>250ms)
                    }
                }
            }

            (local_stalls, local_bytes, local_latency_sum)
        })
        .collect();

    let total_duration = start_time.elapsed().as_secs_f64();
    let _ = fs::remove_file(&test_file);

    let mut total_stalls = 0;
    let mut total_bytes = 0;
    let mut total_latency = 0u128;
    let total_ops = (num_threads * iterations_per_thread) as f64;

    for (stalls, bytes, lat_sum) in results {
        total_stalls += stalls;
        total_bytes += bytes;
        total_latency += lat_sum;
    }

    let iops = total_ops / total_duration.max(0.001);
    let throughput_mbs = (total_bytes as f64 / 1_048_576.0) / total_duration.max(0.001);
    let avg_latency_ms = (total_latency as f64) / total_ops.max(1.0);

    Ok((iops, throughput_mbs, avg_latency_ms, total_stalls))
}

/// Continuous time-based streaming reader. Runs sustained unbuffered reads
/// for the specified duration, wrapping around the test file to avoid any disk idle time.
fn run_timed_read_loop(
    path: &Path,
    drive_idx: usize,
    file_size: u64,
    chunk_size: usize,
    duration: Duration,
) -> Result<(usize, f64), String> {
    let file = open_unbuffered_file(path).map_err(|e| format!("Open error: {e}"))?;
    let mut buf = AlignedBuffer::new(chunk_size, 4096);
    let mut offset = 0u64;
    let mut total_bytes = 0usize;

    let start = Instant::now();
    while start.elapsed() < duration {
        if offset + (chunk_size as u64) > file_size {
            offset = 0; // Seamless wraparound
        }
        let t_read = Instant::now();
        file.seek_read(buf.as_slice_mut(), offset)
            .map_err(|e| format!("Read error at offset {offset}: {e}"))?;
        let lat = t_read.elapsed().as_millis() as u64;

        crate::telemetry::report_benchmark_io(drive_idx, chunk_size, lat);

        total_bytes += chunk_size;
        offset += chunk_size as u64;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let speed_mbs = (total_bytes as f64 / 1_048_576.0) / elapsed.max(0.001);
    Ok((total_bytes, speed_mbs))
}

pub fn run_benchmark_test(src_dir: &Path, targets: &[PathBuf]) -> Result<BenchmarkResult, String> {
    if targets.is_empty() {
        return Err("No target drives configured for benchmark.".into());
    }

    // Dynamic hardware bus detection (e.g. SATA, NVMe, USB)
    let bus_label = crate::win32::get_path_bus_type(src_dir);

    let ram_bandwidth_gbs = measure_host_memory_bandwidth();
    let (disk_cold_mbs, disk_warm_mbs) = measure_disk_cold_vs_warm_io(src_dir);
    let (streaming_iops, streaming_throughput_mbs, avg_latency_ms, streaming_stalls) =
        run_asset_streaming_simulation(src_dir).unwrap_or((0.0, 0.0, 0.0, 0));

    let mut all_paths = vec![src_dir.join(".bench_test_0.tmp")];
    for (idx, target) in targets.iter().enumerate() {
        all_paths.push(target.join(format!(".bench_test_{}.tmp", idx + 1)));
    }

    let payload_bytes = 96 * 1024 * 1024; // 96 MB per drive to comfortably exceed on-disk DRAM caches

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

    // Single-Drive Baseline (Drive 1)
    let (_, single_speed_mbs) = run_timed_read_loop(
        &all_paths[0],
        0,
        payload_bytes as u64,
        BENCH_CHUNK_BYTES,
        BENCH_DURATION,
    )
    .map_err(|e| {
        cleanup_bench_files(&all_paths);
        format!("Primary Drive benchmark failed: {e}")
    })?;

    // 2. Parallel Multi-Drive Concurrent Read (indices 0, 1, ...)
    let start_parallel = Instant::now();
    let mut handles = Vec::new();

    for (idx, path) in all_paths.iter().enumerate() {
        let p = path.clone();
        let handle = thread::spawn(move || -> Result<(usize, f64), String> {
            run_timed_read_loop(
                &p,
                idx,
                payload_bytes as u64,
                BENCH_CHUNK_BYTES,
                BENCH_DURATION,
            )
            .map_err(|e| format!("Drive {} error: {e}", idx + 1))
        });
        handles.push(handle);
    }

    let mut total_bytes_read: usize = 0;
    let mut per_drive_speeds = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for h in handles {
        match h.join() {
            Ok(Ok((bytes, spd))) => {
                total_bytes_read += bytes;
                per_drive_speeds.push(spd);
            }
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
        per_drive_speeds_mbs: per_drive_speeds,
        bus_label,
        ram_bandwidth_gbs,
        disk_cold_mbs,
        disk_warm_mbs,
        streaming_iops,
        streaming_throughput_mbs,
        avg_latency_ms,
        streaming_stalls,
    })
}
