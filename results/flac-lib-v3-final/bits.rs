//! The bit reader, with the behaviour of FFmpeg's checked `GetBitContext`
//! (get_bits.h) and of its Rice code reader (golomb.h).
//!
//! FFmpeg's reader never fails. Its position stops at eight bits past the
//! end of the buffer, and what it reads from there on comes from the zeroed
//! padding every packet has. The decoder relies on that: it reads freely
//! and only compares the final position with the packet size. Here the
//! position is not clamped while reading (bits past the end read as zero
//! either way); `index` gives FFmpeg's clamped value where the decoder
//! looks at it.

use crate::Invalid;

/// `get_ur_golomb_jpegls` reads the unary part 25 bits at a time.
const MIN_CACHE_BITS: usize = 25;

#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// In bits, from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    fn size_in_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// FFmpeg's `get_bits_count`: the position, which stops at 8 bits past
    /// the end.
    pub fn index(&self) -> usize {
        self.pos.min(self.size_in_bits() + 8)
    }

    /// FFmpeg's `get_bits_left`; -8 at the lowest.
    pub fn bits_left(&self) -> i64 {
        self.size_in_bits() as i64 - self.index() as i64
    }

    /// The 64 bits starting at the byte the position is in; zero past the
    /// end of the data.
    #[inline]
    fn window(&self) -> u64 {
        let i = self.pos >> 3;
        match self.data.get(i..).and_then(|d| d.first_chunk::<8>()) {
            Some(c) => u64::from_be_bytes(*c),
            None => {
                let mut b = [0u8; 8];
                if let Some(rest) = self.data.get(i..) {
                    b[..rest.len()].copy_from_slice(rest);
                }
                u64::from_be_bytes(b)
            }
        }
    }

    /// The next `n` bits (1 to 32) without consuming them.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!((1..=32).contains(&n));
        ((self.window() << (self.pos & 7)) >> (64 - n)) as u32
    }

    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    /// `get_bits` / `get_bits_long`: 0 to 32 bits.
    #[inline]
    pub fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n as usize);
        v
    }

    #[inline]
    pub fn get1(&mut self) -> bool {
        self.get(1) != 0
    }

    /// `get_sbits` / `get_sbits_long`: 0 to 32 bits, sign-extended.
    #[inline]
    pub fn get_signed(&mut self, n: u32) -> i32 {
        if n == 0 {
            return 0;
        }
        let shift = 32 - n;
        ((self.get(n) << shift) as i32) >> shift
    }

    /// `get_sbits64(gb, 33)`.
    #[inline]
    pub fn get_signed33(&mut self) -> i64 {
        let hi = u64::from(self.get(1));
        let v = (hi << 32) | u64::from(self.get(32));
        ((v << 31) as i64) >> 31
    }

    /// `align_get_bits`.
    pub fn align(&mut self) {
        let n = self.index().wrapping_neg() & 7;
        self.skip(n);
    }

    /// `get_unary(gb, 1, len)`: the number of zero bits before the next one
    /// bit, which is consumed too; stops after `len` zeros.
    pub fn get_unary(&mut self, len: i64) -> u32 {
        let mut i = 0;
        while i64::from(i) < len && !self.get1() {
            i += 1;
        }
        i
    }

    /// `get_ur_golomb_jpegls(gb, k, limit, 1)`: a Rice code with parameter
    /// `k`. `None` is the C function's -1.
    ///
    /// The unary part is at most `limit - 2` zeros for an ordinary code;
    /// `limit - 1` zeros select the one-bit escape; more is an error, as is
    /// running off the end of the data.
    pub fn get_ur_golomb(&mut self, k: u32, limit: u32) -> Option<u32> {
        let limit = limit as usize;
        let mut i = 0usize;
        while i + MIN_CACHE_BITS <= limit && self.peek(MIN_CACHE_BITS as u32) == 0 {
            if self.pos >= self.size_in_bits() {
                return None;
            }
            self.skip(MIN_CACHE_BITS);
            i += MIN_CACHE_BITS;
        }
        while i < limit && self.peek(1) == 0 {
            self.skip(1);
            i += 1;
        }
        self.skip(1);

        if i < limit - 1 {
            let low = self.get(k);
            Some(low.wrapping_add((i as u32) << k))
        } else if i == limit - 1 {
            Some(self.get(1) + 1)
        } else {
            None
        }
    }
}

