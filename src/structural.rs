use crate::Candidate;
use crate::types::{ByteSwap, ExtractionSpec, MatchRecord, MatchedData, RomInfo};
use anyhow::Context;
use crc32fast::Hasher as CrcHasher;
use std::collections::HashMap;
use std::path::Path;

// ─── Data structures ────────────────────────────────────────────────────────

/// One interleaved bank: a stride-1 run of ROMs multiplexed byte-by-byte.
#[derive(Debug, Clone)]
pub struct InterleavedBank {
    /// Indices into the `roms` slice, one per lane, in lane order (offset order).
    pub rom_indices: Vec<usize>,
    /// Size of each individual ROM in this bank.
    pub rom_size: usize,
    /// Number of lanes (== rom_indices.len()).
    pub lane_count: usize,
    /// Byte offset of this bank within the combined physical file.
    pub physical_offset: usize,
}

/// How chips in a region are laid out in a combined dump.
#[derive(Debug, Clone)]
pub enum RegionLayout {
    /// Exactly one ROM; the candidate IS the ROM (no decomposition needed).
    SingleRom(usize),
    /// ROMs stored contiguously end-to-end; no stride-1 runs.
    Concatenated,
    /// All ROMs form a single stride-1 interleaved bank.
    FullyInterleaved(InterleavedBank),
    /// Multiple stride-1 banks concatenated together (e.g. grom pattern).
    BankedInterleaved(Vec<InterleavedBank>),
    /// Mixed sizes, missing offsets, or other structure we can't handle.
    Unsupported,
}

/// Structural description of one MAME region, ready for candidate matching.
#[derive(Debug, Clone)]
pub struct RegionSpec {
    /// `(game_name, region_name)` key.
    pub region_key: (String, String),
    pub layout: RegionLayout,
    /// Sum of all ROM sizes (== expected combined candidate size).
    pub combined_size: usize,
    /// ROM indices sorted by address-space offset.
    pub all_rom_indices: Vec<usize>,
}

// ─── Region analysis ─────────────────────────────────────────────────────────

/// Group ROMs by `(game, region)` and detect their layout.
pub fn analyze_regions(roms: &[RomInfo]) -> Vec<RegionSpec> {
    let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, rom) in roms.iter().enumerate() {
        if let Some(region) = &rom.region {
            groups
                .entry((rom.game.clone(), region.clone()))
                .or_default()
                .push(i);
        }
    }

    let mut result = Vec::new();
    for (key, mut indices) in groups {
        // Sort by offset; ROMs without an offset sort last.
        indices.sort_by_key(|&i| roms[i].offset.unwrap_or(u64::MAX));
        let combined_size: usize = indices.iter().map(|&i| roms[i].size).sum();

        let layout = if indices.len() == 1 {
            RegionLayout::SingleRom(indices[0])
        } else {
            detect_layout(roms, &indices)
        };

        result.push(RegionSpec {
            region_key: key,
            layout,
            combined_size,
            all_rom_indices: indices,
        });
    }
    result
}

/// Classify the ROM arrangement for a multi-ROM region.
fn detect_layout(roms: &[RomInfo], indices: &[usize]) -> RegionLayout {
    // All ROMs must have offsets.
    for &i in indices {
        if roms[i].offset.is_none() {
            return RegionLayout::Unsupported;
        }
    }

    let offsets: Vec<u64> = indices.iter().map(|&i| roms[i].offset.unwrap()).collect();
    let sizes: Vec<usize> = indices.iter().map(|&i| roms[i].size).collect();

    // Find stride-1 runs: consecutive positions where offset[j] == offset[j-1]+1
    // and all ROM sizes within the run are equal.
    let mut banks: Vec<(usize, usize)> = Vec::new(); // half-open [start, end)
    let mut i = 0;
    while i < indices.len() {
        let mut j = i + 1;
        while j < indices.len() && offsets[j] == offsets[j - 1] + 1 && sizes[j] == sizes[i] {
            j += 1;
        }
        if j > i + 1 {
            // Verify uniform size within this run (belt-and-suspenders).
            if sizes[i..j].iter().any(|&s| s != sizes[i]) {
                return RegionLayout::Unsupported;
            }
            banks.push((i, j));
            i = j;
        } else {
            i += 1;
        }
    }

    // No stride-1 runs → fully concatenated.
    if banks.is_empty() {
        return RegionLayout::Concatenated;
    }

    // Every ROM must belong to exactly one stride-1 bank.
    let total_in_banks: usize = banks.iter().map(|(s, e)| e - s).sum();
    if total_in_banks < indices.len() {
        return RegionLayout::Unsupported;
    }

    // All banks must have the same lane count and ROM size.
    let lane_count = banks[0].1 - banks[0].0;
    let rom_size = sizes[banks[0].0];
    for &(s, e) in &banks {
        if e - s != lane_count || sizes[s..e].iter().any(|&sz| sz != rom_size) {
            return RegionLayout::Unsupported;
        }
    }

    // Build InterleavedBank structs with physical offsets.
    let mut interleaved_banks: Vec<InterleavedBank> = Vec::new();
    let mut physical_offset = 0usize;
    for (bank_s, bank_e) in &banks {
        interleaved_banks.push(InterleavedBank {
            rom_indices: indices[*bank_s..*bank_e].to_vec(),
            rom_size,
            lane_count,
            physical_offset,
        });
        physical_offset += lane_count * rom_size;
    }

    if interleaved_banks.len() == 1 {
        RegionLayout::FullyInterleaved(interleaved_banks.remove(0))
    } else {
        RegionLayout::BankedInterleaved(interleaved_banks)
    }
}

