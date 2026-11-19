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

#[cfg(test)]
mod tests {
    use super::*;

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