/// A bit buffer for runs of codes inside the data, where no padding is
/// involved: up to 64 upcoming bits in a register, topped up without a
/// branch before every code.
///
/// `buf` holds the upcoming bits from its top; `cnt` of them (56 to 63
/// after `refill`) are accounted for, and any below those are either
/// right or zero. `ptr` is the byte the next top-up loads from; the
/// reader's position is `ptr * 8 - cnt`.
struct Fast<'a> {
    data: &'a [u8],
    buf: u64,
    cnt: u32,
    ptr: usize,
}

impl<'a> Fast<'a> {
    /// `None` when fewer than eight bytes are left at `pos`.
    #[inline(always)]
    fn start(data: &'a [u8], pos: usize) -> Option<Self> {
        let byte = pos >> 3;
        let chunk = data.get(byte..)?.first_chunk::<8>()?;
        let skip = (pos & 7) as u32;
        Some(Fast { data, buf: u64::from_be_bytes(*chunk) << skip, cnt: 56 - skip, ptr: byte + 7 })
    }

    /// Top up to at least 56 bits. `false` near the end of the data, where
    /// eight bytes cannot be loaded any more; the buffer is then unchanged.
    #[inline(always)]
    fn refill(&mut self) -> bool {
        let Some(chunk) = self.data.get(self.ptr..).and_then(|d| d.first_chunk::<8>()) else {
            return false;
        };
        self.buf |= u64::from_be_bytes(*chunk) >> self.cnt;
        self.ptr += ((63 - self.cnt) >> 3) as usize;
        self.cnt |= 56;
        true
    }

    #[inline(always)]
    fn pos(&self) -> usize {
        self.ptr * 8 - self.cnt as usize
    }
}

