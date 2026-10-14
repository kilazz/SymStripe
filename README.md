# SymStripe

High-throughput storage accelerator and user-mode virtual filesystem (WinFsp) for Windows. Combines the throughput of mixed physical drives without repartitioning or hardware RAID.

---

## Operating Modes

### 1. In-Place Game Optimizer (Symlinks)
Offloads large game archives (`.ucas`, `.pak`, media) to secondary drives while maintaining transparent links in the original game directory.
- **Volume GUID Anchoring:** Links target `\\?\Volume{GUID}\...` — immune to drive letter changes.
- **UE5 IoStore Guard:** Retains critical bootstrap packages (`pakchunk0`, `global.*`) on the primary drive; keeps companions (`.utoc`, `.sig`) co-located with `.ucas`.
- **LPT Balancing:** Balances file distribution across drives based on container size and available space.
- **Safety & Rollback:** Atomic `.striping_manifest.json`, local `.backup` safety net, and target consolidation.

### 2. Virtual Storage Pool (VFS Z:)
Aggregates folders across multiple physical disks into a unified virtual drive `Z:` via WinFsp.
- **Policies:** RAID-0 (Striped), JBOD (Capacity Merge), RAID-1 (Mirror).
- **Asymmetric Weighted Striping (RAID-0):** Auto-calibrates drive speed ratios (e.g., 2:1 schedule `[D1, D1, D2]`) so faster drives never stall on slower spindles.
- **$O(1)$ CLOCK RAM Cache:** Low-overhead second-chance eviction via an atomic `referenced` bit, eliminating $O(N)$ traversal latency.
- **Asymmetric Prefetch:** Triggers at 50% chunk read, looks ahead up to 4 blocks, and prioritizes higher-latency drives.
- **Single-Flight:** Deduplicates concurrent chunk reads during asset streaming spikes (anti-stampede).
- **Zstd ATG & LZ4:** DirectStorage-grade compression with a 256 KB window log (`WindowLog = 18`) to prevent CPU L3 cache thrashing.

---

## Diagnostics & System Features

- **Time-Based Benchmark:** Measures sustained throughput over a fixed time window using unbuffered 4KB-aligned I/O (`FILE_FLAG_NO_BUFFERING`) with per-drive speed breakdowns.
- **Bus Detection:** Queries real hardware bus interfaces (NVMe, SATA, USB) via Win32 `IOCTL_STORAGE_QUERY_PROPERTY`.
- **S.M.A.R.T. Monitor:** Polls physical drive health via `IOCTL_STORAGE_PREDICT_FAILURE`.
- **Zero-Alloc SEH:** Native emergency crash logger using pre-allocated paths and direct kernel handles to survive stack overflows and heap corruption.
- **Windows Service Mode:** CLI flags (`--install-service`, `--service`) to auto-mount drive `Z:` at boot prior to user logon.
- **Process Priority:** Disables Windows 11 EcoQoS (Power Throttling) and elevates process priority to Above Normal.

---

## Requirements & Build

- Windows 10 / 11 (x64)
- [WinFsp Runtime](https://winfsp.dev/) installed
- Rust 1.80+ (Edition 2024)
- Slint UI 1.18
