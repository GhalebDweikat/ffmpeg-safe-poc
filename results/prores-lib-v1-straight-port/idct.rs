//! Dequantisation, inverse DCT and clipping of one 8x8 block.
//!
//! FFmpeg has three versions of this, and they do not agree on extreme
//! input, so all three are here:
//!
//! * [`Idct::C12`]: the C code for 12-bit output (`prores_idct_12` in
//!   `proresdsp.c`, built from `simple_idct_template.c`). FFmpeg has no
//!   assembly for 12 bits.
//! * [`Idct::Asm10`]: the x86-64 SSE2/AVX assembly for 10-bit output
//!   (`x86/proresdsp.asm` and `x86/simple_idct10_template.asm`). This is
//!   what FFmpeg runs on any x86-64 machine.
//! * [`Idct::C10`]: the C code for 10-bit output (`prores_idct_10`), which
//!   FFmpeg runs when it is built without assembly.
//!
//! All arithmetic is in wrapping 32-bit integers, as in the originals.
//! Blocks are in natural order here: `block[8 * y + x]`.

/// Which of FFmpeg's inverse DCTs to reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idct {
    /// 10-bit output, the C code.
    C10,
    /// 10-bit output, the x86-64 assembly.
    Asm10,
    /// 12-bit output, the C code.
    C12,
}

impl Idct {
    /// The largest sample value the block is clipped to; the smallest is 4.
    pub fn clip_max(self) -> i16 {
        match self {
            Idct::C10 | Idct::Asm10 => (1 << 10) - 4 - 1,
            Idct::C12 => (1 << 12) - 4 - 1,
        }
    }
}

const CLIP_MIN: i16 = 4;

/// cos(i * pi / 16) * sqrt(2) * 2^14, and 2^15 for 12 bits.
const W10: [i32; 8] = [0, 22725, 21407, 19265, 16384, 12873, 8867, 4520];
const W12: [i32; 8] = [0, 45451, 42813, 38531, 32767, 25746, 17734, 9041];

/// One 8-point inverse DCT without the final shift. `bias` is added to
/// every output.
#[inline(always)]
fn idct_1d(w: &[i32; 8], x: [i32; 8], bias: i32) -> [i32; 8] {
    let m = |a: i32, b: i32| a.wrapping_mul(b);
    let e = m(w[4], x[0]).wrapping_add(bias);
    let a0 = e.wrapping_add(m(w[2], x[2])).wrapping_add(m(w[4], x[4])).wrapping_add(m(w[6], x[6]));
    let a1 = e.wrapping_add(m(w[6], x[2])).wrapping_sub(m(w[4], x[4])).wrapping_sub(m(w[2], x[6]));
    let a2 = e.wrapping_sub(m(w[6], x[2])).wrapping_sub(m(w[4], x[4])).wrapping_add(m(w[2], x[6]));
    let a3 = e.wrapping_sub(m(w[2], x[2])).wrapping_add(m(w[4], x[4])).wrapping_sub(m(w[6], x[6]));
    let b0 = m(w[1], x[1]).wrapping_add(m(w[3], x[3])).wrapping_add(m(w[5], x[5])).wrapping_add(m(w[7], x[7]));
    let b1 = m(w[3], x[1]).wrapping_sub(m(w[7], x[3])).wrapping_sub(m(w[1], x[5])).wrapping_sub(m(w[5], x[7]));
    let b2 = m(w[5], x[1]).wrapping_sub(m(w[1], x[3])).wrapping_add(m(w[7], x[5])).wrapping_add(m(w[3], x[7]));
    let b3 = m(w[7], x[1]).wrapping_sub(m(w[5], x[3])).wrapping_add(m(w[3], x[5])).wrapping_sub(m(w[1], x[7]));
    [
        a0.wrapping_add(b0),
        a1.wrapping_add(b1),
        a2.wrapping_add(b2),
        a3.wrapping_add(b3),
        a3.wrapping_sub(b3),
        a2.wrapping_sub(b2),
        a1.wrapping_sub(b1),
        a0.wrapping_sub(b0),
    ]
}

