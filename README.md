# SymStripe

File-level parallel storage accelerator for Windows. Distributes large archives (`.ucas`, `.pak`, `.ba2`, `.prp`, `.safetensors`) across multiple physical drives via native NTFS symbolic links to multiply sequential read throughput without RAID formatting.

---

## Features

- **Multi-Drive Striping ($N$-Drives):** Distributes files across 2, 3, 4+ drives simultaneously using a greedy Longest Processing Time (LPT) size-balancing algorithm.
- **`.backup` Safe Mode:** Renames originals to `<filename>.backup` on the primary drive. Preserves local copies if secondary drives disconnect.
- **Sub-Second Reversion:** Restores files back to the primary drive via atomic renames and cleans up empty directory trees on target drives.
- **Auto-Threshold Detection:** Analyzes the folder tree and computes the minimum file size cutoff that captures ~80% of total payload volume.
- **Aggressive Media Offload:** Selectively routes streaming media containers (`.bik`, `.bk2`, `.mp4`, `.fsb`, `.pck`, `.wem`) to secondary drives to prioritize the primary drive for geometry and textures.
- **Integrity Verifier & Auto-Repair:** Scans `.striping_manifest.json` against actual disk state. Automatically repairs broken symlinks from local `.backup` files if secondary storage is modified.
- **Target Consolidation:** Moves data from a chosen secondary drive back to the primary drive and removes empty folders in one click.
- **Direct I/O Hardware Benchmark:** Tests physical drive read throughput by bypassing Windows RAM cache via `FILE_FLAG_NO_BUFFERING`.
- **S.M.A.R.T. Health Monitor:** Queries physical drive status (`Get-PhysicalDisk`) to warn about failing hardware before striping.
- **Multi-Profile Manager:** Stores settings per game/dataset in a portable `config.json` next to the executable. No registry or `%APPDATA%` writes.

---

## Requirements & Build

- Windows 10 / 11
- Administrator privileges (or Developer Mode enabled for symlink creation)
- Rust 1.80+
