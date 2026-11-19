//! FFmpeg's cached big-endian bit reader (`bitstream_template.h`), as the
//! MagicYUV decoder is compiled with it: checked, 64-bit cache.
//!
//! What matters for bit-exactness is what it returns at the end of a
//! slice. The cache is filled 8 bytes at first and then 4 at a time, and a
//! fill is refused only when the read position has reached the end of the
//! slice. So the last fill can take in up to 7 bytes that belong to
//! whatever follows the slice in the packet, and those bits are decoded
//! like any others. After that the reader returns zeros. The "bits left"
//! count goes negative rather than stopping at zero.
//!
//! Bytes past the end of the packet read as zero here. FFmpeg requires
//! packets to be followed by padding, which callers are told to zero.

use crate::vlc::{Multi, Vlc};

pub struct BitReader<'a> {
    /// From the start of the slice to the end of the packet.
    buf: &'a [u8],
    /// Size of the slice in bytes: no fill starts at or after this.
    end: usize,
    ptr: usize,
    bits: u64,
    /// Bits left in `bits`. Wraps around like the C `unsigned` when more
    /// bits are skipped than were there.
    valid: u32,
    size_in_bits: u32,
}

impl<'a> BitReader<'a> {
    /// `init_get_bits8`. `None` where the C function fails.
    pub fn new(buf: &'a [u8], byte_size: u32) -> Option<Self> {
        if byte_size > (i32::MAX / 8) as u32 {
            return None;
        }
        let mut r = BitReader {
            buf,
            end: byte_size as usize,
            ptr: 0,
            bits: 0,
            valid: 0,
            size_in_bits: byte_size * 8,
        };
        if r.ptr < r.end {
            r.bits = u64::from_be_bytes(r.bytes());
            r.ptr += 8;
            r.valid = 64;
        }
        Some(r)
    }

    /// `N` bytes at the read position; zero past the end of the packet.
    fn bytes<const N: usize>(&self) -> [u8; N] {
        let rest = self.buf.get(self.ptr..).unwrap_or(&[]);
        match rest.first_chunk::<N>() {
            Some(b) => *b,
            None => {
                let mut b = [0; N];
                b[..rest.len()].copy_from_slice(rest);
                b
            }
        }
    }

    /// `bits_priv_refill_32`: false at the end of the slice.
    fn refill_32(&mut self) -> bool {
        if self.ptr >= self.end {
            return false;
        }
        // Only called with fewer than 32 valid bits.
        self.bits |= (u32::from_be_bytes(self.bytes()) as u64) << (32 - self.valid);
        self.ptr += 4;
        self.valid += 32;
        true
    }

    /// `get_bits_left`.
    pub fn left(&self) -> i32 {
        (self.size_in_bits as i64 + self.valid as i64 - 8 * self.ptr as i64) as i32
    }

    /// `bits_peek_nz` for 1 to 32 bits.
    fn peek(&mut self, n: u32) -> u32 {
        if n > self.valid {
            self.refill_32();
        }
        (self.bits >> (64 - n)) as u32
    }

    /// `bits_priv_skip_remaining` for up to 32 bits.
    fn skip(&mut self, n: u32) {
        self.bits <<= n;
        self.valid = self.valid.wrapping_sub(n);
    }

    /// `get_bits` (`bits_read_nz`) for 1 to 32 bits.
    pub fn read(&mut self, n: u32) -> u32 {
        if n > self.valid && !self.refill_32() {
            self.valid = n;
        }
        let v = (self.bits >> (64 - n)) as u32;
        self.skip(n);
        v
    }

    /// `bits_priv_set_idx`: look the next `nb_bits` bits up in the table
    /// that starts at `code`. Returns that entry's symbol and length.
    fn sub_entry(&mut self, vlc: &Vlc, code: i32, nb_bits: u32) -> (i32, i32) {
        let e = vlc.table[self.peek(nb_bits) as usize + code as usize];
        (e.sym as i32, e.len as i32)
    }

    /// The part of `bits_read_vlc` after the first lookup, for a maximum
    /// depth of 3. Takes the first-level entry; returns the symbol and how
    /// many bits are still to be skipped (0 if the bits match no code).
    fn descend(&mut self, vlc: &Vlc, mut code: i32, mut n: i32) -> (i32, u32) {
        if n < 0 {
            self.skip(vlc.bits);
            let nb_bits = n.unsigned_abs();
            (code, n) = self.sub_entry(vlc, code, nb_bits);
            if n < 0 {
                self.skip(nb_bits);
                (code, n) = self.sub_entry(vlc, code, n.unsigned_abs());
            }
        }
        // The table builder never makes a fourth level.
        (code, n.max(0) as u32)
    }

    /// `get_vlc2` with a maximum depth of 3: one symbol, or -1 if the bits
    /// match no code (the bits of the last lookup are then not consumed).
    pub fn read_vlc(&mut self, vlc: &Vlc) -> i32 {
        let e = vlc.table[self.peek(vlc.bits) as usize];
        let (code, n) = self.descend(vlc, e.sym as i32, e.len as i32);
        self.skip(n);
        code
    }

    /// `get_vlc_multi` with a maximum depth of 3: either a joint table
    /// entry holding several symbols, or one symbol from the ordinary
    /// table and whether the bits were a code at all.
    pub fn read_vlc_multi(&mut self, vlc: &Vlc) -> Multi {
        let idx = self.peek(vlc.bits) as usize;
        let elem = vlc.multi[idx];
        let num = (elem >> 56) as usize;
        if num != 0 {
            self.skip((elem >> 48) as u8 as u32);
            return Multi::Joint(elem, num);
        }
        let e = vlc.table[idx];
        let (code, n) = self.descend(vlc, e.sym as i32, e.len as i32);
        self.skip(n);
        Multi::Single(code, n > 0)
    }
}
