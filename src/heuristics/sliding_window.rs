use crate::crc32_window;
use crate::extraction::swap_bytes;
use crate::types::{Bucket, ByteSwap, Candidate, ExtractionSpec, Found, Heuristic, MatchedData};

pub struct SlidingWindow {
    pub byte_swap: ByteSwap,
    name: String,
}

impl SlidingWindow {
    pub fn new(byte_swap: ByteSwap) -> Self {
        let name = match byte_swap {
            ByteSwap::None => "SlidingWindow".to_string(),
            ByteSwap::Swap2 => "SlidingWindow(swap2)".to_string(),
            ByteSwap::Swap4 => "SlidingWindow(swap4)".to_string(),
        };
        SlidingWindow { byte_swap, name }
    }

    fn granularity(&self) -> usize {
        match self.byte_swap {
            ByteSwap::None => 1,
            ByteSwap::Swap2 => 2,
            ByteSwap::Swap4 => 4,
        }
    }
}

fn found_from_skip(skip: usize, size: usize, crc: u32, swap: ByteSwap) -> Found {
    Found {
            size,
            crc,
            data: MatchedData::Spec(ExtractionSpec {
                skip,
                step_by: 1,
                take: 1,
                size,
                byte_swap: swap,
                ..Default::default()
            }),
        }
}

impl Default for SlidingWindow {
    fn default() -> Self {
        Self::new(ByteSwap::None)
    }
}

impl Heuristic for SlidingWindow {
    fn name(&self) -> &str {
        &self.name
    }

    fn estimate_cost(&self, _size: usize, bucket: &Bucket, cands: &[Candidate]) -> Option<u64> {
        if bucket.map.is_empty() || cands.is_empty() {
            return None;
        }
        let win = bucket.size;
        let gran = self.granularity();
        if !win.is_multiple_of(gran) {
            return None;
        }
        if self.byte_swap == ByteSwap::None {
            // Rolling CRC fast path: O(n) per candidate
            let work: u64 = cands
                .iter()
                .filter(|c| c.data.len() >= win)
                .map(|c| c.data.len().saturating_sub(win).saturating_add(1) as u64)
                .sum();
            if work > 0 { Some(work) } else { None }
        } else {
            // Per-window path: O(n * size) per candidate
            let work: u64 = cands
                .iter()
                .filter(|c| c.data.len() >= win && c.data.len().is_multiple_of(gran))
                .map(|c| {
                    let windows = c.data.len().saturating_sub(win).saturating_add(1) as u64;
                    windows.saturating_mul(win as u64)
                })
                .sum();
            if work > 0 { Some(work) } else { None }
        }
    }

    fn probe_cand<'a>(
        &'a self,
        cand: &'a Candidate,
        bucket: &'a Bucket,
    ) -> Box<dyn Iterator<Item = Found> + 'a> {
        let size = bucket.size;
        let gran = self.granularity();

        match self.byte_swap {
            ByteSwap::None => {
                if cand.data.len() < size {
                    return Box::new(std::iter::empty());
                }
                let targets: Vec<u32> = bucket.map.keys().copied().collect();
                let hits =
                    crc32_window::find_windows_crc_rolling_any(&cand.data, size, &targets);
                Box::new(hits.into_iter().map(move |(skip, crc)| Found {
                    size,
                    crc,
                    data: MatchedData::Spec(ExtractionSpec {
                        size,
                        skip,
                        ..Default::default()
                    }),
                }))
            }
            swap => {
                if !size.is_multiple_of(gran)
                    || !cand.data.len().is_multiple_of(gran)
                    || cand.data.len() < size
                {
                    return Box::new(std::iter::empty());
                }
                let targets: Vec<u32> = bucket.map.keys().copied().collect();

                // Build pre_even: swap pairs from position 0 (correct for even skip)
                let mut pre_even = cand.data.clone();
                swap_bytes(&mut pre_even, swap);

                // Build pre_odd: swap pairs from position 1 (correct for odd skip)
                // byte 0 is untouched; pairs are [1,2],[3,4],...
                let mut pre_odd = cand.data.clone();
                swap_bytes(&mut pre_odd[1..], swap);

                let mut results: Vec<Found> = Vec::new();
                for (skip, crc) in crc32_window::find_windows_crc_rolling_any(&pre_even,
                                                                              size, &targets) {
                    if skip % gran == 0 {
                        results.push(found_from_skip(skip, size, crc, swap));
                    }
                }
                for (skip, crc) in crc32_window::find_windows_crc_rolling_any(&pre_odd,
                                                                              size, &targets) {
                    if skip % gran != 0 {
                        results.push(found_from_skip(skip, size, crc, swap));
                    }
                }
                Box::new(results.into_iter())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_candidate, make_rom, run_heuristic};

    #[test]
    fn sliding_window_finds_embedded() {
        let rom_data = b"TARGET";
        let mut roms = vec![make_rom("t", rom_data)];
        let mut cands = vec![make_candidate("cand", b"XXXXTARGETYYYY".to_vec())];

        let matches = run_heuristic(&SlidingWindow::default(), &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        assert!(roms[0].matched);
        let mr = &matches[0];
        assert_eq!(mr.spec.step_by, 1);
        assert_eq!(mr.spec.skip, 4);
        assert_eq!(mr.spec.rotate_left, 0);
    }

    // ---- SlidingWindow with byte_swap (absorbed from ByteSwapSlidingWindow) ----

    #[test]
    fn window_swap2_finds_embedded() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut data: Vec<u8> = b"XXXX".to_vec();
        data.extend_from_slice(b"badc");
        data.extend_from_slice(b"ZZZZ");
        let mut cands = vec![make_candidate("c", data.clone())];

        let h = SlidingWindow::new(ByteSwap::Swap2);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let spec = &matches[0].spec;
        assert_eq!(spec.skip, 4);
        let extracted = spec.apply(&data);
        assert_eq!(extracted, rom_data);
    }

    #[test]
    fn window_swap4_finds_embedded() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut data: Vec<u8> = b"XXXX".to_vec();
        data.extend_from_slice(b"dcba");
        data.extend_from_slice(b"YYYY");
        let mut cands = vec![make_candidate("c", data.clone())];

        let h = SlidingWindow::new(ByteSwap::Swap4);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert_eq!(matches.len(), 1);
        let spec = &matches[0].spec;
        assert_eq!(spec.skip, 4);
        let extracted = spec.apply(&data);
        assert_eq!(extracted, rom_data);
    }

    #[test]
    fn window_no_match_if_unswapped() {
        let rom_data = b"abcd";
        let mut roms = vec![make_rom("r", rom_data)];
        let mut data: Vec<u8> = b"XXXX".to_vec();
        data.extend_from_slice(b"abcd");
        data.extend_from_slice(b"ZZZZ");
        let mut cands = vec![make_candidate("c", data)];

        let h = SlidingWindow::new(ByteSwap::Swap2);
        let matches = run_heuristic(&h, &mut roms, &mut cands);
        assert!(matches.is_empty());
    }
}
