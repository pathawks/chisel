use crate::crc32_window;
use crate::extraction::swap_bytes;
use crate::types::{Bucket, ByteSwap, Candidate, ExtractionSpec, Found, Heuristic, MatchedData};

pub struct ExactCrc {
    pub byte_swap: ByteSwap,
    pub rotate: bool,
    name: String,
}

impl ExactCrc {
    pub fn new(byte_swap: ByteSwap, rotate: bool) -> Self {
        let name = match (byte_swap, rotate) {
            (ByteSwap::None, false) => "ExactCrc".to_string(),
            (ByteSwap::Swap2, false) => "ExactCrc(swap2)".to_string(),
            (ByteSwap::Swap4, false) => "ExactCrc(swap4)".to_string(),
            (ByteSwap::None, true) => "ExactCrc(rotate)".to_string(),
            (ByteSwap::Swap2, true) => "ExactCrc(swap2,rotate)".to_string(),
            (ByteSwap::Swap4, true) => "ExactCrc(swap4,rotate)".to_string(),
        };
        ExactCrc { byte_swap, rotate, name }
    }

    fn granularity(&self) -> usize {
        match self.byte_swap {
            ByteSwap::None => 1,
            ByteSwap::Swap2 => 2,
            ByteSwap::Swap4 => 4,
        }
    }
}

impl Default for ExactCrc {
    fn default() -> Self {
        Self::new(ByteSwap::None, false)
    }
}

impl Heuristic for ExactCrc {
    fn name(&self) -> &str {
        &self.name
    }

    fn estimate_cost(&self, size: usize, bucket: &Bucket, cands: &[Candidate]) -> Option<u64> {
        if bucket.map.is_empty() || cands.is_empty() {
            return None;
        }
        if size != bucket.size {
            return None;
        }
        let gran = self.granularity();
        if !size.is_multiple_of(gran) {
            return None;
        }
        if self.rotate {
            // Rolling rotate: O(data.len()) per candidate
            let work: u64 = cands
                .iter()
                .filter(|c| c.data.len() == size && c.data.len().is_multiple_of(gran))
                .map(|c| c.data.len() as u64)
                .sum();
            if work > 0 { Some(work) } else { None }
        } else {
            // Exact match: O(1) per candidate
            let count = cands
                .iter()
                .filter(|c| c.data.len() == size && c.data.len().is_multiple_of(gran))
                .count() as u64;
            if count > 0 { Some(count) } else { None }
        }
    }

