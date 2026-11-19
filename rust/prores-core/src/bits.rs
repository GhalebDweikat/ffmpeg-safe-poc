//! The bit reader and the adaptive Rice / exp-Golomb codewords.
//!
//! This mirrors FFmpeg's `get_bits.h` reader as `proresdec.c` uses it,
//! including two things a decoder written from the format alone would not
//! have:
//!
//! * The position stops at eight bits past the end of the data, and reads
//!   there see whatever follows the data in the packet (the next plane, the
//!   next slice, or the zero bytes FFmpeg pads every packet with).
//! * The reader keeps a 32-bit window ("cache") that is refilled at the
//!   start of a codeword and shifted as bits are used. The end-of-data
//!   test in the coefficient loop looks at this window after a level and
//!   its sign bit have been shifted out of it, so it sees zeros where the
//!   window has run dry.

/// A bit reader over `data[..size]`; `data` goes on to the end of the
/// packet, and everything after that reads as zero.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    index: u32,
    size_in_bits: u32,
    size_in_bits_plus8: u32,
    cache: u32,
}

/// Big-endian 64-bit word at byte `pos`; bytes past the end are zero.
#[inline(always)]
fn rb64(data: &[u8], pos: usize) -> u64 {
    match data.get(pos..).and_then(|d| d.first_chunk::<8>()) {
        Some(b) => u64::from_be_bytes(*b),
        None => rb64_tail(data, pos),
    }
}

#[cold]
#[inline(never)]
fn rb64_tail(data: &[u8], pos: usize) -> u64 {
    let mut b = [0u8; 8];
    if let Some(tail) = data.get(pos..) {
        b[..tail.len()].copy_from_slice(tail);
    }
    u64::from_be_bytes(b)
}

impl<'a> BitReader<'a> {
    /// `size` is in bytes and at most 65535 here (a slice's size is a
    /// 16-bit field).
    pub fn new(data: &'a [u8], size: usize) -> Self {
        let bits = (size as u32) << 3;
        BitReader { data, index: 0, size_in_bits: bits, size_in_bits_plus8: bits + 8, cache: 0 }
    }

    /// `get_bits_left`: negative once the position is past the end.
    #[inline(always)]
    pub fn bits_left(&self) -> i32 {
        self.size_in_bits as i32 - self.index as i32
    }

    /// `UPDATE_CACHE_32`: the 32 bits at the position.
    #[inline(always)]
    fn update_cache(&mut self) {
        let word = rb64(self.data, (self.index >> 3) as usize);
        self.cache = ((word << (self.index & 7)) >> 32) as u32;
    }

    /// `SKIP_COUNTER`, also `LAST_SKIP_BITS`: the window is left alone.
    #[inline(always)]
    fn skip_counter(&mut self, n: u32) {
        self.index = self.size_in_bits_plus8.min(self.index + n);
    }

    /// `SKIP_BITS`: `n` is below 32.
    #[inline(always)]
    fn skip_bits(&mut self, n: u32) {
        self.cache <<= n;
        self.skip_counter(n);
    }

    /// `SHOW_UBITS` for 1 to 31 bits.
    #[inline(always)]
    fn show_ubits(&self, n: u32) -> u32 {
        self.cache >> (32 - n)
    }

    /// `DECODE_CODEWORD`. `None` for a code that is too long.
    ///
    /// With `keep_window` the bits are also shifted out of the window
    /// (`SKIP_BITS`); without, only the position moves (`LAST_SKIP_BITS`).
    #[inline(always)]
    fn codeword(&mut self, codebook: u8, keep_window: bool) -> Option<u32> {
        self.update_cache();
        let buf = self.cache;

        // Number of prefix bits that switch between Rice and exp-Golomb.
        let switch_bits = u32::from(codebook & 3);
        let rice_order = u32::from(codebook >> 5);
        let exp_order = u32::from((codebook >> 2) & 7);

        // Leading zeros, 31 for an all-zero window.
        let q = (buf | 1).leading_zeros();

        let val;
        if q > switch_bits {
            let bits = exp_order + (q << 1) - switch_bits;
            if bits > 31 {
                return None;
            }
            val = (buf >> (32 - bits))
                .wrapping_sub(1 << exp_order)
                .wrapping_add((switch_bits + 1) << rice_order);
            if keep_window {
                self.skip_bits(bits);
            } else {
                self.skip_counter(bits);
            }
        } else if rice_order != 0 {
            self.skip_bits(q + 1);
            val = (q << rice_order) + self.show_ubits(rice_order);
            if keep_window {
                self.skip_bits(rice_order);
            } else {
                self.skip_counter(rice_order);
            }
        } else {
            val = q;
            if keep_window {
                self.skip_bits(q + 1);
            } else {
                self.skip_counter(q + 1);
            }
        }
        Some(val)
    }

    /// `get_bits1`.
    #[inline(always)]
    pub fn get_bit(&mut self) -> u32 {
        let byte = self.data.get((self.index >> 3) as usize).copied().unwrap_or(0);
        let bit = u32::from(byte << (self.index & 7) >> 7);
        if self.index < self.size_in_bits_plus8 {
            self.index += 1;
        }
        bit
    }

