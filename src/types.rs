use std::path::PathBuf;

/// Coverage tracking for claimed byte ranges
#[derive(Default, Clone)]
pub struct Coverage {
    intervals: Vec<(usize, usize)>, // half-open [start, end)
}

impl Coverage {
    /// Mark coverage according to an extraction spec on the original candidate data.
    /// The extraction spec is assumed to reference valid byte positions within the candidate.
    pub fn add(&mut self, spec: &ExtractionSpec) {
        if spec.step_by == 0 {
            return;
        }

        if spec.step_by == 1 {
            // Contiguous region
            let start = spec.skip;
            let end = start.saturating_add(spec.size);
            self.intervals.push((start, end));
        } else {
            // Non-contiguous: each extracted chunk comes from skip + i*step_by
            let mut remaining = spec.size;
            let mut pos = spec.skip;
            while remaining > 0 {
                let take = spec.take.min(remaining);
                self.intervals.push((pos, pos + take));
                pos += spec.step_by;
                remaining -= take;
            }
        }
        self.normalize();
    }

    fn normalize(&mut self) {
        if self.intervals.is_empty() {
            return;
        }
        self.intervals.sort_unstable();
        let mut merged = Vec::new();
        let mut cur = self.intervals[0];
        for &(s, e) in &self.intervals[1..] {
            if s <= cur.1 {
                cur.1 = cur.1.max(e);
            } else {
                merged.push(cur);
                cur = (s, e);
            }
        }
        merged.push(cur);
        self.intervals = merged;
    }

    /// Returns true if the entire candidate of length `size` is covered.
    pub fn is_fully_covered(&self, size: usize) -> bool {
        self.intervals.len() == 1 && self.intervals[0] == (0, size)
    }

    /// Number of bytes not yet covered within a candidate of length `size`.
    pub fn remaining(&self, size: usize) -> usize {
        let covered: usize = self
            .intervals
            .iter()
            .map(|&(s, e)| e.saturating_sub(s))
            .sum();
        size.saturating_sub(covered)
    }

    /// Returns the uncovered byte ranges within a candidate of length `size` as
    /// half-open `[start, end)` intervals.
    pub fn uncovered_ranges(&self, size: usize) -> Vec<(usize, usize)> {
        if size == 0 {
            return vec![];
        }
        let mut gaps = Vec::new();
        let mut pos: usize = 0;
        for &(start, end) in &self.intervals {
            if start > pos {
                gaps.push((pos, start));
            }
            pos = end.max(pos);
        }
        if pos < size {
            gaps.push((pos, size));
        }
        gaps
    }
}

use nohash_hasher::BuildNoHashHasher;
use std::collections::{HashMap, HashSet};

pub type Crc = u32;
type RomId = usize;

pub enum CrcMatcher {
    Small(Vec<Crc>),                             // ≤8
    Sorted(Vec<Crc>),                            // 9..=512 (binary_search)
    Large(HashSet<Crc, BuildNoHashHasher<Crc>>), // big
}

impl CrcMatcher {
    #[inline]
    pub fn contains(&self, c: Crc) -> bool {
        match self {
            CrcMatcher::Small(v) => v.contains(&c),
            CrcMatcher::Sorted(v) => v.binary_search(&c).is_ok(),
            CrcMatcher::Large(h) => h.contains(&c),
        }
    }
}

type CrcMap = HashMap<Crc, smallvec::SmallVec<[RomId; 2]>, BuildNoHashHasher<Crc>>;

pub struct Bucket {
    pub size: usize,
    pub matcher: CrcMatcher,
    pub map: CrcMap, // crc -> rom ids
}

pub struct Pending {
    pub by_size: HashMap<usize, Bucket>,
}

impl Pending {
    pub fn build(roms: &[RomInfo]) -> Self {
        let mut tmp: HashMap<usize, CrcMap> = HashMap::default();
        for (id, r) in roms.iter().enumerate() {
            tmp.entry(r.size)
                .or_default()
                .entry(r.crc32)
                .or_default()
                .push(id);
        }
        let by_size = tmp
            .into_iter()
            .map(|(size, cmap)| {
                let mut keys: Vec<Crc> = cmap.keys().copied().collect();
                let matcher = match keys.len() {
                    0..=8 => CrcMatcher::Small(keys),
                    9..=512 => {
                        keys.sort_unstable();
                        keys.dedup();
                        CrcMatcher::Sorted(keys)
                    }
                    _ => CrcMatcher::Large(keys.into_iter().collect()),
                };
                (
                    size,
                    Bucket {
                        size,
                        matcher,
                        map: cmap,
                    },
                )
            })
            .collect();
        Self { by_size }
    }

    #[inline]
    pub fn bucket(&self, size: usize) -> Option<&Bucket> {
        self.by_size.get(&size)
    }

