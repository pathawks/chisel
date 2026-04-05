use crate::types::{CandidateSource, Coverage};
use anyhow::Context;
use crc32fast;
use flate2::read::GzDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::{fs, io::Read, path::Path};

const XZ_MAGIC: [u8; 6] = [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00];

/// Lookup table for valid LZMA1 props bytes with lp=0.
/// props = lc + lp*9 + pb*45; with lp=0: props = lc + pb*45.
/// lc: 0-8, pb: 0-4 → 45 valid values out of 256.
const LZMA1_VALID_PROPS: [bool; 256] = {
    let mut table = [false; 256];
    let mut pb: u8 = 0;
    while pb <= 4 {
        let mut lc: u8 = 0;
        while lc <= 8 {
            table[(lc + pb * 45) as usize] = true;
            lc += 1;
        }
        pb += 1;
    }
    table
};

/// Check if a dict_size is plausible for real LZMA1 streams.
/// Real dict sizes are powers of 2 or 2^n + 2^(n-1) (i.e. 3 * 2^(n-1)).
fn is_valid_lzma_dict_size(d: u32) -> bool {
    d.is_power_of_two() || (d.is_multiple_of(3) && d / 3 > 0 && (d / 3).is_power_of_two())
}

use crate::{Candidate, RomInfo};

/// Parse a space-separated hex string (e.g. "4E 45 53 1A") into bytes.
pub fn parse_hex_header(s: &str) -> anyhow::Result<Vec<u8>> {
    s.split_whitespace()
        .map(|tok| u8::from_str_radix(tok, 16).with_context(|| format!("Bad hex byte: {tok:?}")))
        .collect()
}

// CRC32 (ISO 3309 / ITU-T V.42) lookup table, same polynomial as zlib/crc32fast.
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

/// Advance a CRC32 state through `n` zero bytes.
/// Equivalent to feeding n 0x00 bytes through the standard CRC32 algorithm.
pub fn crc32_process_zeros(mut state: u32, n: usize) -> u32 {
    // state is in the inverted form used internally by CRC32.
    // CRC32 processes each byte as: state = table[(state ^ byte) & 0xFF] ^ (state >> 8)
    // For zero bytes, byte=0, so: state = table[state & 0xFF] ^ (state >> 8)
    for _ in 0..n {
        state = CRC32_TABLE[(state & 0xFF) as usize] ^ (state >> 8);
    }
    state
}

/// Derive the CRC32 of content bytes given the full-file CRC32 and header bytes.
///
/// Uses the CRC32 combine identity: `crc(A||B) = process_zeros(crc_A, len_B) ^ crc_B`
/// where all values are finalized CRC32s and `process_zeros` applies the GF(2)-linear
/// zero-byte CRC step. Rearranging: `crc_B = crc(A||B) ^ process_zeros(crc_A, len_B)`.
pub fn derive_content_crc(full_crc: u32, header: &[u8], content_len: usize) -> u32 {
    let header_crc = crc32fast::hash(header);
    full_crc ^ crc32_process_zeros(header_crc, content_len)
}

