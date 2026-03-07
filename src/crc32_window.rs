//! Self-contained CRC-32 (IEEE, reflected) helpers for windowed search.
//!
//! - `window_crc_prefix_combine(...)`: O(1) per window using prefix CRCs + x^(8n) shift
//! - `window_crc_rolling(...)`:      true rolling update (remove/shift/add)
//!
//! No dependencies. Safe Rust. Tested below.

// ===== CRC-32 (IEEE, reflected) basics =====
const POLY_REFLECTED: u32 = 0xEDB8_8320; // 0x04C11DB7 reflected
const INIT: u32 = 0xFFFF_FFFF;
const XOROUT: u32 = 0xFFFF_FFFF;

#[inline]
fn update_byte(state: u32, b: u8) -> u32 {
    (state >> 8) ^ TABLE[((state ^ (b as u32)) & 0xFF) as usize]
}

/// Final, public CRC-32 (IEEE): xorout(state).
#[inline]
fn finalize(state: u32) -> u32 {
    state ^ XOROUT
}

// Precompute the standard 256-entry table once
const fn table_entry(mut c: u32) -> u32 {
    let mut j = 0;
    while j < 8 {
        c = if (c & 1) != 0 {
            (c >> 1) ^ POLY_REFLECTED
        } else {
            c >> 1
        };
        j += 1;
    }
    c
}
static TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = table_entry(i as u32);
        i += 1;
    }
    t
};

// ===== GF(2) matrix helpers for x^(8*n) shift on *final* CRCs =====
//
// We implement the classic “combine” machinery:
//     combine(c1, c2, len) = shift_bytes(c1, len) ^ c2
// where all CRC values are the *final* (xorout-applied) representation.

#[inline]
fn gf2_times(mat: &[u32; 32], mut vec: u32) -> u32 {
    // Multiply 32x32 binary matrix by 32-bit vector (over GF(2))
    let mut sum = 0u32;
    let mut i = 0;
    while vec != 0 {
        if (vec & 1) != 0 {
            sum ^= mat[i];
        }
        vec >>= 1;
        i += 1;
    }
    sum
}

#[inline]
fn gf2_square(out: &mut [u32; 32], mat: &[u32; 32]) {
    // out = mat^2
    let mut i = 0;
    while i < 32 {
        out[i] = gf2_times(mat, mat[i]);
        i += 1;
    }
}

/// Build operator for advancing a *final* CRC by one byte of zeros (i.e., x^8).
fn one_byte_advance_op() -> [u32; 32] {
    // Start from the operator for one zero *bit*, then square up to 8 bits.
    let mut odd = std::array::from_fn(|i| {
        if i == 0 {
            POLY_REFLECTED
        } else {
            1u32 << (i - 1)
        }
    });
    // Square to 2, 4, then 8 bits
    let mut even = [0u32; 32]; // 2 bits
    gf2_square(&mut even, &odd);
    gf2_square(&mut odd, &even); // 4
    gf2_square(&mut even, &odd); // 8
    even
}

/// Return `shift_bytes(crc, n)`, i.e. multiply the *final* CRC by x^(8n) (append n zero bytes).
pub fn shift_bytes(mut crc_final: u32, mut n: usize) -> u32 {
    if n == 0 {
        return crc_final;
    }
    // Operator for one byte:
    let mut op = one_byte_advance_op(); // x^8
    // Exponentiate: op^(n)
    while n != 0 {
        if (n & 1) != 0 {
            crc_final = gf2_times(&op, crc_final);
        }
        n >>= 1;
        if n != 0 {
            let mut squared = [0u32; 32];
            gf2_square(&mut squared, &op);
            op = squared; // square doubles the byte-count (x^(8*2^k))
        }
    }
    crc_final
}

/// Append a single byte `b` to a message whose *final* CRC is `crc_final`.
#[inline]
pub fn append_byte_from_final(crc_final: u32, b: u8) -> u32 {
    let state = crc_final ^ XOROUT;
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("crc") {
            // Safety: feature detected
            unsafe {
                use core::arch::aarch64::__crc32b;
                return __crc32b(state, b) ^ XOROUT;
            }
        }
    }
    // fallback
    finalize(update_byte(state, b))
}

// ===== Prefix + combine window CRC (recommended) =====

