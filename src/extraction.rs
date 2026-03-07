use crate::types::{ByteSwap, ExtractionSpec};

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
    pub fn apply(&self, data: &[u8]) -> Vec<u8> {
        // 1. Skip + step_by
        let mut v: Vec<u8> = if self.skip >= data.len() {
            Vec::new()
        } else if self.step_by == 1 {
            let end = usize::min(data.len(), self.skip + self.size);
            data[self.skip..end].to_vec()
        } else {
            let mut out = Vec::with_capacity(self.size);
            let mut idx = self.skip;
            let mut remaining = self.size;
            while idx < data.len() && remaining > 0 {
                let end = usize::min(idx + self.take, data.len());
                let take = usize::min(end - idx, remaining);
                out.extend_from_slice(&data[idx..idx + take]);
                idx += self.step_by;
                remaining -= take;
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
        v
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
        assert_eq!(spec.apply(b"abcd"), b"badc");
    }

    #[test]
    fn swap4_applies_correctly() {
        let spec = spec_with_swap(4, ByteSwap::Swap4);
        assert_eq!(spec.apply(b"abcd"), b"dcba");
    }

    #[test]
    fn swap2_multi_block() {
        let spec = spec_with_swap(8, ByteSwap::Swap2);
        assert_eq!(spec.apply(b"abcdefgh"), b"badcfehg");
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
        assert_eq!(spec.apply(b"abcdef"), b"dcfeba");
    }

    #[test]
    fn swap2_truncates_odd_byte() {
        // size=4, data=b"abcde" — only first 4 bytes extracted, then Swap2
        let spec = spec_with_swap(4, ByteSwap::Swap2);
        assert_eq!(spec.apply(b"abcde"), b"badc");
    }
}