/// Parse a MAME XML DAT or No-Intro/Logiqx XML DAT and extract `<rom>` entries.
///
/// Supports both MAME format (`<machine name="...">` containers) and No-Intro
/// Logiqx format (`<game name="...">` containers). Format is detected automatically.
pub fn load_rom_list(dat_path: &Path, maybe_game: Option<&str>) -> anyhow::Result<Vec<RomInfo>> {
    let mut reader = Reader::from_file(dat_path)
        .with_context(|| format!("Failed to open XML DAT at {}", dat_path.display()))?;
    let mut buf = Vec::new();
    let mut in_target = false;
    let mut current_game = String::new();
    let mut roms = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) if matches!(e.name().as_ref(), b"machine" | b"game") => {
                let mut game_name = String::new();
                for attr in e.attributes().with_checks(false) {
                    let attr = attr?;
                    if attr.key.as_ref() == b"name" {
                        game_name = attr.unescape_value()?.to_string();
                        break;
                    }
                }
                in_target = if let Some(game) = maybe_game {
                    game_name == game
                } else {
                    true
                };
                if in_target {
                    current_game = game_name;
                }
            }
            Event::Empty(e) if in_target && e.name().as_ref() == b"rom" => {
                let mut info = RomInfo {
                    name: String::new(),
                    game: current_game.clone(),
                    size: 0,
                    crc32: 0,
                    sha1: None,
                    matched: false,
                    unverified: false,
                    region: None,
                    offset: None,
                    header: None,
                };
                let mut raw_header: Option<String> = None;
                for attr in e.attributes().with_checks(false) {
                    let attr = attr?;
                    match attr.key.as_ref() {
                        b"name" => {
                            info.name = attr.unescape_value()?.to_string();
                        }
                        b"size" => {
                            info.size = attr
                                .unescape_value()?
                                .parse()
                                .context("Parsing <rom size>")?;
                        }
                        b"crc" => {
                            let hex = attr.unescape_value()?;
                            info.crc32 =
                                u32::from_str_radix(&hex, 16).context("Parsing <rom crc>")?;
                        }
                        b"sha1" => {
                            info.sha1 = Some(attr.unescape_value()?.to_string());
                        }
                        b"region" => {
                            info.region = Some(attr.unescape_value()?.to_string());
                        }
                        b"offset" => {
                            let hex = attr.unescape_value()?;
                            info.offset = Some(
                                u64::from_str_radix(hex.trim(), 16)
                                    .context("Parsing <rom offset>")?,
                            );
                        }
                        b"header" => {
                            raw_header = Some(attr.unescape_value()?.to_string());
                        }
                        _ => {}
                    }
                }
                // If a header is present, adjust size/CRC for content-only matching
                if let Some(hex_str) = raw_header {
                    let hdr = parse_hex_header(&hex_str).context("Parsing <rom header>")?;
                    let full_crc = info.crc32;
                    let content_len = info
                        .size
                        .checked_sub(hdr.len())
                        .context("Header longer than ROM size")?;
                    info.size = content_len;
                    info.crc32 = derive_content_crc(full_crc, &hdr, content_len);
                    info.header = Some(hdr);
                }
                roms.push(info);
            }
            Event::End(e) if matches!(e.name().as_ref(), b"machine" | b"game") => {
                if maybe_game.is_some() && in_target {
                    break; // finished the one we wanted
                }
                in_target = false; // otherwise keep going
            }
            Event::Eof => {
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    if roms.is_empty() {
        match maybe_game {
            Some(game) => {
                anyhow::bail!("No game named {:?} found in {}", game, dat_path.display());
            }
            None => {
                anyhow::bail!("No <rom> entries found in {}", dat_path.display());
            }
        }
    }
    Ok(roms)
}

