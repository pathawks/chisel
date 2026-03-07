use crate::crc32_window;
use crate::extraction::swap_bytes;
use crate::types::{Bucket, ByteSwap, Candidate, ExtractionSpec, Found, Heuristic, MatchedData};

pub struct DeinterleaveSplit {
    weave: usize,
    word_size: usize,
    pub byte_swap: ByteSwap,
    name: String,
}

impl DeinterleaveSplit {
    pub fn new(weave: usize, word_size: usize) -> Self {
        Self::with_opts(weave, word_size, ByteSwap::None)
    }

    pub fn with_opts(weave: usize, word_size: usize, byte_swap: ByteSwap) -> Self {
        let bs = match byte_swap {
            ByteSwap::None => String::new(),
            ByteSwap::Swap2 => ",swap2".to_string(),
            ByteSwap::Swap4 => ",swap4".to_string(),
        };
        let name_str = if word_size == 1 {
            format!("DeinterleaveSplit({}{})", weave, bs)
        } else {
            format!("DeinterleaveSplit({},{}{})", weave, word_size, bs)
        };
        DeinterleaveSplit { weave, word_size, byte_swap, name: name_str }
    }

    fn granularity(&self) -> usize {
        match self.byte_swap {
            ByteSwap::None => 1,
            ByteSwap::Swap2 => 2,
            ByteSwap::Swap4 => 4,
        }
    }
}

impl Heuristic for DeinterleaveSplit {
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

        let mut work: u64 = 0;
        let stride = self.weave * self.word_size;
        for cand in cands {
            let n = cand.data.len();
            for lane in 0..self.weave {
                let skip_bytes = lane * self.word_size;
                if skip_bytes >= n {
                    break;
                }
                let lane_len_words = (n - skip_bytes).div_ceil(stride);
                let lane_len = lane_len_words * self.word_size;
                if lane_len < size {
                    continue;
                }
                let windows = lane_len.saturating_sub(size).saturating_add(1) as u64;
                const EXTRACTION_FACTOR: u64 = 4;
                work += (lane_len as u64) * EXTRACTION_FACTOR + windows;
            }
        }

