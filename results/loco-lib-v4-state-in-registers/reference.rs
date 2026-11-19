//! Test-only: the straight port of `loco_decode_plane`, kept as the model
//! the optimised decoder is compared against in the unit tests.
//!
//! It follows the C statement by statement, including the bit reader's
//! position clamp and its reads of the zero bytes FFmpeg keeps after every
//! packet.

/// `get_bits.h`: the checked reader. `index` never passes
/// `size_in_bits + 8`; the cache is the 32 bits at `index`, read from the
/// packet and, past its end, from the zero padding.
pub struct GetBits<'a> {
    buf: &'a [u8],
    pub index: u32,
    size_in_bits: u32,
    size_in_bits_plus8: u32,
}

impl<'a> GetBits<'a> {
    /// `init_get_bits8`. Fails on buffers too large for a 31-bit bit count.
    pub fn new(buf: &'a [u8]) -> Option<Self> {
        let bits = buf.len().checked_mul(8)?;
        if bits >= i32::MAX as usize - 64 * 8 {
            return None;
        }
        let bits = bits as u32;
        Some(GetBits { buf, index: 0, size_in_bits: bits, size_in_bits_plus8: bits + 8 })
    }

    /// `UPDATE_CACHE`.
    fn cache(&self, index: u32) -> u32 {
        let at = (index >> 3) as usize;
        let mut word = [0u8; 4];
        for (i, b) in word.iter_mut().enumerate() {
            *b = self.buf.get(at + i).copied().unwrap_or(0);
        }
        u32::from_be_bytes(word) << (index & 7)
    }

    /// `SKIP_COUNTER`.
    fn skip(&self, index: u32, n: u32) -> u32 {
        self.size_in_bits_plus8.min(index + n)
    }

    /// `get_bits_left`.
    pub fn left(&self) -> i64 {
        i64::from(self.size_in_bits) - i64::from(self.index)
    }

    /// `get_ur_golomb_jpegls(gb, k, INT_MAX, 0)`. `u32::MAX` is the C
    /// function's -1.
    pub fn ur_golomb_jpegls(&mut self, k: u32) -> u32 {
        let mut index = self.index;
        let mut cache = self.cache(index);
        let log = 31 - (cache | 1).leading_zeros();

        if log >= k + 7 {
            let buf = (cache >> (log - k)).wrapping_add(30u32.wrapping_sub(log) << k);
            self.index = self.skip(index, 32 + k - log);
            return buf;
        }

        let mut i: u32 = 0;
        while cache >> 7 == 0 {
            if self.size_in_bits <= index {
                self.index = index;
                return u32::MAX;
            }
            index = self.skip(index, 25);
            cache = self.cache(index);
            i += 25;
        }
        while cache >> 31 == 0 {
            cache <<= 1;
            index = self.skip(index, 1);
            i += 1;
        }
        index = self.skip(index, 1);
        cache = self.cache(index);

        let mut buf = 0;
        if k != 0 {
            buf = cache >> (32 - k);
            index = self.skip(index, k);
        }
        self.index = index;
        buf.wrapping_add(i << k)
    }
}

/// `RICEContext`.
pub struct Rice<'a> {
    pub gb: GetBits<'a>,
    save: i32,
    run: i32,
    run2: i32,
    sum: i32,
    count: i32,
    lossy: u32,
}

impl<'a> Rice<'a> {
    pub fn new(buf: &'a [u8], lossy: u32) -> Option<Self> {
        Some(Rice { gb: GetBits::new(buf)?, save: 0, run: 0, run2: 0, sum: 8, count: 1, lossy })
    }

    /// `loco_get_rice_param`.
    fn param(&self) -> u32 {
        let mut cnt = 0;
        let mut val = self.count;
        while self.sum > val && cnt < 9 {
            val <<= 1;
            cnt += 1;
        }
        cnt
    }

    /// `loco_update_rice_param`.
    fn update(&mut self, val: i32) {
        self.sum = self.sum.wrapping_add(val);
        self.count += 1;
        if self.count == 16 {
            self.sum >>= 1;
            self.count >>= 1;
        }
    }

    /// `loco_get_rice`. `None` is the C function's `INT_MIN`.
    pub fn get(&mut self) -> Option<u32> {
        if self.run > 0 {
            self.run -= 1;
            self.update(0);
            return Some(0);
        }
        if self.gb.left() < 1 {
            return None;
        }
        let mut v = self.gb.ur_golomb_jpegls(self.param());
        if v == u32::MAX {
            return None;
        }
        self.update((v.wrapping_add(1) >> 1) as i32);
        if v == 0 {
            if self.save >= 0 {
                let run = self.gb.ur_golomb_jpegls(2);
                if run == u32::MAX {
                    return None;
                }
                self.run = run as i32;
                if self.run > 1 {
                    self.save = self.save.wrapping_add(self.run.wrapping_add(1));
                } else {
                    self.save = self.save.wrapping_sub(3);
                }
            } else {
                self.run2 = self.run2.wrapping_add(1);
            }
        } else {
            v = (v >> 1).wrapping_add(self.lossy) ^ 0u32.wrapping_sub(v & 1);
            if self.run2 > 0 {
                if self.run2 > 2 {
                    self.save = self.save.wrapping_add(self.run2);
                } else {
                    self.save = self.save.wrapping_sub(3);
                }
                self.run2 = 0;
            }
        }
        if v == 0x8000_0000 {
            return None;
        }
        Some(v)
    }
}

fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// `loco_decode_plane` on a plane of `rows` rows, `stride` bytes apart.
/// Returns the number of bytes used; `None` is any negative return.
#[allow(clippy::too_many_arguments)]
pub fn decode_plane(
    data: &mut [u8],
    stride: usize,
    rows: usize,
    width: usize,
    height: usize,
    bottom_up: bool,
    buf: &[u8],
    lossy: u32,
) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    let mut rc = Rice::new(buf, lossy)?;
    let row = |j: usize| if bottom_up { (rows - 1 - j) * stride } else { j * stride };

    // restore top left pixel
    let mut at = row(0);
    let val = rc.get()?;
    data[at] = 128u8.wrapping_add(val as u8);
    // restore top line
    for i in 1..width {
        let val = rc.get()?;
        data[at + i] = data[at + i - 1].wrapping_add(val as u8);
    }
    for j in 1..height {
        let above = at;
        at = row(j);
        // restore left column
        let val = rc.get()?;
        data[at] = data[above].wrapping_add(val as u8);
        // restore all other pixels
        for i in 1..width {
            let val = rc.get()?;
            let a = i32::from(data[above + i]);
            let b = i32::from(data[at + i - 1]);
            let c = i32::from(data[above + i - 1]);
            data[at + i] = (mid_pred(a, a + b - c, b) as u32).wrapping_add(val) as u8;
        }
    }

    Some(((rc.gb.index + 7) >> 3) as usize)
}