/// Detect compressed archives by magic bytes and expand into `Candidate`s.
///
/// - `.gz` / gzip magic (`1f 8b`): decompress into one candidate.
/// - `.zip` magic (`PK\x03\x04`): one candidate per file member. Encrypted members
///   are decrypted using the provided `passwords`; members that cannot be decrypted
///   are skipped with a warning (when `verbose` is set).
/// - Everything else: one plain candidate.
pub fn load_candidates_from_paths<I>(
    paths: I,
    passwords: &[String],
    verbose: bool,
) -> anyhow::Result<Vec<Candidate>>
where
    I: IntoIterator,
    I::Item: AsRef<Path>,
{
    let mut cands = Vec::new();

    for p in paths {
        let path = p.as_ref().to_path_buf();
        if path.is_dir() {
            if verbose {
                eprintln!("skipping directory {}", path.display());
            }
            continue;
        }

        let mut file =
            fs::File::open(&path).with_context(|| format!("Opening file {}", path.display()))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .with_context(|| format!("Reading file {}", path.display()))?;

        if data.starts_with(&[0x1f, 0x8b]) {
            // gzip
            if verbose {
                eprintln!("decompressing gzip: {}", path.display());
            }
            let mut decoder = GzDecoder::new(&data[..]);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .with_context(|| format!("Decompressing gzip {}", path.display()))?;
            cands.push(Candidate {
                path: path.clone(),
                data: decompressed,
                source: CandidateSource::Gzip { archive: path },
                coverage: Coverage::default(),
            });
        } else if data.starts_with(&[b'P', b'K', 0x03, 0x04]) {
            // zip
            if verbose {
                eprintln!("expanding zip: {}", path.display());
            }
            let cursor = std::io::Cursor::new(&data);
            let mut archive = zip::ZipArchive::new(cursor)
                .with_context(|| format!("Opening zip {}", path.display()))?;

            // Collect metadata first to avoid borrow conflicts when retrying
            // passwords on encrypted entries.
            let entry_meta: Vec<_> = (0..archive.len())
                .map(|i| {
                    let entry = archive.by_index_raw(i).with_context(|| {
                        format!("Reading zip entry metadata {} in {}", i, path.display())
                    })?;
                    if entry.is_dir() {
                        return Ok(None);
                    }
                    Ok(Some((i, entry.name().to_string(), entry.encrypted())))
                })
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();

            for (i, member_name, encrypted) in entry_meta {
                let (member_data, password) = if encrypted {
                    let mut decrypted = None;
                    for pw in passwords {
                        match archive.by_index_decrypt(i, pw.as_bytes()) {
                            Err(zip::result::ZipError::InvalidPassword) => continue,
                            Err(e) => {
                                return Err(anyhow::Error::from(e).context(format!(
                                    "Reading encrypted zip entry '{}' in {}",
                                    member_name,
                                    path.display()
                                )));
                            }
                            Ok(mut entry) => {
                                let mut buf = Vec::new();
                                match entry.read_to_end(&mut buf) {
                                    Ok(_) => {
                                        decrypted = Some((buf, pw.clone()));
                                        break;
                                    }
                                    // AES HMAC validation failure at end-of-stream
                                    Err(e)
                                        if e.kind() == std::io::ErrorKind::InvalidData
                                            || e.kind() == std::io::ErrorKind::InvalidInput =>
                                    {
                                        continue;
                                    }
                                    Err(e) => {
                                        return Err(anyhow::Error::from(e).context(format!(
                                            "Reading zip member '{}' in {}",
                                            member_name,
                                            path.display()
                                        )));
                                    }
                                }
                            }
                        }
                    }
                    let Some((data, pw)) = decrypted else {
                        if verbose {
                            eprintln!(
                                "skipping encrypted zip member '{}' in {}",
                                member_name,
                                path.display()
                            );
                        }
                        continue;
                    };
                    (data, Some(pw))
                } else {
                    let mut entry = archive.by_index(i).with_context(|| {
                        format!("Reading zip entry {} in {}", i, path.display())
                    })?;
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).with_context(|| {
                        format!("Reading zip member '{}' in {}", member_name, path.display())
                    })?;
                    (buf, None)
                };
                if verbose {
                    eprintln!(
                        "  zip member: {} ({} bytes{})",
                        member_name,
                        member_data.len(),
                        if password.is_some() {
                            ", decrypted"
                        } else {
                            ""
                        }
                    );
                }
                let logical_path = path.join(&member_name);
                cands.push(Candidate {
                    path: logical_path,
                    data: member_data,
                    source: CandidateSource::Zip {
                        archive: path.clone(),
                        member: member_name,
                        password,
                    },
                    coverage: Coverage::default(),
                });
            }
        } else {
            // plain binary
            cands.push(Candidate {
                path,
                data,
                source: CandidateSource::Plain,
                coverage: Coverage::default(),
            });
        }
    }

    Ok(cands)
}

/// Print unmatched ROMs after processing
pub fn report_results(roms: &[RomInfo]) {
    let matches = roms.iter().filter(|r| r.matched).count();
    println!("\nMatched {} ROMs", matches);
    for r in roms.iter().filter(|r| !r.matched && r.unverified) {
        eprintln!(
            "! Unverified: {} (size={}, crc={:08x})",
            r.name, r.size, r.crc32
        );
    }
    for r in roms.iter().filter(|r| !r.matched && !r.unverified) {
        eprintln!(
            "✗ Unmatched: {} (size={}, crc={:08x})",
            r.name, r.size, r.crc32
        );
    }
}