impl BitReader<'_> {
    /// `get_sr_golomb_flac`, and the caller's check for its error value.
    #[inline]
    fn get_sr_golomb(&mut self, k: u32, limit: u32) -> Result<i32, Invalid> {
        let v = self.get_ur_golomb(k, limit).unwrap_or(u32::MAX);
        let v = ((v >> 1) ^ (v & 1).wrapping_neg()) as i32;
        if v == i32::MIN {
            return Err(Invalid);
        }
        Ok(v)
    }

    /// A run of Rice codes with parameter `k`: one partition of a residual.
    ///
    /// Codes that lie inside the data and are short enough to be in the
    /// bit buffer whole are decoded there. The rest go through
    /// `get_ur_golomb`: long unary parts, the escape, errors, and whatever
    /// comes near the end of the packet.
    #[inline(always)]
    pub fn read_rice(&mut self, out: &mut [i32], k: u32, limit: u32) -> Result<(), Invalid> {
        // A code with at most this many zeros is in the buffer whole, is an
        // ordinary code and cannot be the error value.
        let fast_max = (55 - k).min(limit - 3);
        let mut done = 0;
        if let Some(mut f) = Fast::start(self.data, self.pos) {
            let mut in_sync = false;
            done = out.len();
            for (i, o) in out.iter_mut().enumerate() {
                if !f.refill() {
                    done = i;
                    break;
                }
                let zeros = f.buf.leading_zeros();
                if zeros > fast_max {
                    self.pos = f.pos();
                    *o = self.get_sr_golomb(k, limit)?;
                    match Fast::start(self.data, self.pos) {
                        Some(next) => f = next,
                        None => {
                            done = i + 1;
                            in_sync = true;
                            break;
                        }
                    }
                    continue;
                }
                let rest = (f.buf << zeros) << 1;
                let v = (zeros << k) | ((rest >> 1) >> (63 - k)) as u32;
                f.buf = rest << k;
                f.cnt -= zeros + 1 + k;
                *o = ((v >> 1) ^ (v & 1).wrapping_neg()) as i32;
            }
            if !in_sync {
                self.pos = f.pos();
            }
        }
        for o in &mut out[done..] {
            *o = self.get_sr_golomb(k, limit)?;
        }
        Ok(())
    }

    /// A run of `n`-bit signed values (`n` from 0 to 32): verbatim samples
    /// or an escaped partition.
    #[inline(always)]
    pub fn read_signed_run(&mut self, out: &mut [i32], n: u32) {
        if n == 0 {
            out.fill(0);
            return;
        }
        let mut done = 0;
        if let Some(mut f) = Fast::start(self.data, self.pos) {
            done = out.len();
            for (i, o) in out.iter_mut().enumerate() {
                if !f.refill() {
                    done = i;
                    break;
                }
                *o = ((f.buf as i64) >> (64 - n)) as i32;
                f.buf <<= n;
                f.cnt -= n;
            }
            self.pos = f.pos();
        }
        for o in &mut out[done..] {
            *o = self.get_signed(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buffered readers agree with the one-at-a-time reader on random
    /// bits, for every parameter and at every distance from the end.
    #[test]
    fn runs_match_single_reads() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut data = [0u8; 300];
        for round in 0..40 {
            for b in data.iter_mut() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Mostly-zero bytes now and then, for long unary parts.
                *b = if round % 4 == 3 && (seed >> 40) & 3 != 0 { 0 } else { (seed >> 56) as u8 };
            }
            for k in 0..=30u32 {
                let limit = if k > 1 { ((i32::MAX >> (k - 1)) + 2) as u32 } else { i32::MAX as u32 };
                let start = (round * 7 + k as usize) % 23;
                let mut a = BitReader::new(&data);
                let mut b = BitReader::new(&data);
                a.skip(start);
                b.skip(start);
                let mut run = [0i32; 150];
                let got = a.read_rice(&mut run, k, limit);
                let mut want = Ok(());
                let mut single = [0i32; 150];
                for o in single.iter_mut() {
                    match b.get_sr_golomb(k, limit) {
                        Ok(v) => *o = v,
                        Err(e) => {
                            want = Err(e);
                            break;
                        }
                    }
                }
                assert_eq!(got, want, "k {k} round {round}");
                if got.is_ok() {
                    assert_eq!(run, single, "k {k} round {round}");
                    assert_eq!(a.index(), b.index(), "k {k} round {round}");
                }
            }
            for n in 0..=32u32 {
                let mut a = BitReader::new(&data);
                let mut b = BitReader::new(&data);
                a.skip(n as usize + round);
                b.skip(n as usize + round);
                let mut run = [0i32; 90];
                a.read_signed_run(&mut run, n);
                let single: Vec<i32> = (0..90).map(|_| b.get_signed(n)).collect();
                assert_eq!(run[..], single[..], "n {n}");
                assert_eq!(a.index(), b.index(), "n {n}");
            }
        }
    }

    #[test]
    fn reads_and_zero_padding() {
        let mut r = BitReader::new(&[0xA5, 0xFF, 0x01]);
        assert_eq!(r.get(4), 0xA);
        assert_eq!(r.get_signed(4), 5);
        assert_eq!(r.get_signed(8), -1);
        assert_eq!(r.peek(32), 0x0100_0000);
        assert_eq!(r.get(8), 1);
        assert_eq!(r.bits_left(), 0);
        assert_eq!(r.get(7), 0);
        assert_eq!(r.index(), 31);
        assert_eq!(r.get(32), 0);
        assert_eq!(r.index(), 32);
        assert_eq!(r.bits_left(), -8);
    }

    #[test]
    fn signed33() {
        let mut r = BitReader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x80]);
        assert_eq!(r.get_signed33(), -1);
        let mut r = BitReader::new(&[0x80, 0, 0, 0, 0]);
        assert_eq!(r.get_signed33(), -(1i64 << 32));
    }

    #[test]
    fn rice_codes() {
        // k = 2: 001 then 11 -> 2 << 2 | 3 = 11.
        let mut r = BitReader::new(&[0b0011_1000, 0, 0, 0]);
        assert_eq!(r.get_ur_golomb(2, 1 << 20), Some(11));
        assert_eq!(r.index(), 5);
        // Limit 5 (k = 30): four zeros are the escape, five an error.
        let mut r = BitReader::new(&[0b0000_1100, 0, 0, 0, 0]);
        assert_eq!(r.get_ur_golomb(30, 5), Some(2));
        let mut r = BitReader::new(&[0b0000_0100, 0, 0, 0, 0]);
        assert_eq!(r.get_ur_golomb(30, 5), None);
        // No one bit before the end of the data.
        let mut r = BitReader::new(&[0, 0, 0, 0, 0, 0]);
        assert_eq!(r.get_ur_golomb(3, 1 << 20), None);
    }
}