// ─── Spec building ───────────────────────────────────────────────────────────

/// Return `(rom_idx, ExtractionSpec)` pairs for every ROM in the region.
/// Returns `None` for `Unsupported`.
fn build_specs(spec: &RegionSpec, roms: &[RomInfo]) -> Option<Vec<(usize, ExtractionSpec)>> {
    match &spec.layout {
        RegionLayout::SingleRom(rom_idx) => Some(vec![(
            *rom_idx,
            ExtractionSpec {
                skip: 0,
                step_by: 1,
                take: 1,
                size: roms[*rom_idx].size,
                rotate_left: 0,
                byte_swap: ByteSwap::None,
            },
        )]),

        RegionLayout::Concatenated => {
            let mut result = Vec::new();
            let mut offset = 0usize;
            for &rom_idx in &spec.all_rom_indices {
                let rom_size = roms[rom_idx].size;
                result.push((
                    rom_idx,
                    ExtractionSpec {
                        skip: offset,
                        step_by: 1,
                        take: 1,
                        size: rom_size,
                        rotate_left: 0,
                        byte_swap: ByteSwap::None,
                    },
                ));
                offset += rom_size;
            }
            Some(result)
        }

        RegionLayout::FullyInterleaved(bank) => Some(build_bank_specs(bank, 0)),

        RegionLayout::BankedInterleaved(banks) => {
            let mut result = Vec::new();
            for bank in banks {
                result.extend(build_bank_specs(bank, bank.physical_offset));
            }
            Some(result)
        }

        RegionLayout::Unsupported => None,
    }
}

/// Build ExtractionSpecs for all lanes in one interleaved bank.
/// `physical_offset` is the byte offset of this bank within the combined file.
fn build_bank_specs(
    bank: &InterleavedBank,
    physical_offset: usize,
) -> Vec<(usize, ExtractionSpec)> {
    bank.rom_indices
        .iter()
        .enumerate()
        .map(|(lane, &rom_idx)| {
            (
                rom_idx,
                ExtractionSpec {
                    skip: physical_offset + lane,
                    step_by: bank.lane_count,
                    take: 1,
                    size: bank.rom_size,
                    rotate_left: 0,
                    byte_swap: ByteSwap::None,
                },
            )
        })
        .collect()
}

// ─── Decomposition & verification ────────────────────────────────────────────

/// Apply `spec` to `data` and return `(extracted_bytes, crc32)`.
fn decompose_rom(data: &[u8], spec: &ExtractionSpec) -> (Vec<u8>, u32) {
    let bytes = spec.apply(data);
    let mut h = CrcHasher::new();
    h.update(&bytes);
    (bytes, h.finalize())
}

/// Count how many pieces verify against their expected CRC.
fn score_candidate(cand_data: &[u8], specs: &[(usize, ExtractionSpec)], roms: &[RomInfo]) -> usize {
    specs
        .iter()
        .filter(|(rom_idx, spec)| {
            let (_, crc) = decompose_rom(cand_data, spec);
            crc == roms[*rom_idx].crc32
        })
        .count()
}

/// Per-ROM result of decomposing one candidate against one region.
struct Piece {
    rom_idx: usize,
    spec: ExtractionSpec,
    bytes: Vec<u8>,
    verified: bool,
}