/// Scan existing candidates for KPKA/PAK archive magic and append each entry as an
/// additional candidate. Entries with `flags & 1` are Zstandard-compressed; others
/// are stored uncompressed.
pub fn expand_kpka_entries(cands: &mut Vec<Candidate>, verbose: bool) {
    const KPKA_MAGIC: &[u8; 4] = b"KPKA";
    const KPKA_ENTRY_SIZE: usize = 48;
    const KPKA_HEADER_SIZE: usize = 12;

    let original_len = cands.len();
    for i in 0..original_len {
        let parent = cands[i].path.clone();
        let data = cands[i].data.clone();

        if data.len() < KPKA_HEADER_SIZE || !data.starts_with(KPKA_MAGIC) {
            continue;
        }

        let file_count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let table_end = KPKA_HEADER_SIZE + file_count * KPKA_ENTRY_SIZE;
        if table_end > data.len() {
            continue;
        }

        for index in 0..file_count {
            let base = KPKA_HEADER_SIZE + index * KPKA_ENTRY_SIZE;
            let offset =
                u64::from_le_bytes(data[base + 16..base + 24].try_into().unwrap()) as usize;
            let compressed_size =
                u64::from_le_bytes(data[base + 24..base + 32].try_into().unwrap()) as usize;
            let uncompressed_size =
                u64::from_le_bytes(data[base + 32..base + 40].try_into().unwrap()) as usize;
            let flags = u64::from_le_bytes(data[base + 40..base + 48].try_into().unwrap());

            if offset
                .checked_add(compressed_size)
                .is_none_or(|end| end > data.len())
            {
                continue;
            }

            let is_zstd = (flags & 1) != 0 && compressed_size != uncompressed_size;
            let entry_bytes = &data[offset..offset + compressed_size];

            let candidate_data: Vec<u8> = if is_zstd {
                match zstd::decode_all(entry_bytes) {
                    Ok(dec) if !dec.is_empty() => dec,
                    _ => continue,
                }
            } else {
                entry_bytes.to_vec()
            };

            if candidate_data.is_empty() {
                continue;
            }

            if verbose {
                eprintln!(
                    "KPKA entry {} in {} → {} bytes ({})",
                    index,
                    parent.display(),
                    candidate_data.len(),
                    if is_zstd { "zstd" } else { "stored" }
                );
            }

            cands.push(Candidate {
                path: parent.with_extension(format!("kpka_{index:05}")),
                data: candidate_data,
                source: CandidateSource::Kpka {
                    archive: parent.clone(),
                    index,
                },
                coverage: Coverage::default(),
            });
        }
    }
}

/// Scan existing candidates for embedded LZMA/XZ blocks and append successfully
/// decompressed data as additional candidates.
///
/// XZ blocks are detected by the 6-byte magic `FD 37 7A 58 5A 00`.
/// LZMA1 blocks are detected by validated header fields (props, dict_size,
/// uncompressed size, range coder byte) at 4-byte-aligned offsets.
/// Failures are silently skipped (expected for false-positive magic hits).
pub fn expand_lzma_blocks(cands: &mut Vec<Candidate>, roms: &[RomInfo], verbose: bool) {
    use std::io::{BufReader, Cursor};

    // Optimization 5: skip entirely if all ROMs already matched.
    if roms.iter().all(|r| r.matched) {
        if verbose {
            eprintln!("  Skipping LZMA scan: all ROMs already matched");
        }
        return;
    }

    // Collect pending (unmatched) ROM sizes for uncompressed-size validation.
    let pending_sizes: Vec<u64> = roms
        .iter()
        .filter(|r| !r.matched)
        .map(|r| r.size as u64)
        .collect();

    let original_len = cands.len();
    for i in 0..original_len {
        let parent = cands[i].path.clone();
        // Clone data to avoid borrowing `cands` while we push new entries.
        let data = cands[i].data.clone();

        // ---- XZ blocks ----
        let mut offset = 0;
        let mut xz_attempts = 0u64;
        while offset + XZ_MAGIC.len() <= data.len() {
            if data[offset..].starts_with(&XZ_MAGIC) {
                xz_attempts += 1;
                let mut input = BufReader::new(Cursor::new(&data[offset..]));
                let mut decompressed = Vec::new();
                if lzma_rs::xz_decompress(&mut input, &mut decompressed).is_ok()
                    && !decompressed.is_empty()
                {
                    if verbose {
                        eprintln!(
                            "XZ block at {:#010x} in {} → {} bytes",
                            offset,
                            parent.display(),
                            decompressed.len()
                        );
                    }
                    cands.push(Candidate {
                        path: parent.with_extension(format!("lzma_{offset:08x}")),
                        data: decompressed,
                        source: CandidateSource::Lzma {
                            parent: parent.clone(),
                            offset,
                        },
                        coverage: Coverage::default(),
                    });
                }
            }
            offset += 1;
        }
        if verbose && xz_attempts > 0 {
            eprintln!(
                "  XZ: {} decompression attempts in {}",
                xz_attempts,
                parent.display()
            );
        }

        // ---- LZMA1 blocks ----
        // Format: [props(1)] [dict_size(4 LE)] [uncompressed_size(8 LE)] [data...]
        // 13-byte header + at least 1 byte of compressed data (range coder).
        // Optimization 6: scan at 4-byte-aligned offsets only.
        offset = 0;
        let mut lzma1_attempts = 0u64;
        while offset + 14 <= data.len() {
            'check: {
                // Optimization 1: restrict props to common values (lp=0).
                let props = data[offset];
                if !LZMA1_VALID_PROPS[props as usize] {
                    break 'check;
                }

                // Optimization 2: dict_size must be power-of-two or 1.5× variant.
                let dict_size = u32::from_le_bytes([
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                    data[offset + 4],
                ]);
                if !is_valid_lzma_dict_size(dict_size) {
                    break 'check;
                }

                // Optimization 3: first byte of range-coded data must be 0x00.
                if data[offset + 13] != 0x00 {
                    break 'check;
                }

                // Optimization 4: validate uncompressed size against pending ROM sizes.
                let uncomp_size =
                    u64::from_le_bytes(data[offset + 5..offset + 13].try_into().unwrap());
                if uncomp_size != u64::MAX {
                    // Known size: must be non-zero, ≤16 MB, and match a pending ROM.
                    if uncomp_size == 0
                        || uncomp_size > 16 * 1024 * 1024
                        || !pending_sizes.contains(&uncomp_size)
                    {
                        break 'check;
                    }
                }

                // All filters passed — attempt decompression.
                lzma1_attempts += 1;
                let mut input = BufReader::new(Cursor::new(&data[offset..]));
                let mut decompressed = Vec::new();
                if lzma_rs::lzma_decompress(&mut input, &mut decompressed).is_ok()
                    && !decompressed.is_empty()
                {
                    if verbose {
                        eprintln!(
                            "LZMA1 block at {:#010x} in {} → {} bytes",
                            offset,
                            parent.display(),
                            decompressed.len()
                        );
                    }
                    cands.push(Candidate {
                        path: parent.with_extension(format!("lzma_{offset:08x}")),
                        data: decompressed,
                        source: CandidateSource::Lzma {
                            parent: parent.clone(),
                            offset,
                        },
                        coverage: Coverage::default(),
                    });
                }
            }
            offset += 4;
        }
        if verbose {
            eprintln!(
                "  LZMA1: {} decompression attempts across {} bytes in {}",
                lzma1_attempts,
                data.len(),
                parent.display()
            );
        }
    }
}