/// Compute all window CRCs of length `win` using prefix+combine. Returns indices where CRC == `target`.
pub fn find_windows_crc_prefix_combine(buf: &[u8], win: usize, target_crc: u32) -> Vec<usize> {
    assert!(win <= buf.len());
    // Build prefix final CRCs: P[i] = CRC(buf[0..i])
    let mut pref = Vec::with_capacity(buf.len() + 1);
    pref.push(0u32); // CRC("") = 0 for this variant
    let mut state = INIT;
    for &b in buf {
        state = update_byte(state, b);
        pref.push(finalize(state));
    }

    // Precompute x^(8*win) once
    let op_n = advance_op_pow_bytes(win);

    pref.iter()
        .zip(pref.iter().skip(win)) // (P[i], P[i+win])
        .enumerate()
        .filter_map(|(i, (&p_i, &p_iw))| {
            let w_crc = p_iw ^ gf2_times(&op_n, p_i);
            (w_crc == target_crc).then_some(i)
        })
        .collect()
}

// ===== True rolling window CRC (remove + append) =====
//
// We precompute a 256-entry "outgoing contribution" table for this window length:
//     out_tab[b] = CRC( [b] + [0; win-1] )
// Then for each slide:
//     crc ^= out_tab[out_byte]
//     crc  = shift_bytes(crc, 1)
//     crc  = append_byte_from_final(crc, in_byte)

/// Build the removal table for this specific window length.
fn build_out_table(win: usize) -> [u32; 256] {
    // Operator for shifting FINAL CRCs by (win-1) bytes; identity when no shift.
    let op = if win > 1 {
        advance_op_pow_bytes(win - 1)
    } else {
        identity_mat()
    };

    // Fill the 256-entry table without per-item branching.
    std::array::from_fn(|i| {
        // CRC of a single byte without allocating a [u8; 1]
        let c = finalize(update_byte(INIT, i as u8));
        gf2_times(&op, c)
    })
}

// ===== Convenience: single-shot window CRC at position i (prefix+combine) =====
pub fn window_crc_at(buf: &[u8], i: usize, win: usize) -> u32 {
    assert!(i + win <= buf.len());
    // P[i+win] ^ shift(P[i])
    let mut state = INIT;
    for &b in &buf[..i] {
        state = update_byte(state, b);
    }
    let p_i = finalize(state);
    for &b in &buf[i..i + win] {
        state = update_byte(state, b);
    }
    let p_i_win = finalize(state);
    p_i_win ^ shift_bytes(p_i, win)
}

// ---- Matrix helpers to precompute x^(8n) once and reuse ----
#[inline]
fn identity_mat() -> [u32; 32] {
    std::array::from_fn(|i| 1 << i)
}

#[inline]
fn gf2_matmul(a: &[u32; 32], b: &[u32; 32]) -> [u32; 32] {
    std::array::from_fn(|i| {
        gf2_times(a, b[i]) // a ∘ b on basis vector i
    })
}

/// Precompute the operator for multiplying FINAL CRCs by x^(8*win) once.
fn advance_op_pow_bytes(win: usize) -> [u32; 32] {
    if win == 0 {
        return identity_mat();
    }
    // op = x^8 (one byte)
    let mut op = one_byte_advance_op();
    // res = identity
    let mut res = identity_mat();
    let mut n = win;
    loop {
        if (n & 1) != 0 {
            res = gf2_matmul(&op, &res); // res = op ∘ res
        }
        n >>= 1;
        if n == 0 {
            break;
        }
        let mut squared = [0u32; 32];
        gf2_square(&mut squared, &op); // op = op^2
        op = squared;
    }
    res
}

#[inline]
fn rolling_core<'a, N>(
    steps: usize,
    init_crc: u32,
    mut next_crc: N, // (i, crc) -> next_crc
) -> impl Iterator<Item = (usize, u32)> + 'a
where
    N: FnMut(usize, u32) -> u32 + 'a,
{
    use std::iter::once;

    once((0usize, init_crc)).chain((0..steps).scan(init_crc, move |crc, i| {
        *crc = next_crc(i, *crc);
        Some((i + 1, *crc))
    }))
}

/// Rolling scan: O(1) per shift. Returns indices where window CRC == `target_crc`.
pub fn find_windows_crc_rolling(data: &[u8], win: usize, target_crc: u32) -> Vec<usize> {
    if win == 0 || win > data.len() {
        return Vec::new();
    }
    let len = data.len();
    let out_tab = build_out_table(win);
    let init = crc32fast::hash(&data[..win]);
    let slide_window = |i, crc| {
        let old = data[i];
        let newb = data[i + win];
        append_byte_from_final(crc ^ out_tab[old as usize], newb)
    };

    rolling_core(len - win, init, slide_window)
        .filter_map(move |(step, crc)| (crc == target_crc).then_some(step))
        .collect()
}