fn saturate(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// The row pass over a dequantised block, in place.
fn rows(kind: Idct, block: &mut [i16; 64]) {
    for row in block.as_chunks_mut::<8>().0 {
        let mut x = row.map(i32::from);
        match kind {
            Idct::C10 => {
                // ROW_SHIFT 13 plus the extra shift of 2.
                *row = idct_1d(&W10, x, 1 << 14).map(|v| (v >> 15) as i16);
            }
            Idct::Asm10 => {
                // The assembly rounds by adding 1 to the first coefficient,
                // in 16 bits, and packs the results with signed saturation.
                x[0] = i32::from(row[0].wrapping_add(1));
                *row = idct_1d(&W10, x, 0).map(|v| saturate(v >> 15));
            }
            Idct::C12 => {
                if row[1..] == [0; 7] {
                    // The DC-only shortcut. Not the same as the full
                    // transform, because W4 is 32767 and not 32768.
                    *row = [((x[0] + 1) >> 1) as i16; 8];
                } else {
                    *row = idct_1d(&W12, x, 1 << 15).map(|v| (v >> 16) as i16);
                }
            }
        }
    }
}

/// The column pass, in place.
fn columns(kind: Idct, block: &mut [i16; 64]) {
    for c in 0..8 {
        let mut x = [0i32; 8];
        for (r, v) in x.iter_mut().enumerate() {
            *v = i32::from(block[8 * r + c]);
        }
        let out = match kind {
            Idct::C10 => {
                // 8192 is added in 16 bits, the rounding term 8 in 32.
                x[0] = i32::from(block[c].wrapping_add(8192)) + 8;
                idct_1d(&W10, x, 0).map(|v| (v >> 18) as i16)
            }
            Idct::Asm10 => {
                // Both terms are added in 16 bits.
                x[0] = i32::from(block[c].wrapping_add(8192 + 8));
                idct_1d(&W10, x, 0).map(|v| saturate(v >> 18))
            }
            Idct::C12 => {
                x[0] = i32::from(block[c].wrapping_add(8192)) + 2;
                idct_1d(&W12, x, 0).map(|v| (v >> 17) as i16)
            }
        };
        for (r, v) in out.into_iter().enumerate() {
            block[8 * r + c] = v;
        }
    }
}

/// `idct_put` without the store: dequantise `block` with `qmat`, transform
/// it, and clip the result to the sample range.
pub fn idct_block(kind: Idct, block: &mut [i16; 64], qmat: &[i16; 64]) {
    for (b, q) in block.iter_mut().zip(qmat) {
        *b = b.wrapping_mul(*q);
    }
    rows(kind, block);
    columns(kind, block);
    let max = kind.clip_max();
    for b in block.iter_mut() {
        *b = (*b).clamp(CLIP_MIN, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(kind: Idct, dc: i16) -> [i16; 64] {
        let mut b = [0i16; 64];
        b[0] = dc;
        idct_block(kind, &mut b, &[1; 64]);
        b
    }

    #[test]
    fn zero_block_is_mid_grey() {
        assert_eq!(flat(Idct::C10, 0), [512; 64]);
        assert_eq!(flat(Idct::Asm10, 0), [512; 64]);
        assert_eq!(flat(Idct::C12, 0), [2048; 64]);
    }

    #[test]
    fn dc_scales_and_clips() {
        assert_eq!(flat(Idct::Asm10, 64), [514; 64]);
        assert_eq!(flat(Idct::C10, 64), [514; 64]);
        assert_eq!(flat(Idct::Asm10, 32000), [1019; 64]);
        assert_eq!(flat(Idct::Asm10, -32000), [4; 64]);
        assert_eq!(flat(Idct::C12, 64), [2056; 64]);
        assert_eq!(flat(Idct::C12, 4000), [2548; 64]);
        assert_eq!(flat(Idct::C12, 32000), [4091; 64]);
    }

    /// The assembly and the C code agree on ordinary input...
    #[test]
    fn c10_and_asm10_agree_on_small_coefficients() {
        let mut seed = 99u32;
        for _ in 0..2000 {
            let mut b = [0i16; 64];
            for v in b.iter_mut() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = ((seed >> 16) % 1024) as i16 - 512;
            }
            let (mut c, mut a) = (b, b);
            idct_block(Idct::C10, &mut c, &[4; 64]);
            idct_block(Idct::Asm10, &mut a, &[4; 64]);
            assert_eq!(c, a);
        }
    }

    /// ...and not on extreme input: the assembly's rounding term wraps
    /// around in 16 bits, and its row results saturate.
    #[test]
    fn c10_and_asm10_differ_at_the_extremes() {
        let mut b = [0i16; 64];
        b[0] = 32767;
        let (mut c, mut a) = (b, b);
        idct_block(Idct::C10, &mut c, &[1; 64]);
        idct_block(Idct::Asm10, &mut a, &[1; 64]);
        assert_eq!(c, [1019; 64]);
        assert_eq!(a, [4; 64]);

        let mut b = [0i16; 64];
        b[8..16].copy_from_slice(&[30000, 30000, 30000, 0, 0, 0, 0, 0]);
        let (mut c, mut a) = (b, b);
        idct_block(Idct::C10, &mut c, &[1; 64]);
        idct_block(Idct::Asm10, &mut a, &[1; 64]);
        assert_ne!(c, a);
    }
}