/// Decode a LZ77 compressed block.
///
/// Header: byte 0 = `0x10`, bytes 1-3 = uncompressed size (24-bit LE).
/// Returns `(decompressed_data, total_bytes_consumed)` including the 4-byte header,
/// or `None` on invalid data.
pub fn decode_lz77(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    if data.len() < 4 || data[0] != 0x10 {
        return None;
    }
    let usize_val = data[1] as usize | (data[2] as usize) << 8 | (data[3] as usize) << 16;
    if usize_val == 0 || usize_val > 0x100000 {
        return None;
    }
    let mut out = Vec::with_capacity(usize_val);
    let mut src = 4usize;
    while out.len() < usize_val {
        if src >= data.len() {
            return None;
        }
        let flags = data[src];
        src += 1;
        for bit in 0..8 {
            if out.len() >= usize_val {
                break;
            }
            if flags & (0x80 >> bit) != 0 {
                // backref
                if src + 1 >= data.len() {
                    return None;
                }
                let b0 = data[src];
                let b1 = data[src + 1];
                src += 2;
                let length = (b0 >> 4) as usize + 3;
                let disp = (((b0 & 0x0F) as usize) << 8 | b1 as usize) + 1;
                if disp > out.len() {
                    return None;
                }
                for _ in 0..length {
                    if out.len() >= usize_val {
                        break;
                    }
                    let byte = out[out.len() - disp];
                    out.push(byte);
                }
            } else {
                // literal
                if src >= data.len() {
                    return None;
                }
                out.push(data[src]);
                src += 1;
            }
        }
    }
    Some((out, src))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_hex_header_basic() {
        let hdr = parse_hex_header("DE AD BE EF").unwrap();
        assert_eq!(hdr, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn parse_hex_header_lowercase() {
        let hdr = parse_hex_header("ab cd ef").unwrap();
        assert_eq!(hdr, vec![0xAB, 0xCD, 0xEF]);
    }

    #[test]
    fn parse_hex_header_single_byte() {
        let hdr = parse_hex_header("FF").unwrap();
        assert_eq!(hdr, vec![0xFF]);
    }

    #[test]
    fn parse_hex_header_bad_hex() {
        assert!(parse_hex_header("GG").is_err());
    }

    #[test]
    fn derive_content_crc_matches_direct() {
        // Build header + content, compute full CRC, then derive content CRC
        // and verify it matches crc32(content).
        let header = b"\xDE\xAD\xBE\xEF\x02\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let content: Vec<u8> = (0..256u16).map(|i| (i & 0xFF) as u8).collect();

        let mut full = Vec::new();
        full.extend_from_slice(header);
        full.extend_from_slice(&content);

        let full_crc = crc32fast::hash(&full);
        let derived = derive_content_crc(full_crc, header, content.len());
        let actual = crc32fast::hash(&content);
        assert_eq!(derived, actual, "derived={derived:08x} actual={actual:08x}");
    }

    #[test]
    fn derive_content_crc_empty_header() {
        // With an empty header, content CRC should equal full CRC
        let content = b"hello world";
        let full_crc = crc32fast::hash(content);
        let derived = derive_content_crc(full_crc, b"", content.len());
        assert_eq!(derived, full_crc);
    }

    #[test]
    fn derive_content_crc_single_byte_content() {
        let header = b"\x00";
        let content = b"\x42";
        let mut full = Vec::new();
        full.extend_from_slice(header);
        full.extend_from_slice(content);
        let full_crc = crc32fast::hash(&full);
        let derived = derive_content_crc(full_crc, header, content.len());
        let actual = crc32fast::hash(content);
        assert_eq!(derived, actual);
    }

    #[test]
    fn lz77_invalid_inputs() {
        assert!(decode_lz77(&[]).is_none());
        assert!(decode_lz77(&[0x10]).is_none());
        assert!(decode_lz77(&[0x20, 0x01, 0x00, 0x00]).is_none()); // wrong tag
        assert!(decode_lz77(&[0x10, 0x00, 0x00, 0x00]).is_none()); // zero size
    }

    #[test]
    fn lz77_literals_only() {
        // Header: tag=0x10, size=3
        // Flags: 0x00 = all literals for 8 bits (we only need 3)
        let data = [0x10, 0x03, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0xCC];
        let (out, consumed) = decode_lz77(&data).unwrap();
        assert_eq!(out, vec![0xAA, 0xBB, 0xCC]);
        assert_eq!(consumed, 8); // 4 header + 1 flags + 3 literals
    }

    #[test]
    fn lz77_backref() {
        // Encode "AAABBB" using LZ77: 3 literals 'A','A','A', then backref
        // to copy 3 bytes from displacement 1 (repeats 'A').
        // Wait — that gives AAAAAA. Let's do: 3 literals A,B,C then backref
        // disp=3 len=3 to copy A,B,C again => "ABCABC".
        //
        // Header: 0x10, size=6 (LE: 0x06, 0x00, 0x00)
        // flags byte: bits for 4 entries: lit, lit, lit, backref = 0b0001_0000 = 0x10
        //   Note: in this LZ77 format bit=1 is backref, bit=0 is literal.
        //   So 3 literals + 1 backref = 0b000_1_0000 = bits 7..4 = 0,0,0,1
        //   = 0x10
        // Literal bytes: A(0x41), B(0x42), C(0x43)
        // Backref: length-3 in top nibble, disp-1 in 12 bits
        //   length=3 => (3-3)=0 in top nibble
        //   disp=3 => disp-1=2 in lower 12 bits
        //   => b0 = 0x00, b1 = 0x02
        let data = [
            0x10, 0x06, 0x00, 0x00, // header
            0x10, // flags: bits 7..0 = 0,0,0,1,0,0,0,0
            0x41, 0x42, 0x43, // 3 literals
            0x00, 0x02, // backref: len=3, disp=3
        ];
        let (out, consumed) = decode_lz77(&data).unwrap();
        assert_eq!(out, b"ABCABC");
        assert_eq!(consumed, data.len());
    }

    #[test]
    fn lz77_truncated_input() {
        // Valid header claiming 10 bytes, but input truncated after header
        let data = [0x10, 0x0A, 0x00, 0x00, 0x00];
        // flags=0x00 means 8 literals, but only 1 byte available => None
        assert!(decode_lz77(&data).is_none());
    }

    #[test]
    fn header_rom_round_trip() {
        // Simulate what happens: DAT has header+content CRC/SHA1,
        // we derive content CRC, match against raw content, then verify SHA1.
        use crate::test_support::{make_candidate, make_rom_with_header};
        use crate::{ExactCrc, test_support::run_heuristic};

        let header = b"\xDE\xAD\xBE\xEF";
        let content: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];

        let mut roms = vec![make_rom_with_header("test.bin", header, &content)];
        // Content CRC should match raw content
        assert_eq!(roms[0].crc32, crc32fast::hash(&content));
        assert_eq!(roms[0].size, content.len());

        let mut cands = vec![make_candidate("raw.bin", content.clone())];
        let records = run_heuristic(
            &ExactCrc::new(crate::types::ByteSwap::None, false),
            &mut roms,
            &mut cands,
        );

        assert_eq!(records.len(), 1);
        assert!(roms[0].matched);
        assert_eq!(records[0].header.as_deref(), Some(header.as_slice()));
    }

    /// Helper: create a zip archive in memory with one stored (uncompressed) file.
    fn make_zip(name: &str, content: &[u8]) -> Vec<u8> {
        let buf = Vec::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(buf));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file(name, opts).unwrap();
        writer.write_all(content).unwrap();
        writer.finish().unwrap().into_inner()
    }

    /// Helper: create an AES-256 encrypted zip archive in memory.
    fn make_encrypted_zip(name: &str, content: &[u8], password: &str) -> Vec<u8> {
        let buf = Vec::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(buf));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .with_aes_encryption(zip::AesMode::Aes256, password);
        writer.start_file(name, opts).unwrap();
        writer.write_all(content).unwrap();
        writer.finish().unwrap().into_inner()
    }

    /// Helper: write bytes to a temp file and return the path.
    fn write_temp(dir: &std::path::Path, name: &str, data: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn zip_plain_candidate() {
        let dir = std::env::temp_dir().join("chisel_test_zip_plain");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_bytes = make_zip("rom.bin", b"hello");
        let zip_path = write_temp(&dir, "test.zip", &zip_bytes);

        let cands = load_candidates_from_paths(&[&zip_path], &[], false).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].data, b"hello");
        assert!(matches!(
            &cands[0].source,
            CandidateSource::Zip { password: None, .. }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zip_encrypted_correct_password() {
        let dir = std::env::temp_dir().join("chisel_test_zip_enc_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_bytes = make_encrypted_zip("secret.bin", b"payload", "hunter2");
        let zip_path = write_temp(&dir, "enc.zip", &zip_bytes);

        let passwords = vec!["hunter2".to_string()];
        let cands = load_candidates_from_paths(&[&zip_path], &passwords, false).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].data, b"payload");
        assert!(matches!(
            &cands[0].source,
            CandidateSource::Zip {
                password: Some(_),
                ..
            }
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zip_encrypted_wrong_password_skipped() {
        let dir = std::env::temp_dir().join("chisel_test_zip_enc_wrong");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_bytes = make_encrypted_zip("secret.bin", b"payload", "correct");
        let zip_path = write_temp(&dir, "enc.zip", &zip_bytes);

        let passwords = vec!["wrong1".to_string(), "wrong2".to_string()];
        let cands = load_candidates_from_paths(&[&zip_path], &passwords, false).unwrap();
        assert_eq!(cands.len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zip_encrypted_no_passwords_skipped() {
        let dir = std::env::temp_dir().join("chisel_test_zip_enc_none");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_bytes = make_encrypted_zip("secret.bin", b"payload", "pw");
        let zip_path = write_temp(&dir, "enc.zip", &zip_bytes);

        let cands = load_candidates_from_paths(&[&zip_path], &[], false).unwrap();
        assert_eq!(cands.len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zip_encrypted_multiple_passwords_finds_correct() {
        let dir = std::env::temp_dir().join("chisel_test_zip_enc_multi");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_bytes = make_encrypted_zip("secret.bin", b"data", "third");
        let zip_path = write_temp(&dir, "enc.zip", &zip_bytes);

        let passwords: Vec<String> = ["first", "second", "third"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cands = load_candidates_from_paths(&[&zip_path], &passwords, false).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].data, b"data");

        std::fs::remove_dir_all(&dir).ok();
    }
}
