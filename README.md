# Chisel

Find and extract known data fragments from arbitrary binary files — using only checksums, sizes, and structural
metadata.

## What it does

Given a manifest of expected files — names, sizes, CRC32 checksums — and one or more raw binary blobs, Chisel locates
and extracts every matching fragment. It handles data that has been interleaved, byte-swapped for different CPU
architectures, compressed with various algorithms, or buried inside nested archives. Every extracted fragment is
verified by CRC32 (and SHA1 when available) before being written to disk.

Built for ROM extraction, where a single binary image may contain dozens of interleaved, byte-swapped ROM files
described by [MAME](https://www.mamedev.org/) XML DAT files. MAME catalogs thousands of arcade games, each with a manifest of the individual
ROM chips on the original PCB — Chisel uses these manifests to reverse the hardware's physical memory layout and recover
each ROM from a package.

Key capabilities:

- **Structural matching** — detects concatenated, fully interleaved, and banked interleaved layouts from region/offset
  metadata
- **Heuristic search** — exact CRC lookup, sliding-window CRC scan, deinterleave with split
- **Byte-swap detection** — automatically tries 16-bit and 32-bit byte-swap variants
- **CRC32 + SHA1 verification** — every extracted fragment is verified before writing
- **Archive expansion** — transparently handles gzip, ZIP, KPKA/PAK, LZMA/XZ, and Zstandard compressed data within
  candidates
- **GEX task generation** — emit Python task files for
  the [Game Extraction Toolbox](https://github.com/shawngmc/game-extraction-toolbox/tree/main)

## How it works

Chisel runs a multi-stage extraction pipeline: structural analysis → priority-queue heuristic search → archive
expansion → second pass. Every extracted fragment is verified by CRC32, and by SHA1 when the manifest provides one.

### Rolling CRC32 sliding window

The core challenge is locating a known fragment inside a much larger binary when you only know its size and CRC32.
Naively recomputing CRC32 at every offset is O(n × window) — scanning a 1 GB file for a 512 KB fragment would require
~500 billion hash operations.

Chisel exploits the GF(2)-linear structure of CRC32. It precomputes a 32×32 binary matrix for the polynomial shift
x^(8n) using repeated squaring, then applies it per window position via a single matrix-vector multiply. Two
implementations are provided: prefix-combine (precompute prefix CRCs, then XOR + matrix multiply per window) and true
rolling (maintain running state with a 256-entry removal table). Both achieve O(1) per window shift, making the full
scan O(n) regardless of fragment size.

See `src/crc32_window.rs`.

### Adaptive priority-queue scheduler

With multiple heuristic strategies, fragment sizes, and candidate files, the search space is combinatorially large.
Chisel uses a min-heap ordered by cost/value ratio to always attempt the cheapest, highest-payoff work first. Each
heuristic estimates its own computational cost (O(1) for exact CRC lookup, O(n) for sliding window). Value is derived
from fragment-count × coverage potential, with a 2× bonus for tasks that would fully account for a candidate file.

When a match is found, the candidate's version counter increments, instantly invalidating all stale heap entries — an
O(1) skip on pop that avoids expensive heap rebuilds. Failed heuristic/candidate/size triples are recorded and never
retried, since the pending set can only shrink monotonically.

See `src/pipeline.rs`.

### Structural region decomposition

Before the heuristic pipeline runs, Chisel parses region and offset metadata to detect how data is physically laid out:
concatenated, fully interleaved (e.g., 4-way byte-interleaved CPU ROMs), or banked interleaved (multiple interleaved
banks concatenated together). It automatically selects the best candidate for each region by scoring CRC hit counts,
extracts verified pieces directly, and marks unverified pieces for pipeline retry.

See `src/structural.rs`.

### Embedded LZMA detection

Scanning raw binaries for embedded LZMA streams at every offset would produce millions of false-positive decompression
attempts. Chisel applies six filters before attempting decompression: valid LZMA1 props byte (lp=0 restriction),
power-of-2 dictionary size, range coder first byte must be 0x00, uncompressed size must match a pending fragment,
4-byte-aligned offsets only, and early exit when all fragments are already matched. Together these reduce decompression
attempts by roughly two orders of magnitude.

See `src/utils.rs`.

### Adaptive CRC lookup

CRC lookup tables auto-select between linear scan (≤8 entries), sorted binary search (9–512 entries), and
identity-hashed `HashSet` (>512 entries) based on bucket size. Coverage tracking uses sorted interval merging with O(1)
fully-covered checks.

See `src/types.rs`.

## Installation

Requires Rust 2024 edition (rustc 1.85+).

```sh
cargo install --path .
```

Or build without installing:

```sh
cargo build --release
# Binary at target/release/chisel
```

## Usage

Chisel has two modes: **DAT matching** and **direct extraction**.

### DAT matching

Match ROMs described in a DAT file against candidate binaries:

```sh
chisel --dat game.dat --output-dir out/ candidates/*
```

Filter to a single game:

```sh
chisel --dat mame.dat --game foo --output-dir out/ dump.bin
```

Organize output into per-game subdirectories:

```sh
chisel --dat mame.dat --game-subdirs --output-dir out/ dumps/*
```

Generate a GEX task file instead of extracting ROMs:

```sh
chisel --dat mame.dat --game foo --gex task.py --output-dir out/ dump.bin
```

### Direct extraction

Apply an extraction spec directly without a DAT file:

```sh
chisel --spec "skip=1,step_by=2,size=8192" --output-dir out/ input.bin
```

Spec keys: `skip`, `step_by`, `take`, `size`, `rotate_left`, `byte_swap` (`none`/`swap2`/`swap4`).

### Flags

| Flag                  | Description                                              |
|-----------------------|----------------------------------------------------------|
| `--dat <path>`        | Path to a MAME XML or No-Intro/Logiqx XML DAT file       |
| `--spec <spec>`       | Direct extraction spec (mutually exclusive with `--dat`) |
| `--game <name>`       | Filter to a specific game                                |
| `--output-dir <path>` | Where to write extracted ROMs                            |
| `--gex <path>`        | Emit a GEX Python task file instead of ROMs              |
| `--game-subdirs`      | Write ROMs into per-game subdirectories                  |
| `--emit-unmatched`    | Write unmatched candidate chunks to output               |
| `--no-structural`     | Disable structural (region-layout) matching pass         |
| `--no-byte-swap`      | Disable byte-swap heuristic variants                     |
| `--no-expand`         | Disable KPKA/PAK and LZMA/XZ archive expansion           |
| `-v, --verbose`       | Verbose logging                                          |

## Supported input formats

**DAT files:**

- MAME XML (from `mame -listxml`)
- No-Intro / Logiqx XML

**Candidate files:**

- Raw binary dumps
- gzip, ZIP, KPKA/PAK, LZMA/XZ, Zstandard (expanded automatically)

## Output

- Extracted ROM files, optionally organized into game subdirectories
- GEX Python task files (`--gex`) for use with the Game Extraction Toolbox

## License

[MIT](https://opensource.org/license/mit)