    /// `get_bits` for 1 to 25 bits.
    #[inline(always)]
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let word = rb64(self.data, (self.index >> 3) as usize);
        let window = ((word << (self.index & 7)) >> 32) as u32;
        self.skip_counter(n);
        window >> (32 - n)
    }
}

const FIRST_DC_CB: u8 = 0xB8;
const DC_CODEBOOK: [u8; 7] = [0x04, 0x28, 0x28, 0x4D, 0x4D, 0x70, 0x70];
/// Codebook for the next run / level, chosen by the previous one.
const RUN_TO_CB: [u8; 16] = [
    0x06, 0x06, 0x05, 0x05, 0x04, 0x29, 0x29, 0x29, 0x29, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x4C,
];
const LEV_TO_CB: [u8; 10] = [0x04, 0x0A, 0x05, 0x06, 0x04, 0x28, 0x28, 0x28, 0x28, 0x4C];

/// The damaged-data error of a coefficient plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Damaged;

/// `decode_dc_coeffs`: the DC coefficient of each block, coded as
/// differences. `out` holds `blocks` blocks of 64 coefficients.
pub fn decode_dc_coeffs(r: &mut BitReader<'_>, out: &mut [i16], blocks: usize) -> Result<(), Damaged> {
    let code = r.codeword(FIRST_DC_CB, false).ok_or(Damaged)?;
    let mut prev_dc = ((code >> 1) as i32 ^ -((code & 1) as i32)) as i16;
    out[0] = prev_dc;

    let mut code = 5u32;
    let mut sign = 0i32;
    for block in out.chunks_exact_mut(64).take(blocks).skip(1) {
        code = r.codeword(DC_CODEBOOK[code.min(6) as usize], false).ok_or(Damaged)?;
        if code != 0 {
            sign ^= -((code & 1) as i32);
        } else {
            sign = 0;
        }
        let delta = (((code.wrapping_add(1) >> 1) as i32) ^ sign).wrapping_sub(sign);
        prev_dc = prev_dc.wrapping_add(delta as i16);
        block[0] = prev_dc;
    }
    Ok(())
}

