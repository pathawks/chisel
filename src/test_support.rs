use crate::types::{Candidate, CandidateSource, Found, Heuristic, MatchRecord, Pending, RomInfo};
use crc32fast;
use sha1::{Digest, Sha1};
use std::collections::HashMap;

pub fn make_rom(name: &str, data: &[u8]) -> RomInfo {
    RomInfo {
        name: name.to_string(),
        game: String::new(),
        size: data.len(),
        crc32: crc32fast::hash(data),
        sha1: Some(format!("{:x}", Sha1::digest(data))),
        matched: false,
        unverified: false,
        region: None,
        offset: None,
        header: None,
    }
}

/// Create a RomInfo with a header. The DAT describes the full file (header + content),
/// so size/crc/sha1 cover header+content. We derive content-only size/crc for matching.
pub fn make_rom_with_header(name: &str, header: &[u8], content: &[u8]) -> RomInfo {
    let mut full = Vec::with_capacity(header.len() + content.len());
    full.extend_from_slice(header);
    full.extend_from_slice(content);
    let full_crc = crc32fast::hash(&full);
    let full_sha1 = format!("{:x}", Sha1::digest(&full));
    let content_crc = crate::utils::derive_content_crc(full_crc, header, content.len());
    RomInfo {
        name: name.to_string(),
        game: String::new(),
        size: content.len(),
        crc32: content_crc,
        sha1: Some(full_sha1),
        matched: false,
        unverified: false,
        region: None,
        offset: None,
        header: Some(header.to_vec()),
    }
}

pub fn make_candidate(path: &str, data: Vec<u8>) -> Candidate {
    Candidate {
        path: std::path::PathBuf::from(path),
        data,
        source: CandidateSource::Plain,
        coverage: Default::default(),
    }
}

/// Convenience to run one heuristic and return matches.
/// This groups roms into a single bucket (by size) for the test case.
/// Test-only helper: run a single heuristic over the current ROMs/candidates
/// without mutating `roms`/`pending` and collect `MatchRecord`s.
/// This mirrors the old behavior just for unit tests.
//
/// Test-only helper: run one heuristic, mutate `roms`/`pending` like production,
/// and collect `MatchRecord`s. No files are written.
pub fn run_heuristic(
    heuristic: &dyn Heuristic,
    roms: &mut [RomInfo],
    cands: &mut [Candidate],
) -> Vec<MatchRecord> {
    // Build buckets from ROMs (grouped by size)
    let mut pending = Pending::build(roms);

    // Index candidates by size
    let mut cand_by_size: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, cand) in cands.iter().enumerate() {
        cand_by_size.entry(cand.data.len()).or_default().push(i)
    }

    let mut out = Vec::new();
    let mut unmatched = roms.iter().filter(|r| !r.matched).count();

    // Iterate sizes without holding borrows across mutation of `pending`
    let sizes: Vec<usize> = pending.by_size.keys().copied().collect();
    for size in sizes {
        let cidxs: Vec<_> = cands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.data.len() >= size)
            .map(|(i, _)| i)
            .collect();

        for ci in cidxs.iter().copied() {
            if cands[ci].is_fully_covered() {
                continue;
            }

            // ---- Immutable phase: borrow bucket & candidate, collect small Vec<Found> ----
            let founds: Vec<Found> = {
                let bucket = match pending.bucket(size) {
                    Some(b) => b,
                    None => continue, // bucket may have been emptied
                };
                let cand_ref = &cands[ci];
                heuristic.probe_cand(cand_ref, bucket).collect()
            }; // <- drop borrows before mutating

            // ---- Mutable phase: drain, mark matched, update coverage, collect records ----
            for found in founds {
                crate::apply_found(
                    &found,
                    ci,
                    cands,
                    &mut pending,
                    roms,
                    &mut unmatched,
                    Some(&mut out),
                    false,
                    |_, _| Ok(()),
                )
                .expect("no file IO in test helper");
            }
        }
    }

    out
}
