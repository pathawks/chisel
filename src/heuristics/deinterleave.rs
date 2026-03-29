use crate::crc32_window;
use crate::extraction::swap_bytes;
use crate::types::{Bucket, ByteSwap, Candidate, ExtractionSpec, Found, Heuristic, MatchedData};

pub struct Deinterleave {
    weave: usize,
    word_size: usize,
    pub byte_swap: ByteSwap,
    pub rotate: bool,
    name: String,
}

impl Deinterleave {
    pub fn new(weave: usize, word_size: usize) -> Self {
        Self::with_opts(weave, word_size, ByteSwap::None, false)
    }

    pub fn with_opts(weave: usize, word_size: usize, byte_swap: ByteSwap, rotate: bool) -> Self {
        let bs = match byte_swap {
            ByteSwap::None => String::new(),
            ByteSwap::Swap2 => ",swap2".to_string(),
            ByteSwap::Swap4 => ",swap4".to_string(),
        };
        let rot = if rotate { ",rotate" } else { "" };
        let name_str = if word_size == 1 {
            format!("Deinterleave({}{}{})", weave, bs, rot)
        } else {
            format!("Deinterleave({},{}{}{})", weave, word_size, bs, rot)
        };
        Deinterleave {
            weave,
            word_size,
            byte_swap,
            rotate,
            name: name_str,
        }
    }

    fn granularity(&self) -> usize {
        match self.byte_swap {
            ByteSwap::None => 1,
            ByteSwap::Swap2 => 2,
            ByteSwap::Swap4 => 4,
        }
    }
}

impl Heuristic for Deinterleave {
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

        let stride = self.weave * self.word_size;
        let work = cands
            .iter()
            .flat_map(|cand| {
                let n = cand.data.len();
                (0..self.weave)
                    .map(move |lane| lane * self.word_size)
                    .take_while(move |&skip_bytes| skip_bytes < n)
                    .filter(move |&skip_bytes| {
                        let lane_len_words = (n - skip_bytes).div_ceil(stride);
                        let lane_len = lane_len_words * self.word_size;
                        lane_len == size
                    })
                    .map(|_| {
                        if self.rotate {
                            size as u64 // rolling rotate is O(n)
                        } else {
                            1u64 // exact match is O(1)
                        }
                    })
            })
            .sum::<u64>();

