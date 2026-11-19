//! The predictors (`lossless_videodsp.c` and the loops in `magicyuv.c`),
//! run in place over rows of residuals.
//!
//! FFmpeg has x86 assembly for the 8-bit left and gradient predictors (up
//! to AVX2), for the 8-bit median (SSE2) and for the 16-bit left predictor
//! (SSSE3). Its assembly works in whole vectors and so also rewrites the
//! padding after a row; nothing visible depends on that, and the functions
//! here touch the row and nothing else.
//!
//! The portable functions are the reference. The `avx2` module has the
//! 8-bit left and gradient predictors written with AVX2 intrinsics that
//! take and return values, which safe code may call inside a function
//! compiled for AVX2.

/// `add_left_pred`: each sample becomes the sum of itself and everything to
/// its left, starting from `acc`.
pub fn left8(row: &mut [u8], mut acc: u8) {
    for d in row {
        acc = acc.wrapping_add(*d);
        *d = acc;
    }
}

/// `add_left_pred_int16`.
pub fn left16(row: &mut [u16], mask: u16, mut acc: u16) {
    for d in row {
        acc = acc.wrapping_add(*d) & mask;
        *d = acc;
    }
}

/// Gradient: each residual plus left plus top minus top-left. The first
/// sample of a row has only its top.
pub fn gradient8(row: &mut [u8], top: &[u8]) {
    let (Some(first), Some(t)) = (row.first_mut(), top.first()) else { return };
    *first = first.wrapping_add(*t);
    gradient8_from(row, top, 1);
}

/// The gradient predictor from sample `start` (at least 1) to the end.
fn gradient8_from(row: &mut [u8], top: &[u8], start: usize) {
    let mut left = row[start - 1];
    for ((d, &t), &tl) in row[start..].iter_mut().zip(&top[start..]).zip(&top[start - 1..]) {
        left = left.wrapping_add(t).wrapping_sub(tl).wrapping_add(*d);
        *d = left;
    }
}

pub fn gradient16(row: &mut [u16], top: &[u16], max: u16) {
    let (Some(first), Some(t)) = (row.first_mut(), top.first()) else { return };
    // The C code keeps the running value in an int and never masks it;
    // only its low bits reach the picture, so 16 bits are enough here.
    let mut left = first.wrapping_add(*t);
    *first = left & max;
    for ((d, &t), &tl) in row[1..].iter_mut().zip(&top[1..]).zip(top) {
        left = left.wrapping_add(t).wrapping_sub(tl).wrapping_add(*d);
        *d = left & max;
    }
}

/// `add_median_pred` as MagicYUV calls it: left and top-left both start as
/// the row's own first residual, which makes the first prediction the top
/// sample.
pub fn median8(row: &mut [u8], top: &[u8]) {
    let Some(&first) = row.first() else { return };
    median8_from(row, top, first, first);
}

/// The median predictor over `row`, given the samples to the left and
/// top-left of its first sample.
fn median8_from(row: &mut [u8], top: &[u8], mut l: u8, mut lt: u8) {
    for (d, &t) in row.iter_mut().zip(top) {
        // The median of left, top and left + top - topleft is the last of
        // them limited to the range of the first two.
        let grad = l.wrapping_add(t.wrapping_sub(lt));
        l = grad.max(l.min(t)).min(l.max(t)).wrapping_add(*d);
        lt = t;
        *d = l;
    }
}

/// `magicyuv_median_pred16`. Unlike the 8-bit version, the gradient is not
/// masked: it is an int that can be negative or more than `max`.
pub fn median16(row: &mut [u16], top: &[u16], max: u16) {
    let Some(&first) = row.first() else { return };
    let (mut l, mut lt) = (first as i32, first as i32);
    for (d, &t) in row.iter_mut().zip(top) {
        let t = t as i32;
        let grad = l + t - lt;
        l = (grad.max(l.min(t)).min(l.max(t)) + *d as i32) as u16 as i32 & max as i32;
        lt = t;
        *d = l as u16;
    }
}

/// `add_bytes`.
pub fn add8(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = d.wrapping_add(*s);
    }
}

pub fn add16(dst: &mut [u16], src: &[u16], max: u16) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = d.wrapping_add(*s) & max;
    }
}