    /// Remove ROM ids for (size, crc) that satisfy `pred`; returns removed ids.
    pub fn drain_crc_matching<F>(&mut self, size: usize, crc: Crc, mut pred: F) -> Vec<RomId>
    where
        F: FnMut(RomId) -> bool,
    {
        let mut removed = Vec::new();
        {
            let Some(bucket) = self.by_size.get_mut(&size) else {
                return removed;
            };
            if let Some(rids) = bucket.map.get_mut(&crc) {
                let mut i = 0;
                while i < rids.len() {
                    if pred(rids[i]) {
                        removed.push(rids.swap_remove(i));
                    } else {
                        i += 1;
                    }
                }
                if rids.is_empty() {
                    bucket.map.remove(&crc);
                }
            }
        } // bucket borrow ends here
        if self.by_size.get(&size).is_some_and(|b| b.map.is_empty()) {
            self.by_size.remove(&size);
        }
        removed
    }

    /// Remove all ROM ids for (size, crc); returns removed ids.
    pub fn drain_crc(&mut self, size: usize, crc: Crc) -> Vec<RomId> {
        self.drain_crc_matching(size, crc, |_| true)
    }
}

/// Represents one expected ROM entry
#[derive(Debug, Clone)]
pub struct RomInfo {
    pub name: String,
    pub game: String, // MAME machine short name this ROM belongs to
    pub size: usize,
    pub crc32: u32,
    pub sha1: Option<String>,
    pub matched: bool,
    pub unverified: bool,
    /// MAME region tag (e.g. "maindata", "grom", "sound.0").
    pub region: Option<String>,
    /// Address-space offset of this chip within its region (hex in the DAT).
    pub offset: Option<u64>,
    /// Optional header bytes to prepend when writing output.
    /// When present, `size` and `crc32` describe the content only (header stripped),
    /// while `sha1` describes the full file (header + content).
    pub header: Option<Vec<u8>>,
}

impl std::fmt::Display for RomInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "{}", self.name)
    }
}

/// Records how a `Candidate`'s bytes were obtained — needed for GEX output.
#[derive(Clone, Debug, Default)]
pub enum CandidateSource {
    /// Raw file read directly from disk.
    #[default]
    Plain,
    /// Single file decompressed from a gzip stream.
    Gzip { archive: PathBuf },
    /// One member extracted from a zip archive.
    Zip { archive: PathBuf, member: String },
    /// Decompressed from an LZMA/XZ block found at `offset` inside `parent`.
    Lzma { parent: PathBuf, offset: usize },
    /// One entry extracted from a KPKA/PAK archive (index is entry ordinal, 0-based).
    Kpka { archive: PathBuf, index: usize },
}

/// Candidate file scanned from input_dir
#[derive(Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub data: Vec<u8>,
    pub source: CandidateSource,
    pub coverage: Coverage,
}

impl std::fmt::Display for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        let name = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or("???".into());
        write!(f, "{}", name)?;
        match &self.source {
            CandidateSource::Plain => {}
            CandidateSource::Gzip { archive } => {
                let a = archive
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or("???".into());
                write!(f, "[gzip in {}]", a)?;
            }
            CandidateSource::Zip { archive, member } => {
                let a = archive
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or("???".into());
                write!(f, "[zip:{} in {}]", member, a)?;
            }
            CandidateSource::Lzma { parent, offset } => {
                let p = parent
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or("???".into());
                write!(f, "[lzma@0x{:x} in {}]", offset, p)?;
            }
            CandidateSource::Kpka { archive, index } => {
                let a = archive
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or("???".into());
                write!(f, "[kpka[{}] in {}]", index, a)?;
            }
        }
        Ok(())
    }
}
impl Candidate {
    pub fn is_fully_covered(&self) -> bool {
        self.coverage.is_fully_covered(self.data.len())
    }
}

/// Rough value estimate for attempting to match `cands` against `bucket` using ROM `size`.
/// Larger return values indicate a higher expected payoff.
pub fn estimate_value(size: usize, bucket: &Bucket, cands: &[Candidate]) -> u64 {
    let roms: u64 = bucket.map.values().map(|ids| ids.len() as u64).sum();
    if roms == 0 {
        return 0;
    }
    cands
        .iter()
        .filter(|cand| cand.data.len() >= size)
        .map(|cand| {
            let remaining = cand.coverage.remaining(cand.data.len());
            if remaining == 0 {
                return 0;
            }
            let cover = size.min(remaining) as u64;
            let bonus = if cover == remaining as u64 { 2 } else { 1 };
            cover.saturating_mul(roms).saturating_mul(bonus)
        })
        .sum()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ByteSwap {
    None,
    Swap2, // 16-bit swap
    Swap4, // 32-bit swap
}

#[derive(Clone, Debug)]
pub struct ExtractionSpec {
    pub skip: usize,
    pub step_by: usize,
    pub take: usize,
    pub size: usize,
    pub rotate_left: usize,
    pub byte_swap: ByteSwap,
}

impl Default for ExtractionSpec {
    fn default() -> Self {
        Self {
            skip: 0,
            step_by: 1,
            take: 1,
            size: 0,
            rotate_left: 0,
            byte_swap: ByteSwap::None,
        }
    }
}

impl std::fmt::Display for ExtractionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if self.skip != 0 {
            if self.skip.is_multiple_of(self.step_by) {
                parts.push(format!(
                    "skip {}*{} bytes",
                    self.skip / self.size,
                    self.size
                ));
            } else {
                parts.push(format!(
                    "skip {} byte{}",
                    self.skip,
                    match self.skip {
                        1 => "",
                        _ => "s",
                    }
                ));
            }
        }
        if self.step_by != 1 || self.take != 1 {
            if self.take == 1 {
                parts.push(format!("take every {} bytes", self.step_by));
            } else {
                parts.push(format!(
                    "take {} bytes every {} bytes",
                    self.take, self.step_by
                ));
            }
        }
        if self.rotate_left != 0 {
            parts.push(format!("left-rotate by {}", self.rotate_left));
        }
        if self.size != 0 {
            parts.push(format!(
                "consume {} byte{}",
                self.size,
                match self.size {
                    1 => "",
                    _ => "s",
                }
            ));
        }
        match self.byte_swap {
            ByteSwap::None => {}
            ByteSwap::Swap2 => parts.push("byte-swap 16-bit".to_string()),
            ByteSwap::Swap4 => parts.push("byte-swap 32-bit".to_string()),
        }

        match parts.len() {
            0 => write!(f, "no transformations required"),
            1 => write!(f, "{}", parts[0]),
            _ => write!(f, "{}", parts.join(", ")),
        }
    }
}