        if work > 0 { Some(work) } else { None }
    }

    fn probe_cand<'a>(
        &'a self,
        cand: &'a Candidate,
        bucket: &'a Bucket,
    ) -> Box<dyn Iterator<Item = Found> + 'a> {
        let mut out = Vec::new();
        let stride = self.weave * self.word_size;
        let gran = self.granularity();

        for lane in 0..self.weave {
            let skip_bytes = lane * self.word_size;
            if skip_bytes >= cand.data.len() {
                break;
            }
            let lane_len_words = (cand.data.len() - skip_bytes).div_ceil(stride);
            let lane_len = lane_len_words * self.word_size;
            if lane_len < bucket.size {
                continue;
            }

            // Extract lane into a contiguous buffer
            let mut buf = Vec::with_capacity(lane_len);
            let mut idx = skip_bytes;
            while idx < cand.data.len() && buf.len() < lane_len {
                let end = usize::min(idx + self.word_size, cand.data.len());
                buf.extend_from_slice(&cand.data[idx..end]);
                idx += stride;
            }
            if buf.len() < bucket.size {
                continue;
            }

            match self.byte_swap {
                ByteSwap::None => {
                    // Rolling CRC fast path (O(n))
                    let targets: Vec<u32> = bucket.map.keys().copied().collect();
                    for (offset, crc) in
                        crc32_window::find_windows_crc_rolling_any(&buf, bucket.size, &targets)
                    {
                        out.push(Found {
                            size: bucket.size,
                            crc,
                            data: MatchedData::Spec(ExtractionSpec {
                                skip: (offset * stride) + skip_bytes,
                                step_by: stride,
                                take: self.word_size,
                                size: bucket.size,
                                ..Default::default()
                            }),
                        });
                    }
                }
                swap => {
                    if !bucket.size.is_multiple_of(gran) { continue; }
                    let mut swapped_buf = buf.clone();      // O(lane_len) once
                    swap_bytes(&mut swapped_buf, swap);

                    let targets: Vec<u32> = bucket.map.keys().copied().collect();
                    let hits = crc32_window::find_windows_crc_rolling_any(
                        &swapped_buf, bucket.size, &targets
                    );
                    for (offset, crc) in hits {
                        // Only keep word-aligned positions (pre-swap is only valid there)
                        if offset % self.word_size != 0 { continue; }
                        let word_idx = offset / self.word_size;
                        out.push(Found {
                            size: bucket.size,
                            crc,
                            data: MatchedData::Spec(ExtractionSpec {
                                skip: word_idx * stride + skip_bytes,
                                step_by: stride,
                                take: self.word_size,
                                size: bucket.size,
                                byte_swap: swap,
                                ..Default::default()
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
        let orig = b"234";
        let interleaved = b"0_1_2_3_4_5_6_".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 4);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn two_files_interleaved_from_0() {
        let orig = b"234";
        let interleaved = b"0a1b2c3d4e5f6g".to_vec();
        let mut roms = vec![make_rom("r1", orig), make_rom("r2", b"bcdef")];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 2);
        assert!(roms[0].matched);
        let mr = matches
            .iter()
            .find(|m| m.spec.skip == 4)
            .expect("expected match with skip 4");
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 4);
        assert_eq!(mr.spec.apply(&interleaved), orig);
        let mr = matches
            .iter()
            .find(|m| m.spec.skip == 3)
            .expect("expected match with skip 3");
        assert!(roms[1].matched);
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 3);
        assert_eq!(mr.spec.apply(&interleaved), b"bcdef");
    }

    #[test]
    fn interleaved_from_1() {
        let orig = b"234";
        let interleaved = b"_0_1_2_3_4_5_6".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 5);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_from_0_uneven() {
        let orig = b"234";
        let interleaved = b"0_1_2_3_4_5_6".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 4);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_from_1_uneven() {
        let orig = b"2345";
        let interleaved = b"_0_1_2_3_4_5_6_".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(2, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 2);
        assert_eq!(mr.spec.skip, 5);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_4_from_0() {
        let orig = b"01234";
        let interleaved = b"0_Aa1_Bb2_Cc3_Dd4_Ee5_Ff6_Gg".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(4, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 4);
        assert_eq!(mr.spec.skip, 0);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_4_from_2() {
        let orig = b"23456";
        let interleaved = b"0_Aa1_Bb2_Cc3_Dd4_Ee5_Ff6_Gg".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(4, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 4);
        assert_eq!(mr.spec.skip, 8);
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_is_reproduced() {
        let orig = b"23456";
        let interleaved = b"0_Aa1_Bb2_Cc3_Dd4_Ee5_Ff6_Gg".to_vec();
        let mut roms = vec![make_rom("r1", orig)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(4, 1), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.apply(&interleaved), orig);
    }

    #[test]
    fn interleaved_words_16bit() {
        let rom1 = b"abef";
        let rom2 = b"cdgh";
        let interleaved = b"abcdefgh".to_vec();
        let mut roms = vec![make_rom("r1", rom1), make_rom("r2", rom2)];
        let mut cands = vec![make_candidate("cand1", interleaved.clone())];

        let matches = run_heuristic(&DeinterleaveSplit::new(2, 2), &mut roms, &mut cands);
        assert_eq!(matches.len(), 2);
        let outputs: Vec<Vec<u8>> = matches.iter().map(|m| m.spec.apply(&interleaved)).collect();
        assert!(outputs.contains(&rom1.to_vec()));
        assert!(outputs.contains(&rom2.to_vec()));
        assert!(matches.iter().all(|m| m.spec.take == 2));
    }

    // ---- DeinterleaveSplit with byte_swap ----

    #[test]
    fn split_swap2_finds_swapped_window_in_lane() {
        // ROM = b"abcd"; the candidate has swap2(ROM) = "badc" as a window
        // starting at position 2 in lane 0 (weave=2, word_size=1).
        // Lane 0: stride=2, positions 0,2,4,6,8,... in candidate
        // Candidate positions 0,2 = padding (XX), then 4,6,8,10 = b,a,d,c
        // So candidate = "X_X_b_a_d_c__"
        //                  0123456789...
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        // candidate: lane 0 = [X, X, b, a, d, c] at positions 0,2,4,6,8,10
        // positions 1,3,5,7,9,11 = Y (lane 1)
        // total 12 bytes, positions 0..12
        let mut cand_data = vec![b'X'; 12];
        // lane 0 positions: 0,2,4,6,8,10
        // put swap2("abcd")="badc" at lane positions 2..6 (word indices 2,3,4,5)
        // lane 0 word 2 => cand position 4, word 3 => 6, word 4 => 8, word 5 => 10
        cand_data[4] = b'b';
        cand_data[6] = b'a';
        cand_data[8] = b'd';
        cand_data[10] = b'c';
        let mut cands = vec![make_candidate("c", cand_data.clone())];

        let h = DeinterleaveSplit::with_opts(2, 1, ByteSwap::Swap2);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1, "should find one match");
        let extracted = matches[0].spec.apply(&cand_data);
        assert_eq!(extracted, rom_data, "apply should recover ROM data");
    }
}
