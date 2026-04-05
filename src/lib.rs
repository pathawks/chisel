pub mod config;
pub mod crc32_window;
pub mod extraction;
pub mod heuristics;
pub mod structural;
pub mod types;
pub mod utils;

pub use heuristics::{ExactCrc, SlidingWindow};
pub use types::{Candidate, MatchRecord, RomInfo};

#[cfg(test)]
pub mod test_support;

/// Apply one `Found` result: drain matching ROMs from `pending`, mark them,
/// update coverage, collect records. Returns whether any ROMs were matched.
///
/// The `write_rom` callback is invoked for each newly matched ROM before it is
/// marked; pass `|_, _| Ok(())` in contexts where no file output is needed.
#[allow(clippy::too_many_arguments)]
pub fn apply_found<F>(
    found: &types::Found,
    cand_idx: usize,
    cands: &mut [Candidate],
    pending: &mut types::Pending,
    roms: &mut [RomInfo],
    unmatched: &mut usize,
    mut records: Option<&mut Vec<MatchRecord>>,
    verbose: bool,
    mut write_rom: F,
) -> anyhow::Result<bool>
where
    F: FnMut(&RomInfo, &[u8]) -> anyhow::Result<()>,
{
    use sha1::{Digest, Sha1};
    use types::MatchedData;

    let MatchedData::Spec(ref spec) = found.data;
    let bytes_owned = spec.apply(&cands[cand_idx].data);

    let mut sha1_cache: Option<String> = None;
    let mut sha1_with_header_cache: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    let matched: Vec<_> = pending.drain_crc_matching(found.size, found.crc, |rid| {
        if roms[rid].matched {
            return false;
        }
        // For ROMs with headers, SHA1 covers header+content
        let sha1_hex = if roms[rid].header.is_some() {
            sha1_with_header_cache
                .entry(rid)
                .or_insert_with(|| {
                    let mut hasher = Sha1::new();
                    hasher.update(roms[rid].header.as_ref().unwrap());
                    hasher.update(&bytes_owned);
                    format!("{:x}", hasher.finalize())
                })
                .as_str()
        } else {
            if sha1_cache.is_none() {
                sha1_cache = Some(format!("{:x}", Sha1::digest(&bytes_owned[..])));
            }
            sha1_cache.as_ref().unwrap().as_str()
        };
        match &roms[rid].sha1 {
            Some(exp) => sha1_hex == exp,
            None => true,
        }
    });

    if matched.is_empty() {
        return Ok(false);
    }

    for rid in matched {
        write_rom(&roms[rid], &bytes_owned)?;
        roms[rid].matched = true;
        *unmatched -= 1;

        if let Some(ref mut recs) = records {
            recs.push(MatchRecord {
                rom_name: roms[rid].name.clone(),
                cand_path: cands[cand_idx].path.clone(),
                cand_idx,
                crc32: found.crc,
                spec: spec.clone(),
                data: found.data.clone(),
                header: roms[rid].header.clone(),
            });
        }

        if verbose {
            println!("Found {} [{}]", roms[rid], found.data);
        }
    }
    cands[cand_idx].coverage.add(spec);

    Ok(true)
}
