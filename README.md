# SymStripe

Dual-paradigm multi-drive storage accelerator and virtual filesystem for Windows. Multiplies sequential read throughput by distributing I/O across multiple physical drives without RAID formatting.

---

## Architecture & Paradigms

### 1. In-Place Game Optimizer
Relocates heavy asset archives (e.g., Unreal Engine `.ucas`, `.pak`, `.vfs`) from a primary game directory across secondary physical drives.
- **LPT Bin-Packing:** Distributes files using a Longest Processing Time size-balancing algorithm across $N$ target drives.
- **UE5 Safe Mode:** Uses NTFS Directory Junctions (`mklink /J`) to bypass symlink admin privilege requirements.
- **`.backup` Safety:** Retains local backup copies on the primary drive and maintains `.striping_manifest.json` for integrity verification and auto-repair.
- **Target Consolidation:** Moves data from a secondary target drive back to the primary volume and cleans up empty trees.

### 2. Virtual Storage Pool (VFS Z:)
A user-mode virtual filesystem powered by **WinFsp** that aggregates multiple folders across physical disks into a single mountable drive (`Z:`).
- **Transparent On-The-Fly Striping:** Intercepts writes to `Z:` and dynamically chunks/stripes data across member drives (RAID-0 block extent striping).
- **JBOD Capacity Mode:** Merges free space across disparate drives transparently.
- **Master-Drive Passthrough:** Uses the master drive for directory structure while streaming heavy payloads directly to array members.

---

## Additional Features

- **Direct I/O Hardware Benchmark:** Tests raw physical drive throughput by bypassing Windows RAM cache (`FILE_FLAG_NO_BUFFERING` with 4KB-aligned memory buffers).
- **S.M.A.R.T. Health Monitor:** Native Win32 device querying (`DeviceIoControl`) for physical disk health status.
- **Portable Profiles:** Stores multi-game configurations in `config.json` next to the executable. No registry or `%APPDATA%` writes.

---

## Requirements & Build

- Windows 10 / 11
- [WinFsp Runtime](https://github.com/winfsp/winfsp) installed
- Rust 1.80+ (Edition 2024)
- Slint UI framework (compiled via `build.rs`)