    fn probe_cand<'a>(
        &'a self,
        cand: &'a Candidate,
        bucket: &'a Bucket,
    ) -> Box<dyn Iterator<Item = Found> + 'a> {
        let size = bucket.size;
        let gran = self.granularity();

        if cand.data.len() != size || !size.is_multiple_of(gran) {
            return Box::new(std::iter::empty());
        }

        match (self.rotate, self.byte_swap) {
            (false, ByteSwap::None) => {
                // Plain exact CRC match
                let crc = crc32fast::hash(&cand.data);
                if !bucket.matcher.contains(crc) {
                    return Box::new(std::iter::empty());
                }
                Box::new(std::iter::once(Found {
                    size,
                    crc,
                    data: MatchedData::Spec(ExtractionSpec {
                        skip: 0,
                        step_by: 1,
                        take: 1,
                        size,
                        ..Default::default()
                    }),
                }))
            }
            (false, swap) => {
                // Byte-swap then exact CRC match
                let mut buf = cand.data.clone();
                swap_bytes(&mut buf, swap);
                let crc = crc32fast::hash(&buf);
                if !bucket.matcher.contains(crc) {
                    return Box::new(std::iter::empty());
                }
                Box::new(std::iter::once(Found {
                    size,
                    crc,
                    data: MatchedData::Spec(ExtractionSpec {
                        skip: 0,
                        step_by: 1,
                        take: 1,
                        size,
                        byte_swap: swap,
                        ..Default::default()
                    }),
                }))
            }
            (true, ByteSwap::None) => {
                // Rolling rotate CRC scan
                let targets: Vec<u32> = bucket.map.keys().copied().collect();
                let hits = crc32_window::find_rotate_crc_rolling_any(&cand.data, &targets);
                Box::new(hits.into_iter().map(move |(rotate_left_raw, crc)| {
                    let adj = (size - rotate_left_raw % size) % size;
                    Found {
                        size,
                        crc,
                        data: MatchedData::Spec(ExtractionSpec {
                            size,
                            rotate_left: adj,
                            ..Default::default()
                        }),
                    }
                }))
            }
            (true, swap) => {
                // Pre-swap data, rolling rotate scan, filter to word-aligned rotations only.
                // Exploits commutativity: byteswap(rotate_r(X, k)) == rotate_r(byteswap(X), k)
                // for word-aligned k.
                let gran = self.granularity();
                let mut swapped = cand.data.clone();
                swap_bytes(&mut swapped, swap);
                let targets: Vec<u32> = bucket.map.keys().copied().collect();
                let hits = crc32_window::find_rotate_crc_rolling_any(&swapped, &targets);
                Box::new(hits.into_iter().filter_map(move |(rotate_left_raw, crc)| {
                    let adj = (size - rotate_left_raw % size) % size;
                    if !adj.is_multiple_of(gran) {
                        return None;
                    }
                    Some(Found {
                        size,
                        crc,
                        data: MatchedData::Spec(ExtractionSpec {
                            size,
                            rotate_left: adj,
                            byte_swap: swap,
                            ..Default::default()
                        }),
                    })
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::types::ByteSwap;
    use crate::types::ExtractionSpec;
    use std::path::PathBuf;

    // ---- ExactCrc (no byte_swap, no rotate) ----

    #[test]
    fn exact_crc_matches_full_candidate() {
        let data = b"HELLO_ROM";
        let mut roms = vec![make_rom("r1", data)];
        let mut cands = vec![make_candidate("cand1", data.to_vec())];

        let matches = run_heuristic(&ExactCrc::default(), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let mr = &matches[0];
        assert_eq!(mr.rom_name, "r1");
        assert_eq!(mr.cand_path, PathBuf::from("cand1"));
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.step_by, 1);
        assert_eq!(mr.spec.size, data.len());
        assert_eq!(mr.spec.rotate_left, 0);
        let extracted = mr.spec.apply(&cands[0].data);
        assert_eq!(extracted, data);
        assert!(roms[0].matched);
        assert!(cands[0].coverage.is_fully_covered(mr.spec.size));
    }

    #[test]
    fn no_match_if_crc_differs() {
        let mut roms = vec![make_rom("r1", b"A")];
        let mut cands = vec![make_candidate("cand1", b"B".to_vec())];

        let matches = run_heuristic(&ExactCrc::default(), &mut roms, &mut cands);
        assert!(matches.is_empty());
        assert!(!roms[0].matched);
        assert!(!cands[0].coverage.is_fully_covered(1));
    }

    #[test]
    fn describe_string_contains_components() {
        let spec = ExtractionSpec {
            skip: 2,
            step_by: 3,
            take: 1,
            size: 5,
            rotate_left: 1,
            byte_swap: ByteSwap::None,
        };
        let desc = spec.to_string();
        assert!(desc.contains("skip 2"));
        assert!(desc.contains("take every 3"));
        assert!(desc.contains("left-rotate by 1"));
    }

    #[test]
    fn exact_crc_matches_zero_size_rom() {
        let mut roms = vec![make_rom("r0", b"")];
        let mut cands = vec![make_candidate("cand0", Vec::new())];

        let matches = run_heuristic(&ExactCrc::default(), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let mr = &matches[0];
        assert_eq!(mr.rom_name, "r0");
        assert_eq!(mr.cand_path, PathBuf::from("cand0"));
        assert_eq!(mr.crc32, crc32fast::hash(&[]));
        assert_eq!(mr.spec.size, 0);
        assert!(roms[0].matched);
        assert!(cands[0].coverage.is_fully_covered(0));
    }

    // ---- ExactCrc with byte_swap (absorbed from ByteSwapExact) ----

    #[test]
    fn exact_swap2_matches() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut cands = vec![make_candidate("c", b"badc".to_vec())];

        let h = ExactCrc::new(ByteSwap::Swap2, false);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1, "should find one match");
        let spec = &matches[0].spec;
        let extracted = spec.apply(&cands[0].data);
        assert_eq!(extracted, rom_data, "apply should recover ROM data");
    }

    #[test]
    fn exact_swap4_matches() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut cands = vec![make_candidate("c", b"dcba".to_vec())];

        let h = ExactCrc::new(ByteSwap::Swap4, false);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let extracted = matches[0].spec.apply(&cands[0].data);
        assert_eq!(extracted, rom_data);
    }

    #[test]
    fn exact_no_match_wrong_swap() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut cands = vec![make_candidate("c", b"badc".to_vec())];

        let h = ExactCrc::new(ByteSwap::Swap4, false);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert!(matches.is_empty());
    }

    #[test]
    fn exact_no_match_if_not_swapped() {
        let rom_data = b"WXYZ";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut cands = vec![make_candidate("c", b"WXYZ".to_vec())];

        let h = ExactCrc::new(ByteSwap::Swap2, false);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert!(matches.is_empty());
    }

    #[test]
    fn exact_skip_if_wrong_size() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut cands = vec![make_candidate("c", b"badcXX".to_vec())];

        let h = ExactCrc::new(ByteSwap::Swap2, false);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert!(matches.is_empty());
    }

    #[test]
    fn exact_skip_if_odd_size_swap2() {
        let rom_data = b"abc";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut cands = vec![make_candidate("c", b"bac".to_vec())];

        let h = ExactCrc::new(ByteSwap::Swap2, false);
        assert!(
            h.estimate_cost(3, &crate::types::Pending::build(&roms).by_size[&3], &cands)
                .is_none()
        );
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert!(matches.is_empty());
    }

    // ---- ExactCrc with rotate (absorbed from Rotate) ----

    #[test]
    fn rotate_finds_rotated_version() {
        let orig = b"ABCDEFG";
        let mut roms = vec![make_rom("r1", orig)];
        let rotated = b"CDEFGAB".to_vec();
        let mut cands = vec![make_candidate("cand1", rotated)];

        let h = ExactCrc::new(ByteSwap::None, true);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 1);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.rotate_left, 5);
    }

    #[test]
    fn rotate_finds_rotated_version_among_multiple() {
        let orig = b"ABCDEFG";
        let mut roms = vec![make_rom("r0", b"0000000"), make_rom("r1", orig)];
        let rotated = b"CDEFGAB".to_vec();
        let mut cands = vec![make_candidate("cand1", rotated)];

        let h = ExactCrc::new(ByteSwap::None, true);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1, "expected exactly one match, got {}", matches.len());
        assert!(roms[1].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 1);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.rotate_left, 5);
    }

    #[test]
    fn rotate_flipped() {
        let orig = b"01";
        let mut roms = vec![make_rom("r1", orig)];
        let rotated = b"10".to_vec();
        let mut cands = vec![make_candidate("cand1", rotated)];

        let h = ExactCrc::new(ByteSwap::None, true);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 1);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.rotate_left, 1);
    }
}
