# SymStripe

High-throughput storage accelerator and user-mode VFS (WinFsp) for Windows. Aggregates mixed physical drives without repartitioning or hardware RAID.

---

## Operating Modes

### 1. In-Place Game Optimizer (Symlinks)
Offloads heavy game archives (`.ucas`, `.pak`, media) to secondary drives via transparent symlinks.
- **Volume GUID Anchoring:** Links target `\\?\Volume{GUID}\...` (immune to drive letter changes).
- **UE5 Safe Mode:** Retains bootstrap packages (`pakchunk0`) on primary; strictly co-locates `.utoc`/`.sig` with `.ucas`.
- **LPT Balancing & Recovery:** Capacity-aware distribution, atomic manifests, and `.backup` safety nets.

### 2. Virtual Storage Pool (VFS Z:)
Mounts multi-drive folders as a unified `Z:` drive.
- **Topologies:** RAID-0 (Striped), RAID-1 (Mirror), JBOD (Merged Capacity).
- **RAID-0 Asymmetric Striping:** Auto-calibrates speed weights (e.g., 2:1 schedule) so fast drives never wait for slow ones.
- **RAID-1 Smart Passthrough:** Uses **Least Queue Depth (LQD)** balancing and `FILE_FLAG_SEQUENTIAL_SCAN` for native OS readahead with zero-memory duplication.
- **I/O Engine:** `mimalloc` allocator, `xxh3_64` SIMD hashing, and **Persistent Handle Cache** (zero `CreateFileW` syscalls).
- **Hardware NCQ Throttling:** Limits queue depth (QD=4) per spindle to optimize magnetic head sweeps without thrashing.
- **RAM Cache:** $O(1)$ CLOCK byte-bounded eviction, IOCP asynchronous prefetching, and Single-Flight deduplication (anti-stampede).
- **Compression:** Zstd ATG (`WindowLog = 18`) & LZ4 for on-the-fly DirectStorage-grade asset unpacking.

---

## Diagnostics & System Features

- **Hardware Profiler:** Unbuffered 4KB-aligned I/O benchmark, Bus detection, and S.M.A.R.T. polling.
- **Zero-Alloc SEH Logger:** Native crash handler using pre-allocated paths to survive stack/heap corruption.
- **System Integration:** Silent boot Windows Service (`--install-service`), EcoQoS disabling, and Above Normal process priority.

---

## Requirements
- Windows 10/11 (x64)
- [WinFsp Runtime](https://winfsp.dev/)
- Rust 1.80+ / Slint 1.18