/// Attempt to decompose `cand_data` according to `region_spec`.
/// Returns `None` only for `Unsupported` layout.
fn attempt_decomposition(
    cand_data: &[u8],
    region_spec: &RegionSpec,
    roms: &[RomInfo],
) -> Option<Vec<Piece>> {
    let specs = build_specs(region_spec, roms)?;
    let pieces = specs
        .into_iter()
        .map(|(rom_idx, spec)| {
            let (bytes, crc) = decompose_rom(cand_data, &spec);
            let verified = crc == roms[rom_idx].crc32;
            Piece {
                rom_idx,
                spec,
                bytes,
                verified,
            }
        })
        .collect();
    Some(pieces)
}

// ─── Human-readable layout labels ────────────────────────────────────────────

fn layout_label(layout: &RegionLayout) -> String {
    match layout {
        RegionLayout::SingleRom(_) => "single ROM".to_string(),
        RegionLayout::Concatenated => "concatenated".to_string(),
        RegionLayout::FullyInterleaved(b) => {
            let total_kb = b.lane_count * b.rom_size / 1024;
            format!("{}-way interleave, {}KB", b.lane_count, total_kb)
        }
        RegionLayout::BankedInterleaved(banks) => {
            let total_bytes: usize = banks.iter().map(|b| b.lane_count * b.rom_size).sum();
            format!(
                "{} banks × {}-way interleave, {}KB total",
                banks.len(),
                banks.first().map(|b| b.lane_count).unwrap_or(0),
                total_bytes / 1024,
            )
        }
        RegionLayout::Unsupported => "unsupported".to_string(),
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Run structural matching before the main pipeline.
///
/// For each region whose combined size exactly matches a candidate file:
/// - Decompose the candidate according to the region's layout.
/// - Verify each piece against the DAT CRC.
/// - Write verified (and unverified in guess mode) pieces to `output_dir`.
/// - Mark verified ROMs as `matched` so the pipeline skips them.
///
/// When `gex_mode` is `true` files are not written; instead, `MatchRecord`s
/// for verified pieces are returned for GEX task generation.
pub fn run_structural_pass(
    roms: &mut [RomInfo],
    cands: &mut [Candidate],
    output_dir: &Path,
    gex_mode: bool,
    game_subdirs: bool,
    verbose: bool,
) -> anyhow::Result<Vec<MatchRecord>> {
    let region_specs = analyze_regions(roms);
    if region_specs.is_empty() {
        return Ok(Vec::new());
    }

    // Build combined_size → [cand_idx] lookup.
    let mut size_to_cands: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ci, cand) in cands.iter().enumerate() {
        size_to_cands.entry(cand.data.len()).or_default().push(ci);
    }

    let mut all_records: Vec<MatchRecord> = Vec::new();

    'region: for region_spec in &region_specs {
        if matches!(region_spec.layout, RegionLayout::Unsupported) {
            if verbose {
                eprintln!(
                    "Structural: region {:?} has unsupported layout, skipping",
                    region_spec.region_key
                );
            }
            continue;
        }

        // Skip regions where every ROM is already matched.
        if region_spec.all_rom_indices.iter().all(|&i| roms[i].matched) {
            continue;
        }

        let cand_indices = match size_to_cands.get(&region_spec.combined_size) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => {
                if verbose {
                    eprintln!(
                        "Structural: no candidate with size {} for region {:?}",
                        region_spec.combined_size, region_spec.region_key
                    );
                }
                continue;
            }
        };

        if verbose {
            eprintln!(
                "Structural: region {:?} combined_size={}, {} candidate(s)",
                region_spec.region_key,
                region_spec.combined_size,
                cand_indices.len()
            );
        }

        // Select the best candidate (disambiguate when multiple share the size).
        let best_cand_idx = if cand_indices.len() == 1 {
            cand_indices[0]
        } else {
            let Some(specs) = build_specs(region_spec, roms) else {
                continue;
            };
            let scored: Vec<(usize, usize)> = cand_indices
                .iter()
                .map(|&ci| {
                    let score = score_candidate(&cands[ci].data, &specs, roms);
                    (ci, score)
                })
                .collect();

            let best_score = scored.iter().map(|&(_, s)| s).max().unwrap_or(0);
            if best_score == 0 {
                eprintln!(
                    "Structural: region {:?}: {} candidates match size {} but no CRC evidence to disambiguate — skipping",
                    region_spec.region_key,
                    cand_indices.len(),
                    region_spec.combined_size
                );
                continue 'region;
            }
            let best: Vec<_> = scored.iter().filter(|&&(_, s)| s == best_score).collect();
            if best.len() > 1 {
                eprintln!(
                    "Structural: region {:?}: multiple candidates tie at score {} — skipping",
                    region_spec.region_key, best_score
                );
                continue 'region;
            }
            best[0].0
        };

        let pieces = match attempt_decomposition(&cands[best_cand_idx].data, region_spec, roms) {
            Some(p) => p,
            None => continue,
        };

        // Print section header.
        println!(
            "Structural [{}, {}]:",
            region_spec.region_key.1,
            layout_label(&region_spec.layout)
        );

        if !gex_mode {
            std::fs::create_dir_all(output_dir)
                .with_context(|| format!("Creating output dir {}", output_dir.display()))?;
        }

        let cand_path = cands[best_cand_idx].path.clone();

        for piece in &pieces {
            let rom = &roms[piece.rom_idx];
            if piece.verified {
                println!("  ✓  {}  (CRC verified)", rom.name);
                if !gex_mode {
                    let rom_path = if game_subdirs {
                        let dir = output_dir.join(&rom.game);
                        std::fs::create_dir_all(&dir)?;
                        dir.join(&rom.name)
                    } else {
                        output_dir.join(&rom.name)
                    };
                    std::fs::write(&rom_path, &piece.bytes)
                        .with_context(|| format!("Writing {}", rom_path.display()))?;
                }
                all_records.push(MatchRecord {
                    rom_name: rom.name.clone(),
                    cand_path: cand_path.clone(),
                    cand_idx: best_cand_idx,
                    crc32: rom.crc32,
                    spec: piece.spec.clone(),
                    data: MatchedData::Spec(piece.spec.clone()),
                    header: rom.header.clone(),
                });
            } else {
                println!(
                    "  !  {}  [UNVERIFIED — CRC mismatch, expected {:08x}]",
                    rom.name, rom.crc32
                );
                // Always write in guess mode (non-GEX).
                if !gex_mode {
                    let rom_path = if game_subdirs {
                        let dir = output_dir.join(&rom.game);
                        std::fs::create_dir_all(&dir)?;
                        dir.join(&rom.name)
                    } else {
                        output_dir.join(&rom.name)
                    };
                    std::fs::write(&rom_path, &piece.bytes)
                        .with_context(|| format!("Writing {}", rom_path.display()))?;
                }
                // Do NOT mark matched — let the normal pipeline find it elsewhere.
                roms[piece.rom_idx].unverified = true;
            }
        }

        // Mark verified ROMs as matched.
        for piece in &pieces {
            if piece.verified {
                roms[piece.rom_idx].matched = true;
            }
        }

        // Only mark the candidate as fully consumed when every piece verified.
        // If any piece is unverified, leave coverage open so the pipeline can
        // retry with ExactCrc(rotate), ExactCrc(swap2,rotate), etc.
        let all_verified = pieces.iter().all(|p| p.verified);
        if all_verified {
            let full_size = cands[best_cand_idx].data.len();
            let full_spec = ExtractionSpec {
                skip: 0,
                step_by: 1,
                take: 1,
                size: full_size,
                rotate_left: 0,
                byte_swap: ByteSwap::None,
            };
            cands[best_cand_idx].coverage.add(&full_spec);
        }
        // Otherwise leave the candidate uncovered — the pipeline will find the
        // unverified ROM(s) via ExactCrc(rotate) or other heuristics, and the
        // guess file already written by the structural pass will be overwritten
        // with the correct data.
    }

    Ok(all_records)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RomInfo;

    fn make_rom(game: &str, region: &str, offset: u64, size: usize, crc32: u32) -> RomInfo {
        RomInfo {
            name: format!("rom_{region}_{offset}"),
            game: game.to_string(),
            size,
            crc32,
            sha1: None,
            matched: false,
            unverified: false,
            region: Some(region.to_string()),
            offset: Some(offset),
            header: None,
        }
    }

    #[test]
    fn single_rom_region() {
        let roms = vec![make_rom("game", "soundcpu", 0x10000, 131072, 0xdeadbeef)];
        let specs = analyze_regions(&roms);
        assert_eq!(specs.len(), 1);
        assert!(matches!(specs[0].layout, RegionLayout::SingleRom(0)));
        assert_eq!(specs[0].combined_size, 131072);
    }

    #[test]
    fn fully_interleaved_four_way() {
        let roms: Vec<RomInfo> = (0..4)
            .map(|i| make_rom("game", "maindata", i, 524288, i as u32))
            .collect();
        let specs = analyze_regions(&roms);
        assert_eq!(specs.len(), 1);
        assert!(matches!(specs[0].layout, RegionLayout::FullyInterleaved(_)));
        assert_eq!(specs[0].combined_size, 4 * 524288);
    }

    #[test]
    fn concatenated_region() {
        // sound.0: offsets 0 and 0x200000, not stride-1
        let roms = vec![
            make_rom("game", "sound.0", 0, 1048576, 1),
            make_rom("game", "sound.0", 0x200000, 524288, 2),
        ];
        let specs = analyze_regions(&roms);
        assert_eq!(specs.len(), 1);
        assert!(matches!(specs[0].layout, RegionLayout::Concatenated));
        assert_eq!(specs[0].combined_size, 1048576 + 524288);
    }

    #[test]
    fn banked_interleaved_three_banks() {
        // grom: 3 banks × 4 lanes
        let mut roms = Vec::new();
        for bank in 0u64..3 {
            for lane in 0u64..4 {
                roms.push(make_rom(
                    "game",
                    "grom",
                    bank * 0x200000 + lane,
                    524288,
                    (bank * 4 + lane) as u32,
                ));
            }
        }
        let specs = analyze_regions(&roms);
        assert_eq!(specs.len(), 1);
        let RegionLayout::BankedInterleaved(ref banks) = specs[0].layout else {
            panic!("expected BankedInterleaved");
        };
        assert_eq!(banks.len(), 3);
        assert_eq!(banks[0].lane_count, 4);
        assert_eq!(banks[1].physical_offset, 4 * 524288);
        assert_eq!(banks[2].physical_offset, 8 * 524288);
        assert_eq!(specs[0].combined_size, 12 * 524288);
    }

    #[test]
    fn build_specs_concatenated() {
        let roms = vec![
            make_rom("game", "sound.0", 0, 1048576, 1),
            make_rom("game", "sound.0", 0x200000, 524288, 2),
        ];
        let region_specs = analyze_regions(&roms);
        let s = build_specs(&region_specs[0], &roms).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].1.skip, 0);
        assert_eq!(s[0].1.size, 1048576);
        assert_eq!(s[1].1.skip, 1048576);
        assert_eq!(s[1].1.size, 524288);
    }

    #[test]
    fn build_specs_fully_interleaved() {
        let roms: Vec<RomInfo> = (0u64..4)
            .map(|i| make_rom("game", "maindata", i, 524288, i as u32))
            .collect();
        let region_specs = analyze_regions(&roms);
        let s = build_specs(&region_specs[0], &roms).unwrap();
        assert_eq!(s.len(), 4);
        for (lane, &(_, ref es)) in s.iter().enumerate() {
            assert_eq!(es.skip, lane);
            assert_eq!(es.step_by, 4);
            assert_eq!(es.take, 1);
            assert_eq!(es.size, 524288);
        }
    }

    #[test]
    fn build_specs_banked_interleaved() {
        let mut roms = Vec::new();
        for bank in 0u64..3 {
            for lane in 0u64..4 {
                roms.push(make_rom(
                    "game",
                    "grom",
                    bank * 0x200000 + lane,
                    524288,
                    (bank * 4 + lane) as u32,
                ));
            }
        }
        let region_specs = analyze_regions(&roms);
        let s = build_specs(&region_specs[0], &roms).unwrap();
        assert_eq!(s.len(), 12);
        // Bank 1, lane 2: skip = 4*524288 + 2 = 2097154
        let bank1_lane2 = s.iter().find(|(_, es)| es.skip == 2097154).unwrap();
        assert_eq!(bank1_lane2.1.step_by, 4);
        assert_eq!(bank1_lane2.1.size, 524288);
    }

    #[test]
    fn roundtrip_four_way_interleave() {
        // Create a 4-way interleaved file and verify deinterleaving recovers each lane.
        let lane_size = 256usize;
        let n_lanes = 4;
        let mut combined = vec![0u8; n_lanes * lane_size];
        for lane in 0..n_lanes {
            for byte in 0..lane_size {
                combined[byte * n_lanes + lane] = (lane * 17 + byte * 3) as u8;
            }
        }

        let roms: Vec<RomInfo> = (0u64..n_lanes as u64)
            .map(|i| {
                let mut lane_bytes = vec![0u8; lane_size];
                for b in 0..lane_size {
                    lane_bytes[b] = (i as usize * 17 + b * 3) as u8;
                }
                let mut h = crc32fast::Hasher::new();
                h.update(&lane_bytes);
                make_rom("game", "region", i, lane_size, h.finalize())
            })
            .collect();

        let region_specs = analyze_regions(&roms);
        let specs = build_specs(&region_specs[0], &roms).unwrap();
        for (rom_idx, spec) in &specs {
            let (extracted, crc) = decompose_rom(&combined, spec);
            assert_eq!(crc, roms[*rom_idx].crc32, "lane {rom_idx} CRC mismatch");
            assert_eq!(extracted.len(), lane_size);
        }
    }
}