/// Rolling scan (remove + append): returns (offset, matched_crc) for any target in `targets`.
pub fn find_windows_crc_rolling_any(data: &[u8], win: usize, targets: &[u32]) -> Vec<(usize, u32)> {
    if win == 0 || win > data.len() || targets.is_empty() {
        return Vec::new();
    }
    let len = data.len();
    let out_tab = build_out_table(win);
    let init = crc32fast::hash(&data[..win]);

    let slide = move |i: usize, crc: u32| {
        let old = data[i];
        let newb = data[i + win];
        append_byte_from_final(crc ^ out_tab[old as usize], newb)
    };

    // factory so each match arm gets a fresh iterator
    let make = || rolling_core(len - win, init, slide);

    match *targets {
        [t] => make()
            .filter_map(move |(step, crc)| (crc == t).then_some((step, crc)))
            .collect(),
        [t1, t2] => make()
            .filter_map(move |(step, crc)| (crc == t1 || crc == t2).then_some((step, crc)))
            .collect(),
        _ => {
            let set: std::collections::HashSet<u32, nohash_hasher::BuildNoHashHasher<u32>> =
                targets.iter().copied().collect();
            make()
                .filter_map(move |(step, crc)| set.contains(&crc).then_some((step, crc)))
                .collect()
        }
    }
}

pub fn find_rotate_crc_rolling_any(buf: &[u8], targets: &[u32]) -> Vec<(usize, u32)> {
    if buf.is_empty() || targets.is_empty() {
        return Vec::new();
    }
    let len = buf.len();
    let out_tab = build_out_table(len);
    let init = crc32fast::hash(buf);

    let rotate = move |i: usize, crc: u32| {
        let b = buf[i];
        append_byte_from_final(crc ^ out_tab[b as usize], b)
    };
    let to_offset = |step: usize| (len - step) % len;

    let make = || rolling_core(len - 1, init, rotate);

    match *targets {
        [t] => make()
            .filter_map(move |(step, crc)| (crc == t).then_some((to_offset(step), crc)))
            .collect(),
        [t1, t2] => make()
            .filter_map(move |(step, crc)| {
                (crc == t1 || crc == t2).then_some((to_offset(step), crc))
            })
            .collect(),
        _ => {
            let set: std::collections::HashSet<u32, nohash_hasher::BuildNoHashHasher<u32>> =
                targets.iter().copied().collect();
            make()
                .filter_map(move |(step, crc)| set.contains(&crc).then_some((to_offset(step), crc)))
                .collect()
        }
    }
}

// ===== Tests =====
#[cfg(test)]
mod tests {
    use super::*;

    // ---- helper ----

    /// Compute final CRC-32 (IEEE) of `data`.
    /// For testing the basics of our implementation
    pub fn crc32_ieee(data: &[u8]) -> u32 {
        /// Internal state update (no xorout). Start with INIT.
        #[inline]
        fn update_state(mut state: u32, data: &[u8]) -> u32 {
            for &b in data {
                state = update_byte(state, b);
            }
            state
        }
        finalize(update_state(INIT, data))
    }

    /// Prefix+combine scan: returns (offset, matched_crc) for any target in `targets`.
    pub fn find_windows_crc_prefix_combine_any(
        buf: &[u8],
        win: usize,
        targets: &[u32],
    ) -> Vec<(usize, u32)> {
        assert!(win <= buf.len());
        if targets.is_empty() {
            return Vec::new();
        }

        let set: std::collections::HashSet<u32, nohash_hasher::BuildNoHashHasher<u32>> =
            targets.iter().copied().collect();

        // Build prefix final CRCs: P[i] = CRC(buf[0..i])
        let mut pref = Vec::with_capacity(buf.len() + 1);
        pref.push(0u32); // CRC("") = 0
        let mut state = INIT;
        for &b in buf {
            state = update_byte(state, b);
            pref.push(finalize(state));
        }

        // Precompute x^(8*win) operator once, then apply via one gf2 multiply per window
        let op_n = advance_op_pow_bytes(win);

        let mut hits = Vec::new();
        for i in 0..=buf.len() - win {
            // CRC(buf[i..i+win]) = P[i+win] ^ (P[i] * x^(8*win))
            let shifted = gf2_times(&op_n, pref[i]);
            let w_crc = pref[i + win] ^ shifted;
            if set.contains(&w_crc) {
                hits.push((i, w_crc));
            }
        }
        hits
    }