#[cfg(target_arch = "x86_64")]
pub mod avx2 {
    use core::arch::x86_64::*;

    #[target_feature(enable = "avx2")]
    fn load(c: &[u8; 32]) -> __m256i {
        let q = |i: usize| i64::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3], c[i + 4], c[i + 5], c[i + 6], c[i + 7]]);
        _mm256_set_epi64x(q(24), q(16), q(8), q(0))
    }

    #[target_feature(enable = "avx2")]
    fn store(c: &mut [u8; 32], x: __m256i) {
        let q = [
            _mm256_extract_epi64::<0>(x),
            _mm256_extract_epi64::<1>(x),
            _mm256_extract_epi64::<2>(x),
            _mm256_extract_epi64::<3>(x),
        ];
        for (d, q) in c.as_chunks_mut::<8>().0.iter_mut().zip(q) {
            *d = q.to_le_bytes();
        }
    }

    /// Running sums of 32 bytes, on top of `carry` (the sum so far, in
    /// every byte). Returns the sums and the new carry.
    #[target_feature(enable = "avx2")]
    fn running_sums(mut x: __m256i, carry: __m256i) -> (__m256i, __m256i) {
        let last = _mm256_set1_epi8(15);
        // Within each 16-byte half: add the byte one, two, four and eight
        // places to the left.
        x = _mm256_add_epi8(x, _mm256_slli_si256::<1>(x));
        x = _mm256_add_epi8(x, _mm256_slli_si256::<2>(x));
        x = _mm256_add_epi8(x, _mm256_slli_si256::<4>(x));
        x = _mm256_add_epi8(x, _mm256_slli_si256::<8>(x));
        // The total of the low half goes onto every byte of the high half.
        let low_total = _mm256_shuffle_epi8(_mm256_permute2x128_si256::<0x08>(x, x), last);
        x = _mm256_add_epi8(_mm256_add_epi8(x, low_total), carry);
        (x, _mm256_shuffle_epi8(_mm256_permute2x128_si256::<0x11>(x, x), last))
    }

    /// [`super::left8`], 32 samples at a time.
    #[target_feature(enable = "avx2")]
    pub fn left8(row: &mut [u8], acc: u8) {
        let (chunks, tail) = row.as_chunks_mut::<32>();
        let mut carry = _mm256_set1_epi8(acc as i8);
        for c in chunks {
            let x;
            (x, carry) = running_sums(load(c), carry);
            store(c, x);
        }
        super::left8(tail, _mm256_extract_epi8::<0>(carry) as u8);
    }

    /// [`super::gradient8`], 32 samples at a time: the running sums of
    /// residual + top - topleft.
    #[target_feature(enable = "avx2")]
    pub fn gradient8(row: &mut [u8], top: &[u8]) {
        let (Some(first), Some(t)) = (row.first_mut(), top.first()) else { return };
        *first = first.wrapping_add(*t);
        let mut carry = _mm256_set1_epi8(*first as i8);
        let (chunks, _) = row[1..].as_chunks_mut::<32>();
        let done = 1 + 32 * chunks.len();
        let tops = top[1..].as_chunks::<32>().0;
        let toplefts = top.as_chunks::<32>().0;
        for ((c, t), tl) in chunks.iter_mut().zip(tops).zip(toplefts) {
            let d = _mm256_add_epi8(load(c), _mm256_sub_epi8(load(t), load(tl)));
            let x;
            (x, carry) = running_sums(d, carry);
            store(c, x);
        }
        if done < row.len() && done < top.len() {
            super::gradient8_from(row, top, done);
        }
    }

    #[target_feature(enable = "avx2")]
    fn load128(c: &[u8; 16]) -> __m128i {
        let q = |i: usize| i64::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3], c[i + 4], c[i + 5], c[i + 6], c[i + 7]]);
        _mm_set_epi64x(q(8), q(0))
    }

    /// [`super::median8`]. Each sample depends on the one before it, so
    /// this still goes one sample at a time, but in vector registers,
    /// where a byte minimum or maximum takes one step instead of a
    /// compare and a conditional move: the same way FFmpeg's SSE2
    /// assembly does it. Sixteen samples are loaded and stored at once;
    /// only byte 0 of the working registers means anything.
    #[target_feature(enable = "avx2")]
    pub fn median8(row: &mut [u8], top: &[u8]) {
        let Some(&first) = row.first() else { return };
        let mut l = _mm_cvtsi32_si128(first as i32);
        let mut lt = first;
        let (chunks, tail) = row.as_chunks_mut::<16>();
        let done = 16 * chunks.len();
        for (c, t) in chunks.iter_mut().zip(top.as_chunks::<16>().0) {
            let mut t = load128(t);
            let mut d = load128(c);
            let topleft = _mm_or_si128(_mm_slli_si128::<1>(t), _mm_cvtsi32_si128(lt as i32));
            lt = _mm_extract_epi8::<15>(t) as u8;
            let mut delta = _mm_sub_epi8(t, topleft);
            let mut out = _mm_setzero_si128();
            for _ in 0..16 {
                let grad = _mm_add_epi8(l, delta);
                let hi = _mm_max_epu8(l, t);
                let lo = _mm_min_epu8(l, t);
                l = _mm_add_epi8(_mm_max_epu8(_mm_min_epu8(hi, grad), lo), d);
                out = _mm_or_si128(_mm_srli_si128::<1>(out), _mm_slli_si128::<15>(l));
                t = _mm_srli_si128::<1>(t);
                d = _mm_srli_si128::<1>(d);
                delta = _mm_srli_si128::<1>(delta);
            }
            let q = [_mm_extract_epi64::<0>(out), _mm_extract_epi64::<1>(out)];
            for (d, q) in c.as_chunks_mut::<8>().0.iter_mut().zip(q) {
                *d = q.to_le_bytes();
            }
        }
        if let Some(top) = top.get(done..) {
            super::median8_from(tail, top, _mm_cvtsi128_si32(l) as u8, lt);
        }
    }

    #[target_feature(enable = "avx2")]
    fn load16(c: &[u16; 16]) -> __m256i {
        let q = |i: usize| (c[i] as u64 | (c[i + 1] as u64) << 16 | (c[i + 2] as u64) << 32 | (c[i + 3] as u64) << 48) as i64;
        _mm256_set_epi64x(q(12), q(8), q(4), q(0))
    }

    #[target_feature(enable = "avx2")]
    fn store16(c: &mut [u16; 16], x: __m256i) {
        let q = [
            _mm256_extract_epi64::<0>(x) as u64,
            _mm256_extract_epi64::<1>(x) as u64,
            _mm256_extract_epi64::<2>(x) as u64,
            _mm256_extract_epi64::<3>(x) as u64,
        ];
        for (d, q) in c.as_chunks_mut::<4>().0.iter_mut().zip(q) {
            *d = [q as u16, (q >> 16) as u16, (q >> 32) as u16, (q >> 48) as u16];
        }
    }

    /// [`super::left16`], 16 samples at a time.
    #[target_feature(enable = "avx2")]
    pub fn left16(row: &mut [u16], mask: u16, acc: u16) {
        let (chunks, tail) = row.as_chunks_mut::<16>();
        let last = _mm256_set1_epi16(0x0f0e);
        let maskv = _mm256_set1_epi16(mask as i16);
        // The carry is kept unmasked: only bits under the mask matter.
        let mut carry = _mm256_set1_epi16(acc as i16);
        for c in chunks {
            let mut x = load16(c);
            x = _mm256_add_epi16(x, _mm256_slli_si256::<2>(x));
            x = _mm256_add_epi16(x, _mm256_slli_si256::<4>(x));
            x = _mm256_add_epi16(x, _mm256_slli_si256::<8>(x));
            let low_total = _mm256_shuffle_epi8(_mm256_permute2x128_si256::<0x08>(x, x), last);
            x = _mm256_add_epi16(_mm256_add_epi16(x, low_total), carry);
            store16(c, _mm256_and_si256(x, maskv));
            carry = _mm256_shuffle_epi8(_mm256_permute2x128_si256::<0x11>(x, x), last);
        }
        super::left16(tail, mask, _mm256_extract_epi16::<0>(carry) as u16);
    }
}
