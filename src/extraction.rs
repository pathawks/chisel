use crate::types::{ByteSwap, ExtractionSpec};
use anyhow::anyhow;

pub(crate) fn swap_bytes(data: &mut [u8], swap: ByteSwap) {
    match swap {
        ByteSwap::None => {}
        ByteSwap::Swap2 => {
            for c in data.chunks_exact_mut(2) {
                c.swap(0, 1);
            }
        }
        ByteSwap::Swap4 => {
            for c in data.chunks_exact_mut(4) {
                c.reverse();
            }
        }
    }
}

impl ExtractionSpec {
    /// Applies this extraction to `data`.
    ///
    /// Returns an error for invalid stepping parameters or when calculating an
    /// extraction range would overflow `usize`.
    pub fn apply(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if self.step_by == 0 {
            return Err(anyhow!("extraction step_by must be at least 1"));
        }
        if self.take == 0 {
            return Err(anyhow!("extraction take must be at least 1"));
        }
        if self.take > self.step_by {
            return Err(anyhow!(
                "extraction take ({}) must not exceed step_by ({})",
                self.take,
                self.step_by
            ));
        }

        // 1. Skip + step_by
        let mut v: Vec<u8> = if self.skip >= data.len() {
            Vec::new()
        } else if self.step_by == 1 {
            let requested_end = self.skip.checked_add(self.size).ok_or_else(|| {
                anyhow!(
                    "extraction range overflows usize: skip {} + size {}",
                    self.skip,
                    self.size
                )
            })?;
            let end = usize::min(data.len(), requested_end);
            data[self.skip..end].to_vec()
        } else {
            let available = data.len().saturating_sub(self.skip);
            let mut out = Vec::with_capacity(self.size.min(available));
            let mut idx = self.skip;
            let mut remaining = self.size;
            while idx < data.len() && remaining > 0 {
                let take = self.take.min(remaining);
                let requested_end = idx.checked_add(take).ok_or_else(|| {
                    anyhow!("extraction chunk range overflows usize: offset {idx} + length {take}")
                })?;
                let end = usize::min(requested_end, data.len());
                let take = end - idx;
                out.extend_from_slice(&data[idx..end]);
                remaining -= take;
                if remaining == 0 || end == data.len() {
                    break;
                }
                idx = idx.checked_add(self.step_by).ok_or_else(|| {
                    anyhow!(
                        "extraction offset overflows usize: offset {idx} + step_by {}",
                        self.step_by
                    )
                })?;
            }
            out
        };
        if v.len() > self.size {
            v.truncate(self.size);
        }
        // 2. Rotate
        if self.rotate_left != 0 && !v.is_empty() {
            let r = self.rotate_left % v.len();
            if r != 0 {
                v.rotate_left(r);
            }
        }
        // 3. Byte swap
        match self.byte_swap {
            ByteSwap::None => {}
            ByteSwap::Swap2 => {
                for chunk in v.chunks_exact_mut(2) {
                    chunk.swap(0, 1);
                }
            }
            ByteSwap::Swap4 => {
                for chunk in v.chunks_exact_mut(4) {
                    chunk.reverse();
                }
            }
        }
        Ok(v)
    }

    pub fn name(&self) -> String {
        let mut name = format!(
            "skip{}_step{}_take{}_size{}",
            self.skip, self.step_by, self.take, self.size
        );
        if self.rotate_left != 0 {
            name.push_str(&format!("_rot{}", self.rotate_left));
        }
        match self.byte_swap {
            ByteSwap::None => {}
            ByteSwap::Swap2 => name.push_str("_swap2"),
            ByteSwap::Swap4 => name.push_str("_swap4"),
        }
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_includes_take() {
        let spec = ExtractionSpec {
            skip: 3,
            step_by: 4,
            take: 2,
            size: 8,
            rotate_left: 0,
            byte_swap: ByteSwap::None,
        };
        let n = spec.name();
        assert!(n.contains("take2"));
        assert!(n.contains("step4"));
    }

    fn spec_with_swap(size: usize, swap: ByteSwap) -> ExtractionSpec {
        ExtractionSpec {
            size,
            byte_swap: swap,
            ..Default::default()
        }
    }

    #[test]
    fn swap2_applies_correctly() {
        let spec = spec_with_swap(4, ByteSwap::Swap2);
        assert_eq!(spec.apply(b"abcd").unwrap(), b"badc");
    }

    #[test]
    fn swap4_applies_correctly() {
        let spec = spec_with_swap(4, ByteSwap::Swap4);
        assert_eq!(spec.apply(b"abcd").unwrap(), b"dcba");
    }

    #[test]
    fn swap2_multi_block() {
        let spec = spec_with_swap(8, ByteSwap::Swap2);
        assert_eq!(spec.apply(b"abcdefgh").unwrap(), b"badcfehg");
    }

    #[test]
    fn rotate_then_swap2() {
        // rotate_left=2 on b"abcdef" -> b"cdefab", then Swap2 -> b"dcfeba"
        let spec = ExtractionSpec {
            size: 6,
            rotate_left: 2,
            byte_swap: ByteSwap::Swap2,
            ..Default::default()
        };
        assert_eq!(spec.apply(b"abcdef").unwrap(), b"dcfeba");
    }

    #[test]
    fn swap2_truncates_odd_byte() {
        // size=4, data=b"abcde" — only first 4 bytes extracted, then Swap2
        let spec = spec_with_swap(4, ByteSwap::Swap2);
        assert_eq!(spec.apply(b"abcde").unwrap(), b"badc");
    }

    #[test]
    fn contiguous_range_overflow_has_context() {
        let spec = ExtractionSpec {
            skip: 1,
            size: usize::MAX,
            ..Default::default()
        };

        let error = spec.apply(b"data").unwrap_err();
        assert!(error.to_string().contains("skip 1 + size"));
    }

    #[test]
    fn stepped_offset_overflow_has_context() {
        let spec = ExtractionSpec {
            skip: 1,
            step_by: usize::MAX,
            take: 1,
            size: 2,
            ..Default::default()
        };

        let error = spec.apply(b"abcd").unwrap_err();
        assert!(error.to_string().contains("offset 1 + step_by"));
    }

    #[test]
    fn oversized_requested_size_does_not_overallocate() {
        let spec = ExtractionSpec {
            size: usize::MAX,
            ..Default::default()
        };

        assert_eq!(spec.apply(b"data").unwrap(), b"data");
    }

    #[test]
    fn display_handles_zero_size_and_uses_size_for_grouping() {
        let zero_size = ExtractionSpec {
            skip: 1,
            step_by: 2,
            size: 0,
            ..Default::default()
        };
        assert!(zero_size.to_string().contains("skip 1 byte"));

        let grouped = ExtractionSpec {
            skip: 8,
            step_by: 2,
            size: 4,
            ..Default::default()
        };
        assert!(grouped.to_string().contains("skip 2*4 bytes"));

        let ungrouped = ExtractionSpec { skip: 6, ..grouped };
        assert!(ungrouped.to_string().contains("skip 6 bytes"));
    }
}