    #[test]
    fn test_crc32_basics() {
        assert_eq!(crc32_ieee(b""), crc32fast::hash(b""));
        assert_eq!(crc32_ieee(b"a"), crc32fast::hash(b"a"));
        assert_eq!(crc32_ieee(b"123456789"), crc32fast::hash(b"123456789")); // standard check
    }

    #[test]
    fn test_shift_and_append() {
        let a = b"hello";
        let b = b"world";
        let ca = crc32_ieee(a);
        let cb = crc32_ieee(b);
        // combine(a, b) == shift(len(b)) of ca XOR cb
        let combined = shift_bytes(ca, b.len()) ^ cb;

        // Coerce to &[u8] so concat() works
        let parts: [&[u8]; 2] = [a, b];
        assert_eq!(combined, crc32_ieee(&parts.concat()));

        // append one byte via append_byte_from_final
        let c = append_byte_from_final(crc32_ieee(a), b'X');

        let parts2: [&[u8]; 2] = [a, b"X"];
        assert_eq!(c, crc32_ieee(&parts2.concat()));
    }

    #[test]
    fn test_windows_match_between_methods() {
        let data = b"The quick brown fox jumps over the lazy dog. The quick brown fox jumps again.";
        let win = 16usize;

        // Cross-check window CRCs across all positions
        for i in 0..=data.len() - win {
            let w = &data[i..i + win];
            let direct = crc32_ieee(w);
            let via_fn = super::window_crc_at(data, i, win);
            assert_eq!(direct, via_fn);
        }

        // Cross-check hit positions between prefix+combine and rolling
        let target = crc32_ieee(&data[10..10 + win]);
        let h1 = find_windows_crc_prefix_combine(data, win, target);
        let h2 = find_windows_crc_rolling(data, win, target);
        assert_eq!(h1, h2);
        assert!(h1.contains(&10));
    }

    #[test]
    fn test_any_targets_match() {
        let data = b"The quick brown fox jumps over the lazy dog.";
        let win = 9;

        // Pick two windows as targets
        let t1 = crc32_ieee(&data[4..4 + win]); // "quick bro"
        let t2 = crc32_ieee(&data[16..16 + win]); // "fox jumps"

        let targets = [t1, t2];

        let a = find_windows_crc_prefix_combine_any(data, win, &targets);
        let b = find_windows_crc_rolling_any(data, win, &targets);

        assert!(a.contains(&(4, t1)));
        assert!(a.contains(&(16, t2)));
        assert_eq!(a, b);
    }

    #[test]
    fn rolling_step_equivalence() {
        let data = b"The quick brown fox jumps over the lazy dog.";
        let win = 16;
        let out = build_out_table(win);
        let direct = crc32_ieee(&data[1..1 + win]);

        let mut rolled = crc32_ieee(&data[..win]);
        rolled ^= out[data[0] as usize];
        rolled = append_byte_from_final(rolled, data[win]);

        assert_eq!(direct, rolled);
    }

    #[test]
    fn out_table_sanity() {
        let win = 9;
        let out = build_out_table(win);
        for b in 0..=255u8 {
            assert_eq!(out[b as usize], shift_bytes(crc32_ieee(&[b]), win - 1));
        }
    }

    #[test]
    fn test_rotate_identity() {
        let data = b"The quick brown fox jumps over the lazy dog.";

        let results = find_rotate_crc_rolling_any(data, &[crc32fast::hash(data)]);

        assert_eq!(results.len(), 1);
        assert_eq!(results, [(0, crc32fast::hash(data))]);
    }
}

// ===== Example usage =====
//
// use crc32_window::{find_windows_crc_prefix_combine, find_windows_crc_rolling, crc32_ieee};
//
// fn main() {
//     let haystack = std::fs::read("bigfile.bin").unwrap();
//     let needle_len = 4096usize;
//     let needle_crc = 0xDEADBEEF; // <-- the CRC you're searching for
//
//     // Fastest & simplest:
//     let hits = crc32_window::find_windows_crc_prefix_combine(&haystack, needle_len, needle_crc);
//     for i in hits { println!("hit at offset {i}"); }
//
//     // True rolling variant (same results, different mechanics):
//     let hits2 = crc32_window::find_windows_crc_rolling(&haystack, needle_len, needle_crc);
//     assert_eq!(hits, hits2);
// }