        if work > 0 { Some(work) } else { None }
    }

    fn probe_cand<'a>(
        &'a self,
        cand: &'a Candidate,
        bucket: &'a Bucket,
    ) -> Box<dyn Iterator<Item = Found> + 'a> {
        let mut out = Vec::new();
        let size = bucket.size;
        if size == 0 {
            return Box::new(out.into_iter());
        }

        let stride = self.weave * self.word_size;
        let gran = self.granularity();

        for lane in 0..self.weave {
            let skip_bytes = lane * self.word_size;
            if skip_bytes >= cand.data.len() {
                break;
            }
            let lane_len_words = (cand.data.len() - skip_bytes).div_ceil(stride);
            let lane_len = lane_len_words * self.word_size;
            if lane_len != size {
                continue;
            }

            // Extract lane into a contiguous buffer
            let mut slice = Vec::with_capacity(lane_len);
            let mut idx = skip_bytes;
            while idx < cand.data.len() && slice.len() < lane_len {
                let end = usize::min(idx + self.word_size, cand.data.len());
                slice.extend_from_slice(&cand.data[idx..end]);
                idx += stride;
            }

            match (self.rotate, self.byte_swap) {
                (false, ByteSwap::None) => {
                    let crc = crc32fast::hash(&slice);
                    if bucket.matcher.contains(crc) {
                        out.push(Found {
                            size,
                            crc,
                            data: MatchedData::Spec(ExtractionSpec {
                                skip: skip_bytes,
                                step_by: stride,
                                take: self.word_size,
                                size,
                                ..Default::default()
                            }),
                        });
                    }
                }
                (false, swap) => {
                    if !size.is_multiple_of(gran) {
                        continue;
                    }
                    swap_bytes(&mut slice, swap);
                    let crc = crc32fast::hash(&slice);
                    if bucket.matcher.contains(crc) {
                        out.push(Found {
                            size,
                            crc,
                            data: MatchedData::Spec(ExtractionSpec {
                                skip: skip_bytes,
                                step_by: stride,
                                take: self.word_size,
                                size,
                                byte_swap: swap,
                                ..Default::default()
                            }),
                        });
                    }
                }
                (true, ByteSwap::None) => {
                    let targets: Vec<u32> = bucket.map.keys().copied().collect();
                    let hits = crc32_window::find_rotate_crc_rolling_any(&slice, &targets);
                    for (rotate_left_raw, crc) in hits {
                        let adj = (size - rotate_left_raw % size) % size;
                        out.push(Found {
                            size,
                            crc,
                            data: MatchedData::Spec(ExtractionSpec {
                                skip: skip_bytes,
                                step_by: stride,
                                take: self.word_size,
                                size,
                                rotate_left: adj,
                                ..Default::default()
                            }),
                        });
                    }
                }
                (true, swap) => {
                    if !size.is_multiple_of(gran) {
                        continue;
                    }
                    // Pre-swap the lane, run rolling rotate, filter word-aligned rotations.
                    // Exploits commutativity: byteswap(rotate_r(X, k)) == rotate_r(byteswap(X), k)
                    // for word-aligned k.
                    swap_bytes(&mut slice, swap);
                    let targets: Vec<u32> = bucket.map.keys().copied().collect();
                    let hits = crc32_window::find_rotate_crc_rolling_any(&slice, &targets);
                    for (rotate_left_raw, crc) in hits {
                        let adj = (size - rotate_left_raw % size) % size;
                        if !adj.is_multiple_of(gran) {
                            continue;
                        }
                        out.push(Found {
                            size,
                            crc,
                            data: MatchedData::Spec(ExtractionSpec {
                                skip: skip_bytes,
                                step_by: stride,
                                take: self.word_size,
                                size,
                                rotate_left: adj,
                                byte_swap: swap,
                            }),
                        });
                    }
                }
            }
        }

        Box::new(out.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn interleaved_from_0() {
        let orig = b"0123456";
        let interleaved = b"0_1_2_3_4_5_6_".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn two_files_interleaved_from_0() {
        let orig = b"0123456";
        let interleaved = b"0a1b2c3d4e5f6g".to_vec();
        let mut roms = vec![make_rom("r1", orig), make_rom("r2", b"abcdefg")];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 2);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_words_16bit() {
        let rom1 = b"abef";
        let rom2 = b"cdgh";
        let interleaved = b"abcdefgh".to_vec();
        let mut roms = vec![make_rom("r1", rom1), make_rom("r2", rom2)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 2), &mut roms, &mut cands);
        assert_eq!(matches.len(), 2);
        let outputs: Vec<Vec<u8>> = matches.iter().map(|m| m.spec.apply(&interleaved)).collect();
        assert!(outputs.contains(&rom1.to_vec()));
        assert!(outputs.contains(&rom2.to_vec()));
        assert!(matches.iter().all(|m| m.spec.take == 2));
    }

    #[test]
    fn no_matches() {
        let orig = b"0123456789001234567890";
        let interleaved = b"abc".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn interleaved_from_1() {
        let orig = b"0123456";
        let interleaved = b"_0_1_2_3_4_5_6".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 1);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_from_0_uneven() {
        let orig = b"0123456";
        let interleaved = b"0_1_2_3_4_5_6".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_from_1_uneven() {
        let orig = b"0123456";
        let interleaved = b"_0_1_2_3_4_5_6_".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 1);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_4_from_0() {
        let orig = b"0123456";
        let interleaved = b"0_Aa1_Bb2_Cc3_Dd4_Ee5_Ff6_Gg".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&Deinterleave::new(4, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 4);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    // ---- Deinterleave with byte_swap ----

    #[test]
    fn deinterleave_swap2_finds_swapped_lane() {
        // ROM = b"abcd"; interleaved candidate has the ROM byte-swapped in lane 0
        // Lane 0 (stride=2, word_size=1): bytes at positions 0,2,4,6 of candidate
        // We want lane 0 to be swap2("abcd") = "badc"
        // So candidate positions 0,2,4,6 = 'b','a','d','c'
        // positions 1,3,5,7 = anything (say 'X')
        // candidate = "bXaXdXcX"
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let cand_data = b"bXaXdXcX".to_vec();
        let mut cands = vec![make_candidate("c", cand_data.clone())];

        let h = Deinterleave::with_opts(2, 1, ByteSwap::Swap2, false);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let extracted = matches[0].spec.apply(&cand_data);
        assert_eq!(extracted, rom_data);
    }

    // ---- Deinterleave with rotate ----

    #[test]
    fn deinterleave_rotate_finds_rotated_lane() {
        // ROM = b"ABCD"; lane 0 of candidate contains ROM rotated left by 2 = "CDAB"
        // candidate with weave=2, word_size=1: positions 0,2,4,6 = C,D,A,B
        // positions 1,3,5,7 = X
        // candidate = "CXDXAXBX"
        let rom_data = b"ABCD";
        let mut roms = vec![make_rom("r", rom_data)];
        let cand_data = b"CXDXAXBX".to_vec();
        let mut cands = vec![make_candidate("c", cand_data.clone())];

        let h = Deinterleave::with_opts(2, 1, ByteSwap::None, true);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let extracted = matches[0].spec.apply(&cand_data);
        assert_eq!(extracted, rom_data);
    }
}