/// `decode_ac_coeffs`: (run, level) pairs over the coefficients of all the
/// blocks of a plane, interleaved: position `p` is coefficient `p / blocks`
/// (in scan order) of block `p % blocks`. `blocks` is a power of two.
pub fn decode_ac_coeffs(
    r: &mut BitReader<'_>,
    out: &mut [i16],
    blocks: usize,
    scan: &[u8; 64],
) -> Result<(), Damaged> {
    let log2_block_count = blocks.ilog2();
    let max_coeffs = 64u32 << log2_block_count;
    let block_mask = blocks as u32 - 1;

    r.update_cache();
    let mut run = 4u32;
    let mut level = 2u32;
    let mut pos = block_mask;
    loop {
        // Stop at the end of the data, or when only zero bits are left. The
        // window is not refilled for this test: after the first pair it has
        // already lost the level and sign bits.
        let bits_left = r.bits_left();
        if bits_left <= 0 || (bits_left < 32 && r.show_ubits(bits_left as u32) == 0) {
            break;
        }

        run = r.codeword(RUN_TO_CB[run.min(15) as usize], false).ok_or(Damaged)?;
        pos = pos.wrapping_add(run).wrapping_add(1);
        if pos >= max_coeffs {
            return Err(Damaged);
        }

        level = r.codeword(LEV_TO_CB[level.min(9) as usize], true).ok_or(Damaged)?;
        level = level.wrapping_add(1);

        let i = (pos >> log2_block_count) as usize;
        let sign = (r.cache as i32) >> 31;
        r.skip_bits(1);
        let value = (level as i32 ^ sign).wrapping_sub(sign) as i16;
        out[(((pos & block_mask) as usize) << 6) + usize::from(scan[i])] = value;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A bit writer and the encoder side of the codewords, for tests.
    pub struct BitWriter {
        pub bytes: std::vec::Vec<u8>,
        acc: u64,
        n: u32,
    }

    impl BitWriter {
        pub fn new() -> Self {
            BitWriter { bytes: std::vec::Vec::new(), acc: 0, n: 0 }
        }
        pub fn put(&mut self, bits: u32, value: u32) {
            for i in (0..bits).rev() {
                self.acc = self.acc << 1 | u64::from(value >> i & 1);
                self.n += 1;
                if self.n == 8 {
                    self.bytes.push(self.acc as u8);
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
        pub fn finish(mut self) -> std::vec::Vec<u8> {
            while self.n != 0 {
                self.put(1, 0);
            }
            self.bytes
        }
        /// `encode_vlc_codeword` of FFmpeg's encoders.
        pub fn codeword(&mut self, codebook: u8, val: u32) {
            let switch_bits = u32::from(codebook & 3) + 1;
            let rice_order = u32::from(codebook >> 5);
            let exp_order = u32::from((codebook >> 2) & 7);
            let switch_val = switch_bits << rice_order;
            if val >= switch_val {
                let v = val - switch_val + (1 << exp_order);
                let exponent = v.ilog2();
                self.put(exponent - exp_order + switch_bits, 0);
                self.put(exponent + 1, v);
            } else {
                let exponent = val >> rice_order;
                self.put(exponent, 0);
                self.put(1, 1);
                self.put(rice_order, val & ((1 << rice_order) - 1));
            }
        }
        /// The coefficients of a plane, the way the encoders write them.
        pub fn plane(&mut self, coeffs: &[i16], blocks: usize, scan: &[u8; 64]) {
            let mut prev = i32::from(coeffs[0]);
            let first = if prev < 0 { (-prev * 2 - 1) as u32 } else { (prev * 2) as u32 };
            self.codeword(FIRST_DC_CB, first);
            let (mut code, mut sign) = (5u32, 0i32);
            for b in 1..blocks {
                let dc = i32::from(coeffs[b * 64]);
                let delta = dc - prev;
                // Undo the decoder: delta = ((code + 1) >> 1) with the sign
                // of the last non-zero delta flipped when code is odd.
                let mag = delta.unsigned_abs();
                let new_sign = if delta < 0 { -1 } else { 0 };
                let c = if mag == 0 {
                    0
                } else if new_sign != sign {
                    2 * mag - 1
                } else {
                    2 * mag
                };
                self.codeword(DC_CODEBOOK[code.min(6) as usize], c);
                code = c;
                if c != 0 {
                    sign = new_sign;
                } else {
                    sign = 0;
                }
                prev = dc;
            }
            let (mut run_cb, mut lev_cb) = (4u32, 2u32);
            let mut run = 0u32;
            for i in 1..64 {
                for b in 0..blocks {
                    let v = i32::from(coeffs[b * 64 + usize::from(scan[i])]);
                    if v == 0 {
                        run += 1;
                        continue;
                    }
                    self.codeword(RUN_TO_CB[run_cb.min(15) as usize], run);
                    let level = v.unsigned_abs() - 1;
                    self.codeword(LEV_TO_CB[lev_cb.min(9) as usize], level);
                    self.put(1, u32::from(v < 0));
                    run_cb = run;
                    lev_cb = level + 1;
                    run = 0;
                }
            }
        }
    }

    const IDENTITY: [u8; 64] = {
        let mut s = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            s[i] = i as u8;
            i += 1;
        }
        s
    };

    #[test]
    fn codewords_round_trip() {
        for &cb in [FIRST_DC_CB].iter().chain(&DC_CODEBOOK).chain(&RUN_TO_CB).chain(&LEV_TO_CB) {
            for val in (0..300).chain([1000, 4095, 20000]) {
                let mut w = BitWriter::new();
                w.codeword(cb, val);
                w.put(1, 1);
                let bytes = w.finish();
                let mut r = BitReader::new(&bytes, bytes.len());
                assert_eq!(r.codeword(cb, false), Some(val), "codebook {cb:#x} value {val}");
                assert_eq!(r.get_bit(), 1);
            }
        }
    }

    #[test]
    fn plane_round_trip() {
        let mut seed = 12345u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed >> 8
        };
        for blocks in [1usize, 2, 4, 8, 16, 32] {
            let mut coeffs = [0i16; 32 * 64];
            for c in coeffs[..blocks * 64].iter_mut() {
                if rnd() % 3 == 0 {
                    *c = (rnd() % 2001) as i16 - 1000;
                }
            }
            let mut w = BitWriter::new();
            w.plane(&coeffs[..blocks * 64], blocks, &IDENTITY);
            let bytes = w.finish();
            let mut out = [0i16; 32 * 64];
            let mut r = BitReader::new(&bytes, bytes.len());
            decode_dc_coeffs(&mut r, &mut out, blocks).unwrap();
            decode_ac_coeffs(&mut r, &mut out, blocks, &IDENTITY).unwrap();
            assert_eq!(out[..], coeffs[..], "{blocks} blocks");
        }
    }

    #[test]
    fn reads_past_the_end_see_what_follows() {
        // One byte of data, then a byte that is not part of it.
        let bytes = [0b1010_0000u8, 0xff];
        let mut r = BitReader::new(&bytes, 1);
        assert_eq!(r.get_bits(4), 0b1010);
        assert_eq!(r.get_bits(8), 0b0000_1111);
        assert_eq!(r.bits_left(), -4);
        assert_eq!(r.get_bits(8), 0b1111_0000);
        // The position has stopped at 16; from here on every read is zero.
        assert_eq!(r.bits_left(), -8);
        assert_eq!(r.get_bits(8), 0);
        assert_eq!(r.get_bit(), 0);
        assert_eq!(r.bits_left(), -8);
    }

    #[test]
    fn too_long_a_code_is_damage() {
        let bytes = [0u8; 16];
        let mut r = BitReader::new(&bytes, 16);
        let mut out = [0i16; 64];
        assert_eq!(decode_dc_coeffs(&mut r, &mut out, 1), Err(Damaged));
    }
}
