use crate::heuristics::{Deinterleave, DeinterleaveSplit, ExactCrc, SlidingWindow};
use crate::types::{ByteSwap, Heuristic};

/// Configuration for which heuristic combinations to generate.
pub struct PipelineConfig {
    /// Which byte-swap variants to include.
    pub byte_swaps: Vec<ByteSwap>,
    /// (weave, word_size) pairs for deinterleave heuristics.
    pub deinterleaves: Vec<(usize, usize)>,
    /// Include rotation search.
    pub rotate: bool,
    /// Include sliding-window search.
    pub sliding: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            byte_swaps: vec![ByteSwap::None, ByteSwap::Swap2, ByteSwap::Swap4],
            deinterleaves: vec![(2, 1), (3, 1), (4, 1), (8, 1), (2, 2), (4, 2), (8, 2)],
            rotate: true,
            sliding: true,
        }
    }
}

impl PipelineConfig {
    /// Generate all valid heuristic combinations in cost order (cheapest first).
    ///
    /// Sliding + rotate combinations are intentionally skipped because they
    /// are O(n²) and algorithmically unsound.
    pub fn heuristics(&self) -> Vec<Box<dyn Heuristic>> {
        let mut out: Vec<Box<dyn Heuristic>> = Vec::new();

        for &bs in &self.byte_swaps {
            // ExactCrc (non-sliding, no rotate)
            out.push(Box::new(ExactCrc::new(bs, false)));

            // ExactCrc with rotate
            if self.rotate {
                out.push(Box::new(ExactCrc::new(bs, true)));
            }

            // Deinterleave (non-sliding) variants
            for &(weave, word_size) in &self.deinterleaves {
                out.push(Box::new(Deinterleave::with_opts(weave, word_size, bs, false)));
                if self.rotate {
                    out.push(Box::new(Deinterleave::with_opts(weave, word_size, bs, true)));
                }
            }

            // Sliding-window variants (rotate intentionally excluded: O(n²))
            if self.sliding {
                out.push(Box::new(SlidingWindow::new(bs)));
                for &(weave, word_size) in &self.deinterleaves {
                    out.push(Box::new(DeinterleaveSplit::with_opts(weave, word_size, bs)));
                }
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_heuristics_count() {
        let heuristics = PipelineConfig::default().heuristics();
        // Per byte_swap (3): ExactCrc + ExactCrc(rotate) + 7*Deinterleave + 7*Deinterleave(rotate)
        //                    + SlidingWindow + 7*DeinterleaveSplit
        // = 2 + 14 + 1 + 7 = 24 per byte_swap, times 3 = 72
        assert_eq!(heuristics.len(), 72);
    }

    #[test]
    fn no_sliding_rotate_combo() {
        let heuristics = PipelineConfig::default().heuristics();
        for h in &heuristics {
            let name = h.name();
            // Sliding heuristics must not have rotate in their name
            if name.starts_with("SlidingWindow") || name.starts_with("DeinterleaveSplit") {
                assert!(
                    !name.contains("rotate"),
                    "sliding heuristic should not have rotate: {}",
                    name
                );
            }
        }
    }

    #[test]
    fn rotate_only_config() {
        let cfg = PipelineConfig {
            byte_swaps: vec![ByteSwap::None],
            deinterleaves: vec![],
            rotate: true,
            sliding: false,
        };
        let h = cfg.heuristics();
        // ExactCrc(no rotate) + ExactCrc(rotate) = 2
        assert_eq!(h.len(), 2);
        assert!(h.iter().any(|x| x.name() == "ExactCrc"));
        assert!(h.iter().any(|x| x.name() == "ExactCrc(rotate)"));
    }

    #[test]
    fn sliding_only_config() {
        let cfg = PipelineConfig {
            byte_swaps: vec![ByteSwap::None],
            deinterleaves: vec![(2, 1)],
            rotate: false,
            sliding: true,
        };
        let h = cfg.heuristics();
        // ExactCrc + Deinterleave(2) + SlidingWindow + DeinterleaveSplit(2) = 4
        assert_eq!(h.len(), 4);
    }
}