impl std::str::FromStr for ByteSwap {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(ByteSwap::None),
            "swap2" => Ok(ByteSwap::Swap2),
            "swap4" => Ok(ByteSwap::Swap4),
            _ => Err(format!(
                "unknown byte_swap value: {s:?} (expected none, swap2, swap4)"
            )),
        }
    }
}

impl std::str::FromStr for ExtractionSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut spec = ExtractionSpec::default();
        let mut size_set = false;
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (key, val) = part
                .split_once('=')
                .ok_or_else(|| format!("expected key=value, got {part:?}"))?;
            match key.trim() {
                "skip" => spec.skip = val.trim().parse().map_err(|e| format!("bad skip: {e}"))?,
                "step_by" => {
                    spec.step_by = val
                        .trim()
                        .parse()
                        .map_err(|e| format!("bad step_by: {e}"))?
                }
                "take" => spec.take = val.trim().parse().map_err(|e| format!("bad take: {e}"))?,
                "size" => {
                    spec.size = val.trim().parse().map_err(|e| format!("bad size: {e}"))?;
                    size_set = true;
                }
                "rotate_left" => {
                    spec.rotate_left = val
                        .trim()
                        .parse()
                        .map_err(|e| format!("bad rotate_left: {e}"))?
                }
                "byte_swap" => spec.byte_swap = val.trim().parse()?,
                other => return Err(format!("unknown key: {other:?}")),
            }
        }
        if !size_set {
            // size=0 signals "use entire remaining file"; caller must fill it in
            spec.size = 0;
        }
        if spec.step_by == 0 {
            return Err("step_by must be >= 1".to_string());
        }
        if spec.take == 0 {
            return Err("take must be >= 1".to_string());
        }
        if spec.step_by < spec.take {
            return Err(format!(
                "step_by ({}) must be >= take ({})",
                spec.step_by, spec.take
            ));
        }
        Ok(spec)
    }
}

#[derive(Clone, Debug)]
pub enum MatchedData {
    Spec(ExtractionSpec),
}

impl std::fmt::Display for MatchedData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MatchedData::Spec(es) => write!(f, "{}", es),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MatchRecord {
    pub rom_name: String,
    pub cand_path: PathBuf,
    pub spec: ExtractionSpec,
    pub cand_idx: usize, // index into cands; easy access to source bytes
    pub crc32: u32,
    pub data: MatchedData,
    /// Optional header bytes to prepend when writing output.
    pub header: Option<Vec<u8>>,
}

impl std::fmt::Display for MatchRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} - {}", self.cand_path, self.data)
    }
}

pub struct Found {
    pub size: usize,
    pub crc: u32,
    pub data: MatchedData,
}

/// Trait for matching heuristics
pub trait Heuristic {
    fn name(&self) -> &str;

    /// Estimate a cost/priority for (size, bucket, these candidates).
    /// Lower numbers indicate a higher expected value relative to work.
    /// Returning `None` skips scheduling.
    fn estimate_cost(&self, size: usize, bucket: &Bucket, cands: &[Candidate]) -> Option<u64> {
        // Default: estimate total work for rolling CRC windows.
        if bucket.map.is_empty() || cands.is_empty() {
            return None;
        }
        let work: u64 = cands
            .iter()
            .map(|c| {
                let n = c.data.len();
                n.saturating_sub(size).saturating_add(1) as u64
            })
            .sum();
        if work > 0 { Some(work) } else { None }
    }

    /// Stream matches for ONE candidate against ONE size bucket.
    fn probe_cand<'a>(
        &'a self,
        cand: &'a Candidate,
        bucket: &'a Bucket,
    ) -> Box<dyn Iterator<Item = Found> + 'a>;
}
