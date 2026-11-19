//! Safe Rust port of FFmpeg's Apple Intermediate Codec decoder (`aic.c`),
//! together with the parts of FFmpeg it calls: the bit reader, the unary
//! and Exp-Golomb codes, the 8-bit "simple" inverse DCT and the clamping
//! pixel store.
//!
//! A frame is a 24-byte header, a table of slice sizes, and slices. A slice
//! covers up to 32 macroblocks of one macroblock row and holds four bands of
//! coefficients (luma, chroma, and the high-frequency rest of each). The
//! picture is 4:2:0, 8 bits.
//!
//! Three things in the C decoder are copied here on purpose (see
//! `notes/aic.md`):
//!
//! * the coefficient buffer is kept between slices and frames and only its
//!   first part is cleared for a slice narrower than the others, so skipped
//!   coefficients of such a slice keep older values;
//! * the bit reader runs past the end of a slice into the bytes after it;
//! * FFmpeg's x86 assembly IDCT and its C IDCT round differently when
//!   intermediate values leave 16 bits, so both are here ([`Idct`]).

#![forbid(unsafe_code)]

pub const HDR_SIZE: usize = 24;
const BAND_COEFFS: usize = 64 + 32 + 192 + 96;

const NUM_BAND_COEFFS: [usize; 4] = [64, 32, 192, 96];
const BAND_OFF: [usize; 4] = [0, 64, 96, 288];

const QUANT_MATRIX: [u8; 64] = [
     8, 16, 19, 22, 22, 26, 26, 27,
    16, 16, 22, 22, 26, 27, 27, 29,
    19, 22, 26, 26, 27, 29, 29, 35,
    22, 24, 27, 27, 29, 32, 34, 38,
    26, 27, 29, 29, 32, 35, 38, 46,
    27, 29, 34, 34, 35, 40, 46, 56,
    29, 34, 34, 37, 40, 48, 56, 69,
    34, 37, 38, 40, 48, 58, 69, 83,
];

const Y_SCAN: [u8; 64] = [
     0,  4,  1,  2,  5,  8, 12,  9,
     6,  3,  7, 10, 13, 14, 11, 15,
    47, 43, 46, 45, 42, 39, 35, 38,
    41, 44, 40, 37, 34, 33, 36, 32,
    16, 20, 17, 18, 21, 24, 28, 25,
    22, 19, 23, 26, 29, 30, 27, 31,
    63, 59, 62, 61, 58, 55, 51, 54,
    57, 60, 56, 53, 50, 49, 52, 48,
];

const Y_EXT_SCAN: [u8; 192] = [
     64,  72,  65,  66,  73,  80,  88,  81,
     74,  67,  75,  82,  89,  90,  83,  91,
      0,   4,   1,   2,   5,   8,  12,   9,
      6,   3,   7,  10,  13,  14,  11,  15,
     16,  20,  17,  18,  21,  24,  28,  25,
     22,  19,  23,  26,  29,  30,  27,  31,
    155, 147, 154, 153, 146, 139, 131, 138,
    145, 152, 144, 137, 130, 129, 136, 128,
     47,  43,  46,  45,  42,  39,  35,  38,
     41,  44,  40,  37,  34,  33,  36,  32,
     63,  59,  62,  61,  58,  55,  51,  54,
     57,  60,  56,  53,  50,  49,  52,  48,
     96, 104,  97,  98, 105, 112, 120, 113,
    106,  99, 107, 114, 121, 122, 115, 123,
     68,  76,  69,  70,  77,  84,  92,  85,
     78,  71,  79,  86,  93,  94,  87,  95,
    100, 108, 101, 102, 109, 116, 124, 117,
    110, 103, 111, 118, 125, 126, 119, 127,
    187, 179, 186, 185, 178, 171, 163, 170,
    177, 184, 176, 169, 162, 161, 168, 160,
    159, 151, 158, 157, 150, 143, 135, 142,
    149, 156, 148, 141, 134, 133, 140, 132,
    191, 183, 190, 189, 182, 175, 167, 174,
    181, 188, 180, 173, 166, 165, 172, 164,
];

const C_SCAN: [u8; 64] = [
     0,  4,  1,  2,  5,  8, 12,  9,
     6,  3,  7, 10, 13, 14, 11, 15,
    31, 27, 30, 29, 26, 23, 19, 22,
    25, 28, 24, 21, 18, 17, 20, 16,
    32, 36, 33, 34, 37, 40, 44, 41,
    38, 35, 39, 42, 45, 46, 43, 47,
    63, 59, 62, 61, 58, 55, 51, 54,
    57, 60, 56, 53, 50, 49, 52, 48,
];

const C_EXT_SCAN: [u8; 192] = [
     16,  24,  17,  18,  25,  32,  40,  33,
     26,  19,  27,  34,  41,  42,  35,  43,
      0,   4,   1,   2,   5,   8,  12,   9,
      6,   3,   7,  10,  13,  14,  11,  15,
     20,  28,  21,  22,  29,  36,  44,  37,
     30,  23,  31,  38,  45,  46,  39,  47,
     95,  87,  94,  93,  86,  79,  71,  78,
     85,  92,  84,  77,  70,  69,  76,  68,
     63,  59,  62,  61,  58,  55,  51,  54,
     57,  60,  56,  53,  50,  49,  52,  48,
     91,  83,  90,  89,  82,  75,  67,  74,
     81,  88,  80,  73,  66,  65,  72,  64,
    112, 120, 113, 114, 121, 128, 136, 129,
    122, 115, 123, 130, 137, 138, 131, 139,
     96, 100,  97,  98, 101, 104, 108, 105,
    102,  99, 103, 106, 109, 110, 107, 111,
    116, 124, 117, 118, 125, 132, 140, 133,
    126, 119, 127, 134, 141, 142, 135, 143,
    191, 183, 190, 189, 182, 175, 167, 174,
    181, 188, 180, 173, 166, 165, 172, 164,
    159, 155, 158, 157, 154, 151, 147, 150,
    153, 156, 152, 149, 146, 145, 148, 144,
    187, 179, 186, 185, 178, 171, 163, 170,
    177, 184, 176, 169, 162, 161, 168, 160,
];

/// Coefficients per macroblock: six blocks of 64.
const MB_COEFFS: usize = BAND_COEFFS;

/// Where a coefficient goes in a macroblock's 384 values, given its band
/// and its position in the C decoder's buffer for that band (the value of
/// the C scan tables).
///
/// The C decoder keeps the four bands apart and builds each block from a
/// base and an extension band just before the IDCT (`recombine_block`,
/// `recombine_block_il`). Here the coefficients are stored where the IDCT
/// wants them straight away: block after block (four luma, two chroma),
/// each transposed, so that eight consecutive values are one column.
const fn place(interlaced: bool, band: usize, pos: usize) -> usize {
    let (block, row, col) = if interlaced && band == 0 {
        // Blocks 0 and 1: the left half of every row.
        (pos / 32, pos % 32 / 4, pos % 4)
    } else if interlaced && band == 2 {
        if pos < 64 {
            // Blocks 0 and 1: the right half of every row.
            (pos / 32, pos % 32 / 4, 4 + pos % 4)
        } else {
            // Blocks 2 and 3 entirely.
            (2 + (pos - 64) / 64, (pos - 64) % 64 / 8, pos % 8)
        }
    } else if band < 2 {
        // The top left quarter of each block.
        (4 * band + pos / 16, pos % 16 / 4, pos % 4)
    } else {
        let q = pos % 48;
        if q < 16 {
            // The top right quarter.
            (4 * (band - 2) + pos / 48, q / 4, 4 + q % 4)
        } else {
            // The bottom half.
            (4 * (band - 2) + pos / 48, 4 + (q - 16) / 8, q % 8)
        }
    };
    block * 64 + col * 8 + row
}

const fn place_table<const N: usize>(interlaced: bool, band: usize, scan: &[u8]) -> [u16; N] {
    let mut t = [0u16; N];
    let mut i = 0;
    while i < N {
        t[i] = place(interlaced, band, scan[i] as usize) as u16;
        i += 1;
    }
    t
}

// Progressive frames use the chroma scans for luma too (`force_chroma`).
static PLACE_LUMA: [u16; 64] = place_table(false, 0, &C_SCAN);
static PLACE_CHROMA: [u16; 32] = place_table(false, 1, &C_SCAN);
static PLACE_LUMA_EXT: [u16; 192] = place_table(false, 2, &C_EXT_SCAN);
static PLACE_CHROMA_EXT: [u16; 96] = place_table(false, 3, &C_EXT_SCAN);
static PLACE_LUMA_IL: [u16; 64] = place_table(true, 0, &Y_SCAN);
static PLACE_LUMA_EXT_IL: [u16; 192] = place_table(true, 2, &Y_EXT_SCAN);

/// For each band, where the coefficient with coding index i goes.
fn place_tables(interlaced: bool) -> [&'static [u16]; 4] {
    if interlaced {
        [&PLACE_LUMA_IL, &PLACE_CHROMA, &PLACE_LUMA_EXT_IL, &PLACE_CHROMA_EXT]
    } else {
        [&PLACE_LUMA, &PLACE_CHROMA, &PLACE_LUMA_EXT, &PLACE_CHROMA_EXT]
    }
}

/// Why a packet was rejected. FFmpeg returns `AVERROR_INVALIDDATA` for all
/// of them; the distinction is only used for the log message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    TooSmall,
    Version(u8),
    HeaderSize(u8),
    FrameSize(u32),
    Dimensions(u16, u16),
    /// A slice size of zero or one that runs past the packet, at this
    /// macroblock position.
    SliceSize(usize, usize),
    /// Bad coefficient data in the slice at this macroblock position.
    Slice(usize, usize),
}

/// Which of FFmpeg's inverse DCTs to reproduce. They give the same picture
/// for coefficients an encoder would write and different ones when
/// intermediate values do not fit in 16 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idct {
    /// `ff_simple_idct_int16_8bit` (simple_idct_template.c): the output of
    /// the row pass is cut to its low 16 bits.
    C,
    /// `ff_simple_idct8_sse2` / `_avx` (x86/simple_idct10.asm), which is
    /// what FFmpeg uses on x86-64: the output of the row pass saturates,
    /// and the rounding term of the column pass is added in 16 bits and
    /// wraps.
    X86,
}

/// The fields of the frame header that the slices need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub quant: u8,
    pub interlaced: bool,
}

/// The visible picture: three planes, each `stride * (rows - 1)` bytes plus
/// one visible row.
pub struct Frame<'a> {
    planes: [&'a mut [u8]; 3],
    strides: [usize; 3],
    /// Visible width and height of the luma plane and of a chroma plane.
    dims: [(usize, usize); 3],
    height: usize,
}

impl<'a> Frame<'a> {
    pub fn new(
        planes: [&'a mut [u8]; 3],
        strides: [usize; 3],
        width: usize,
        height: usize,
    ) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let chroma = (width.div_ceil(2), height.div_ceil(2));
        let dims = [(width, height), chroma, chroma];
        let cut = |p: &'a mut [u8], i: usize| -> Option<&'a mut [u8]> {
            let (w, h) = dims[i];
            if strides[i] < w {
                return None;
            }
            <[u8]>::get_mut(p, ..strides[i].checked_mul(h - 1)?.checked_add(w)?)
        };
        let [p0, p1, p2] = planes;
        let planes = [cut(p0, 0)?, cut(p1, 1)?, cut(p2, 2)?];
        Some(Frame { planes, strides, dims, height })
    }

    /// Store an 8x8 block of pixels at (x, y), on every `step`-th row.
    /// Each row is eight bytes in a little-endian word. The parts outside
    /// the visible plane are dropped (the C decoder writes them into row
    /// padding and into rows below the picture).
    #[inline(always)]
    fn put_rows(&mut self, p: usize, x: usize, y: usize, step: usize, rows: &[u64; 8]) {
        let (w, h) = self.dims[p];
        let stride = self.strides[p];
        if x + 8 <= w && y + 7 * step < h {
            let plane = &mut self.planes[p][y * stride + x..];
            for (r, row) in rows.iter().enumerate() {
                if let Some(d) = plane.get_mut(r * step * stride..).and_then(|s| s.first_chunk_mut::<8>()) {
                    *d = row.to_le_bytes();
                }
            }
            return;
        }
        let n = w.saturating_sub(x).min(8);
        for (r, row) in rows.iter().enumerate() {
            let yy = y + r * step;
            if yy >= h || n == 0 {
                break;
            }
            self.planes[p][yy * stride + x..][..n].copy_from_slice(&row.to_le_bytes()[..n]);
        }
    }
}

/// FFmpeg's `GetBitContext` with the bounds-checked reader: the position
/// stops 8 bits after the end of the data, and reads there keep returning
/// the bits found at that position.
struct BitReader<'a> {
    /// The slice and everything after it in the packet. Past the end of
    /// the packet FFmpeg requires zero bytes.
    buf: &'a [u8],
    /// The slice itself.
    data: &'a [u8],
    index: usize,
    size_in_bits: usize,
    size_in_bits_plus8: usize,
}

/// `AVERROR_INVALIDDATA` as the C code sees it in an `unsigned`.
const INVALID_AS_UNSIGNED: u32 = 0xBEBB_B1B7;

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8], size: usize) -> Self {
        BitReader {
            buf,
            data: &buf[..size.min(buf.len())],
            index: 0,
            size_in_bits: size * 8,
            size_in_bits_plus8: size * 8 + 8,
        }
    }

    fn byte(&self, i: usize) -> u8 {
        self.buf.get(i).copied().unwrap_or(0)
    }

    /// `UPDATE_CACHE`: 32 bits starting at the current byte, shifted up to
    /// the current bit.
    fn cache(&self) -> u32 {
        let p = self.index >> 3;
        let word = match self.buf.get(p..).and_then(|s| s.first_chunk::<4>()) {
            Some(b) => u32::from_be_bytes(*b),
            None => u32::from_be_bytes([self.byte(p), self.byte(p + 1), self.byte(p + 2), self.byte(p + 3)]),
        };
        word << (self.index & 7)
    }

    fn skip(&mut self, n: usize) {
        self.index = self.size_in_bits_plus8.min(self.index + n);
    }

    fn bits_left(&self) -> i64 {
        self.size_in_bits as i64 - self.index as i64
    }

    /// 1 to 25 bits.
    fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.cache() >> (32 - n);
        self.skip(n as usize);
        v
    }

    fn get_bits1(&mut self) -> u32 {
        let bit = (self.byte(self.index >> 3) << (self.index & 7)) >> 7;
        if self.index < self.size_in_bits_plus8 {
            self.index += 1;
        }
        u32::from(bit)
    }

    /// `get_unary(gb, 1, 31)`: the number of 0 bits before a 1, at most 31.
    fn get_unary(&mut self) -> u32 {
        let mut i = 0;
        while i < 31 && self.get_bits1() != 1 {
            i += 1;
        }
        i
    }

    /// `get_ue_golomb`, as converted to `unsigned` by the caller: codes up
    /// to 8190, or the error value for a longer run of zeros.
    fn get_ue_golomb(&mut self) -> u32 {
        let buf = self.cache();
        if buf >= 1 << 27 {
            let len = 2 * buf.leading_zeros() + 1;
            self.skip(len as usize);
            (buf >> (32 - len)) - 1
        } else {
            let log = 2 * (31 - (buf | 1).leading_zeros() as i32) - 31;
            self.skip((32 - log) as usize);
            if log < 7 {
                return INVALID_AS_UNSIGNED;
            }
            (buf >> log) - 1
        }
    }

    /// `GET_CODE`, read the way the C code does it.
    #[cold]
    fn get_code_near_end(&mut self, golomb: bool, add_bits: u32) -> u32 {
        let mut val = if golomb { self.get_ue_golomb() } else { self.get_unary() };
        if add_bits != 0 {
            val = (val << add_bits).wrapping_add(self.get_bits(add_bits));
        }
        val
    }

    /// `GET_CODE`: a unary or Exp-Golomb code followed by `add_bits` bits.
    /// A result of 0x10000 or more is an error.
    ///
    /// While eight bytes of the slice are left, the whole code (at most
    /// 38 bits) is taken from one 64-bit word and the position cannot
    /// reach the end. After that it is read as in C, step by step.
    #[inline(always)]
    fn get_code(&mut self, golomb: bool, add_bits: u32) -> u32 {
        let Some(c) = self.data.get(self.index >> 3..).and_then(|s| s.first_chunk::<8>()) else {
            return self.get_code_near_end(golomb, add_bits);
        };
        let w = u64::from_be_bytes(*c) << (self.index & 7);
        let zeros = w.leading_zeros();
        let (hi, len) = if golomb {
            if zeros > 12 {
                return INVALID_AS_UNSIGNED;
            }
            let len = 2 * zeros + 1;
            ((w >> (64 - len)) as u32 - 1, len)
        } else {
            let zeros = zeros.min(31);
            (zeros, zeros + u32::from(zeros < 31))
        };
        let lo = ((w << len) >> 1 >> (63 - add_bits)) as u32;
        self.index += (len + add_bits) as usize;
        (hi << add_bits) + lo
    }
}

/// `aic_decode_coeffs`: one band of a slice. `place[i]` is where the
/// coefficient with coding index i goes in its macroblock.
fn decode_coeffs(gb: &mut BitReader<'_>, dst: &mut [i16], place: &[u16], slice_width: usize) -> Option<()> {
    let num_coeffs = place.len();

    if gb.bits_left() < 5 {
        return None;
    }
    let has_skips = gb.get_bits1() != 0;
    let coeff_type = gb.get_bits1() != 0;
    let coeff_bits = gb.get_bits(3);

    if has_skips {
        let skip_type = gb.get_bits1() != 0;
        let skip_bits = gb.get_bits(3);

        for mb in dst.as_chunks_mut::<MB_COEFFS>().0.iter_mut().take(slice_width) {
            let mut idx = usize::MAX; // -1
            loop {
                let val = gb.get_code(skip_type, skip_bits);
                if val >= 0x10000 {
                    return None;
                }
                idx = idx.wrapping_add(val as usize + 1);
                let Some(&at) = place.get(idx) else {
                    break;
                };
                let val = gb.get_code(coeff_type, coeff_bits).wrapping_add(1);
                if val >= 0x10000 {
                    return None;
                }
                mb[usize::from(at)] = val as i16;
                if idx >= num_coeffs - 1 {
                    break;
                }
            }
        }
    } else {
        for mb in dst.as_chunks_mut::<MB_COEFFS>().0.iter_mut().take(slice_width) {
            for &at in place {
                let val = gb.get_code(coeff_type, coeff_bits);
                if val >= 0x10000 {
                    return None;
                }
                mb[usize::from(at)] = val as i16;
            }
        }
    }
    Some(())
}

/// The frame's quantiser times the quantisation matrix.
struct Factors {
    natural: [i32; 64],
    /// The same, column after column, as the coefficients are stored.
    transposed: [[i16; 8]; 8],
}

impl Factors {
    fn new(quant: u8) -> Self {
        let mut f = Factors { natural: [0; 64], transposed: [[0; 8]; 8] };
        for (i, &m) in QUANT_MATRIX.iter().enumerate() {
            let v = i16::from(quant) * i16::from(m); // at most 255 * 83
            f.natural[i] = i32::from(v);
            f.transposed[i % 8][i / 8] = v;
        }
        f
    }
}

/// `unquant_block`. Bit 0 of a coded value is the sign; `factors` is the
/// frame's quantiser times the quantisation matrix.
fn unquant_block(block: &mut [i16; 64], factors: &[i32; 64]) {
    for (b, &f) in block.iter_mut().zip(factors) {
        let val = i32::from(*b as u16);
        let sign = val & 1;
        *b = ((((val >> 1) ^ -sign) * f >> 4) + sign) as i16;
    }
}

const W1: i32 = 22725;
const W2: i32 = 21407;
const W3: i32 = 19266;
const W4: i32 = 16383;
const W5: i32 = 12873;
const W6: i32 = 8867;
const W7: i32 = 4520;
const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;
const DC_SHIFT: u32 = 3;

/// The eight sums of one IDCT pass, before the shift: `e[i] + o[i]` are
/// outputs 0 to 3 and `e[i] - o[i]` outputs 7 to 4. All 32-bit arithmetic
/// wraps, as it does in C (unsigned) and in the assembly.
fn idct_1d(s: [i32; 8], a: i32) -> ([i32; 4], [i32; 4]) {
    let m = |w: i32, x: i32| w.wrapping_mul(x);
    let (a0, a1, a2, a3) = (
        a.wrapping_add(m(W2, s[2])).wrapping_add(m(W4, s[4])).wrapping_add(m(W6, s[6])),
        a.wrapping_add(m(W6, s[2])).wrapping_sub(m(W4, s[4])).wrapping_sub(m(W2, s[6])),
        a.wrapping_sub(m(W6, s[2])).wrapping_sub(m(W4, s[4])).wrapping_add(m(W2, s[6])),
        a.wrapping_sub(m(W2, s[2])).wrapping_add(m(W4, s[4])).wrapping_sub(m(W6, s[6])),
    );
    let (b0, b1, b2, b3) = (
        m(W1, s[1]).wrapping_add(m(W3, s[3])).wrapping_add(m(W5, s[5])).wrapping_add(m(W7, s[7])),
        m(W3, s[1]).wrapping_sub(m(W7, s[3])).wrapping_sub(m(W1, s[5])).wrapping_sub(m(W5, s[7])),
        m(W5, s[1]).wrapping_sub(m(W1, s[3])).wrapping_add(m(W7, s[5])).wrapping_add(m(W3, s[7])),
        m(W7, s[1]).wrapping_sub(m(W5, s[3])).wrapping_add(m(W3, s[5])).wrapping_sub(m(W1, s[7])),
    );
    ([a0, a1, a2, a3], [b0, b1, b2, b3])
}

/// `idctRowCondDC`.
fn idct_row(row: &mut [i16; 8], kind: Idct) {
    if row[1..].iter().all(|&c| c == 0) {
        // A row with only its first coefficient is not multiplied by W4 but
        // shifted, which is not the same thing: W4 is 16383, not 16384.
        *row = [row[0].wrapping_shl(DC_SHIFT); 8];
        return;
    }
    let s = row.map(i32::from);
    let (e, o) = idct_1d(s, W4.wrapping_mul(s[0]).wrapping_add(1 << (ROW_SHIFT - 1)));
    let pack = |v: i32| match kind {
        Idct::C => v as i16,
        Idct::X86 => v.clamp(-32768, 32767) as i16,
    };
    for i in 0..4 {
        row[i] = pack(e[i].wrapping_add(o[i]) >> ROW_SHIFT);
        row[7 - i] = pack(e[i].wrapping_sub(o[i]) >> ROW_SHIFT);
    }
}

/// `idctSparseCol` on column `c`.
fn idct_col(block: &mut [i16; 64], c: usize, kind: Idct) {
    let s: [i32; 8] = core::array::from_fn(|i| i32::from(block[8 * i + c]));
    // The rounding term, (1 << (COL_SHIFT - 1)) / W4 = 32, goes on the
    // first coefficient.
    let first = match kind {
        Idct::C => s[0] + 32,
        Idct::X86 => i32::from(block[c].wrapping_add(32)),
    };
    let (e, o) = idct_1d(s, W4.wrapping_mul(first));
    for i in 0..4 {
        block[8 * i + c] = (e[i].wrapping_add(o[i]) >> COL_SHIFT) as i16;
        block[8 * (7 - i) + c] = (e[i].wrapping_sub(o[i]) >> COL_SHIFT) as i16;
    }
}

/// The 8-bit "simple" IDCT, in place.
fn idct(block: &mut [i16; 64], kind: Idct) {
    for row in block.as_chunks_mut::<8>().0 {
        idct_row(row, kind);
    }
    for c in 0..8 {
        idct_col(block, c, kind);
    }
}

/// One block the portable way: dequantise, transform, and clamp to
/// pixels. `coeffs` is the block as it is stored, transposed. Each row of
/// the result is eight pixels in a little-endian word.
fn block_scalar(coeffs: &[i16; 64], factors: &Factors, kind: Idct) -> [u64; 8] {
    let mut block = [0i16; 64];
    for (i, b) in block.iter_mut().enumerate() {
        *b = coeffs[i % 8 * 8 + i / 8];
    }
    unquant_block(&mut block, &factors.natural);
    idct(&mut block, kind);
    let mut rows = [0u64; 8];
    for (row, src) in rows.iter_mut().zip(block.as_chunks::<8>().0) {
        // put_signed_pixels_clamped
        *row = u64::from_le_bytes(src.map(|s| (s.clamp(-128, 127) + 128) as u8));
    }
    rows
}

/// The same with SSE2. It follows FFmpeg's assembly
/// (x86/simple_idct10_template.asm): eight 1-D transforms at a time, the
/// 16-bit values multiplied in pairs by `pmaddwd`.
///
/// The intrinsics used take and return values, not pointers, so safe code
/// may call them inside a function compiled for SSE2.
#[cfg(target_arch = "x86_64")]
mod sse2 {
    use super::{Factors, Idct, COL_SHIFT, DC_SHIFT, ROW_SHIFT, W1, W2, W3, W4, W5, W6, W7};
    use core::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "sse2")]
    fn load(v: &[i16; 8]) -> __m128i {
        _mm_set_epi16(v[7], v[6], v[5], v[4], v[3], v[2], v[1], v[0])
    }

    /// The quantisation factors of a frame, one vector per column.
    pub struct Quant([__m128i; 8]);

    impl Quant {
        #[inline]
        #[target_feature(enable = "sse2")]
        pub fn new(f: &Factors) -> Self {
            Quant(f.transposed.map(|c| load(&c)))
        }
    }

    /// `unquant_block` in 16-bit lanes: the product is at most 31 bits,
    /// and bits 4 to 19 of it are wanted.
    #[inline]
    #[target_feature(enable = "sse2")]
    fn unquant(v: __m128i, f: __m128i) -> __m128i {
        let sign = _mm_and_si128(v, _mm_set1_epi16(1));
        let t = _mm_xor_si128(_mm_srli_epi16::<1>(v), _mm_sub_epi16(_mm_setzero_si128(), sign));
        let lo = _mm_mullo_epi16(t, f);
        let hi = _mm_mulhi_epi16(t, f);
        _mm_add_epi16(_mm_or_si128(_mm_srli_epi16::<4>(lo), _mm_slli_epi16::<12>(hi)), sign)
    }

    /// A pair of multipliers for `pmaddwd`.
    #[inline]
    #[target_feature(enable = "sse2")]
    fn pair(a: i32, b: i32) -> __m128i {
        _mm_set1_epi32((b << 16) | (a & 0xffff))
    }

    /// Eight 1-D transforms: `s[j]` holds coefficient j of each. `round`
    /// is added before the shift. The 32-bit results are narrowed to 16
    /// bits with saturation, or by dropping the high bits if `truncate`.
    #[inline]
    #[target_feature(enable = "sse2")]
    fn idct_1d<const SHIFT: i32>(s: [__m128i; 8], round: __m128i, truncate: bool) -> [__m128i; 8] {
        // One half (four transforms) at a time: the sums a0..a3 and b0..b3
        // of the C code.
        let half = |x02: __m128i, x46: __m128i, x13: __m128i, x57: __m128i| {
            let a = |c02: __m128i, c46: __m128i, sub: bool| {
                let v = _mm_add_epi32(_mm_madd_epi16(x02, c02), round);
                let w = _mm_madd_epi16(x46, c46);
                if sub {
                    _mm_sub_epi32(v, w)
                } else {
                    _mm_add_epi32(v, w)
                }
            };
            let b = |c13: __m128i, c57: __m128i, sub: bool| {
                let v = _mm_madd_epi16(x13, c13);
                let w = _mm_madd_epi16(x57, c57);
                if sub {
                    _mm_sub_epi32(v, w)
                } else {
                    _mm_add_epi32(v, w)
                }
            };
            let a = [
                a(pair(W4, W2), pair(W4, W6), false),
                a(pair(W4, W6), pair(W4, W2), true),
                a(pair(W4, -W6), pair(W4, -W2), true),
                a(pair(W4, -W2), pair(W4, -W6), false),
            ];
            let b = [
                b(pair(W1, W3), pair(W5, W7), false),
                b(pair(W3, -W7), pair(W1, W5), true),
                b(pair(W5, -W1), pair(W7, W3), false),
                b(pair(W7, -W5), pair(W3, -W1), false),
            ];
            let narrow = |v: __m128i| {
                let v = _mm_srai_epi32::<SHIFT>(v);
                if truncate {
                    _mm_srai_epi32::<16>(_mm_slli_epi32::<16>(v))
                } else {
                    v
                }
            };
            let mut sum = [_mm_setzero_si128(); 4];
            let mut diff = [_mm_setzero_si128(); 4];
            for i in 0..4 {
                sum[i] = narrow(_mm_add_epi32(a[i], b[i]));
                diff[i] = narrow(_mm_sub_epi32(a[i], b[i]));
            }
            (sum, diff)
        };
        let (sl, dl) = half(
            _mm_unpacklo_epi16(s[0], s[2]),
            _mm_unpacklo_epi16(s[4], s[6]),
            _mm_unpacklo_epi16(s[1], s[3]),
            _mm_unpacklo_epi16(s[5], s[7]),
        );
        let (sh, dh) = half(
            _mm_unpackhi_epi16(s[0], s[2]),
            _mm_unpackhi_epi16(s[4], s[6]),
            _mm_unpackhi_epi16(s[1], s[3]),
            _mm_unpackhi_epi16(s[5], s[7]),
        );
        [
            _mm_packs_epi32(sl[0], sh[0]),
            _mm_packs_epi32(sl[1], sh[1]),
            _mm_packs_epi32(sl[2], sh[2]),
            _mm_packs_epi32(sl[3], sh[3]),
            _mm_packs_epi32(dl[3], dh[3]),
            _mm_packs_epi32(dl[2], dh[2]),
            _mm_packs_epi32(dl[1], dh[1]),
            _mm_packs_epi32(dl[0], dh[0]),
        ]
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    fn transpose(r: [__m128i; 8]) -> [__m128i; 8] {
        let a = [
            _mm_unpacklo_epi16(r[0], r[1]),
            _mm_unpackhi_epi16(r[0], r[1]),
            _mm_unpacklo_epi16(r[2], r[3]),
            _mm_unpackhi_epi16(r[2], r[3]),
            _mm_unpacklo_epi16(r[4], r[5]),
            _mm_unpackhi_epi16(r[4], r[5]),
            _mm_unpacklo_epi16(r[6], r[7]),
            _mm_unpackhi_epi16(r[6], r[7]),
        ];
        let b = [
            _mm_unpacklo_epi32(a[0], a[2]),
            _mm_unpackhi_epi32(a[0], a[2]),
            _mm_unpacklo_epi32(a[1], a[3]),
            _mm_unpackhi_epi32(a[1], a[3]),
            _mm_unpacklo_epi32(a[4], a[6]),
            _mm_unpackhi_epi32(a[4], a[6]),
            _mm_unpacklo_epi32(a[5], a[7]),
            _mm_unpackhi_epi32(a[5], a[7]),
        ];
        [
            _mm_unpacklo_epi64(b[0], b[4]),
            _mm_unpackhi_epi64(b[0], b[4]),
            _mm_unpacklo_epi64(b[1], b[5]),
            _mm_unpackhi_epi64(b[1], b[5]),
            _mm_unpacklo_epi64(b[2], b[6]),
            _mm_unpackhi_epi64(b[2], b[6]),
            _mm_unpacklo_epi64(b[3], b[7]),
            _mm_unpackhi_epi64(b[3], b[7]),
        ]
    }

    /// See `block_scalar`.
    #[inline]
    #[target_feature(enable = "sse2")]
    pub fn block(coeffs: &[i16; 64], q: &Quant, kind: Idct) -> [u64; 8] {
        let x86 = kind == Idct::X86;
        let (cols, _) = coeffs.as_chunks::<8>();
        let mut s = [_mm_setzero_si128(); 8];
        for j in 0..8 {
            s[j] = unquant(load(&cols[j]), q.0[j]);
        }

        // Rows. Lane r of s[j] is coefficient j of row r. A row with
        // nothing but its first coefficient is not transformed but set to
        // that coefficient times 8.
        let rest = _mm_or_si128(
            _mm_or_si128(_mm_or_si128(s[1], s[2]), _mm_or_si128(s[3], s[4])),
            _mm_or_si128(_mm_or_si128(s[5], s[6]), s[7]),
        );
        let dc_only = _mm_cmpeq_epi16(rest, _mm_setzero_si128());
        let dc = _mm_and_si128(_mm_slli_epi16::<{ DC_SHIFT as i32 }>(s[0]), dc_only);
        let mut t = idct_1d::<{ ROW_SHIFT as i32 }>(s, _mm_set1_epi32(1 << (ROW_SHIFT - 1)), !x86);
        for v in t.iter_mut() {
            *v = _mm_or_si128(_mm_andnot_si128(dc_only, *v), dc);
        }

        // Columns. The rounding term of 32 goes on the first coefficient:
        // the assembly adds it in 16 bits, where it can wrap; C adds it
        // in 32.
        let mut t = transpose(t);
        let round = if x86 {
            t[0] = _mm_add_epi16(t[0], _mm_set1_epi16(32));
            _mm_setzero_si128()
        } else {
            _mm_set1_epi32(32 * W4)
        };
        let t = idct_1d::<{ COL_SHIFT as i32 }>(t, round, false);

        // put_signed_pixels_clamped
        let mut rows = [0u64; 8];
        for i in 0..4 {
            let p = _mm_add_epi8(_mm_packs_epi16(t[2 * i], t[2 * i + 1]), _mm_set1_epi8(-128));
            rows[2 * i] = _mm_cvtsi128_si64(p) as u64;
            rows[2 * i + 1] = _mm_cvtsi128_si64(_mm_unpackhi_epi64(p, p)) as u64;
        }
        rows
    }
}

/// [`block_sse2`] and [`block_portable`] must agree; this is here for the
/// test of that, which needs `unsafe` to call the SSE2 one and so lives in
/// the FFI crate. One block, stored column after column, to eight rows of
/// pixels.
pub fn block_portable(coeffs: &[i16; 64], quant: u8, idct: Idct) -> [u64; 8] {
    block_scalar(coeffs, &Factors::new(quant), idct)
}

/// See [`block_portable`].
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub fn block_sse2(coeffs: &[i16; 64], quant: u8, idct: Idct) -> [u64; 8] {
    sse2::block(coeffs, &sse2::Quant::new(&Factors::new(quant)), idct)
}

/// A decoder for one stream. It holds the coefficient buffer, which the C
/// decoder does not clear completely between slices.
pub struct Decoder {
    mb_width: usize,
    mb_height: usize,
    num_x_slices: usize,
    slice_width: usize,
    idct: Idct,
    /// The coefficients of a slice, 384 per macroblock (see [`place`]).
    slice_data: Vec<i16>,
    /// Whether the luma coefficients in `slice_data` are placed as for an
    /// interlaced frame.
    interlaced_layout: bool,
}

impl Decoder {
    /// `aic_decode_init`.
    pub fn new(width: usize, height: usize, idct: Idct) -> Self {
        let mb_width = width.div_ceil(16);
        let mb_height = height.div_ceil(16);

        let mut num_x_slices = (mb_width + 15) >> 4;
        let mut slice_width = 16;
        for i in 1..mb_width {
            if mb_width % i == 0 && mb_width / i <= 32 {
                slice_width = mb_width / i;
                num_x_slices = i;
                break;
            }
        }

        Decoder {
            mb_width,
            mb_height,
            num_x_slices,
            slice_width,
            idct,
            slice_data: vec![0; slice_width * MB_COEFFS],
            interlaced_layout: false,
        }
    }

    /// Where the slice data starts.
    fn data_offset(&self) -> usize {
        let table = self.num_x_slices.saturating_mul(self.mb_height).saturating_mul(2);
        HDR_SIZE.saturating_add(table).saturating_add(3) & !3
    }

    /// The checks the C decoder makes before it asks for a frame:
    /// the packet holds the header and the slice table, and the header
    /// (`aic_decode_header`) is for a picture of `width` x `height`.
    pub fn check(&self, packet: &[u8], width: usize, height: usize) -> Result<Header, Error> {
        if packet.len() < self.data_offset() {
            return Err(Error::TooSmall);
        }
        let hdr = packet.first_chunk::<HDR_SIZE>().ok_or(Error::TooSmall)?;
        if hdr[0] != 1 {
            return Err(Error::Version(hdr[0]));
        }
        if usize::from(hdr[1]) != HDR_SIZE - 2 {
            return Err(Error::HeaderSize(hdr[1]));
        }
        let frame_size = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]);
        let w = u16::from_be_bytes([hdr[6], hdr[7]]);
        let h = u16::from_be_bytes([hdr[8], hdr[9]]);
        if frame_size as usize > packet.len() {
            return Err(Error::FrameSize(frame_size));
        }
        if usize::from(w) != width || usize::from(h) != height {
            return Err(Error::Dimensions(w, h));
        }
        Ok(Header { quant: hdr[15], interlaced: hdr[16] >> 4 == 3 })
    }

    /// `aic_decode_frame` after the frame was allocated. On an error the
    /// picture is partly written; FFmpeg then discards it.
    pub fn decode(&mut self, packet: &[u8], frame: &mut Frame<'_>) -> Result<(), Error> {
        self.decode_with(packet, frame, |data, slice_width, at, frame, factors, kind| {
            reconstruct(data, slice_width, at, frame, |c| block_scalar(c, factors, kind))
        })
    }

    /// [`Decoder::decode`] with SSE2. Same output.
    ///
    /// Safe code can only call this from another function compiled for
    /// SSE2; anyone else has to use `unsafe`, even though every x86-64 CPU
    /// has SSE2.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse2")]
    pub fn decode_sse2(&mut self, packet: &[u8], frame: &mut Frame<'_>) -> Result<(), Error> {
        self.decode_with(packet, frame, |data, slice_width, at, frame, factors, kind| {
            reconstruct_sse2(data, slice_width, at, frame, factors, kind)
        })
    }

    #[inline(always)]
    fn decode_with(
        &mut self,
        packet: &[u8],
        frame: &mut Frame<'_>,
        rebuild: impl Fn(&[i16], usize, &Position, &mut Frame<'_>, &Factors, Idct),
    ) -> Result<(), Error> {
        let hdr = self.check(packet, frame.dims[0].0, frame.dims[0].1)?;
        let mut off = self.data_offset();
        let factors = Factors::new(hdr.quant);

        let mut sizes = packet[HDR_SIZE..].chunks_exact(2);
        for y in 0..self.mb_height {
            for x in (0..self.mb_width).step_by(self.slice_width) {
                let size = sizes.next().map_or(0, |s| usize::from(u16::from_le_bytes([s[0], s[1]]))) * 4;
                if size + off > packet.len() || size == 0 {
                    return Err(Error::SliceSize(x, y));
                }
                let (slice_width, at) = self
                    .decode_slice(&hdr, x, y, &packet[off..], size, frame.height)
                    .ok_or(Error::Slice(x, y))?;
                rebuild(&self.slice_data, slice_width, &at, frame, &factors, self.idct);
                off += size;
            }
        }
        Ok(())
    }

    /// What the C decoder's `memset` does for a slice narrower than the
    /// full slice width (the last slice of a row, where the width in
    /// macroblocks has no suitable divisor; every slice of a picture up
    /// to 16 pixels wide). It clears `slice_width * 384` values at the
    /// start of its buffer, where the bands follow one another, each
    /// `self.slice_width` macroblocks long. That covers the luma band of
    /// this slice, but only part of the others, or nothing: what is not
    /// cleared keeps the coefficients of the slice before.
    #[cold]
    fn clear_narrow(&mut self, slice_width: usize) {
        for band in 0..4 {
            let n = NUM_BAND_COEFFS[band];
            let cleared = (slice_width * BAND_COEFFS)
                .saturating_sub(self.slice_width * BAND_OFF[band])
                .min(slice_width * n);
            for i in 0..cleared {
                self.slice_data[i / n * MB_COEFFS + place(self.interlaced_layout, band, i % n)] = 0;
            }
        }
    }

    /// Move the luma coefficients to where a frame of the other kind
    /// (interlaced or not) has them. Only needed where coefficients
    /// survive from one slice to the next, see `clear_narrow`.
    #[cold]
    fn change_layout(&mut self, interlaced: bool) {
        for mb in self.slice_data.as_chunks_mut::<MB_COEFFS>().0 {
            let old = *mb;
            for band in [0, 2] {
                for pos in 0..NUM_BAND_COEFFS[band] {
                    mb[place(interlaced, band, pos)] = old[place(self.interlaced_layout, band, pos)];
                }
            }
        }
        self.interlaced_layout = interlaced;
    }

    /// The first half of `aic_decode_slice`: the coefficients. `src`
    /// starts at the slice and runs to the end of the packet. Returns the
    /// number of macroblocks and where they go.
    fn decode_slice(
        &mut self,
        hdr: &Header,
        mb_x: usize,
        mb_y: usize,
        src: &[u8],
        src_size: usize,
        height: usize,
    ) -> Option<(usize, Position)> {
        let slice_width = self.slice_width.min(self.mb_width - mb_x);
        let last_row = mb_y != 0 && mb_y == self.mb_height - 1;
        let (y_pos, c_pos) = if last_row {
            // The last row is placed against the bottom of the picture and
            // overlaps the row above it.
            (height.saturating_sub(16), height.div_ceil(2).saturating_sub(8))
        } else {
            (mb_y * 16, mb_y * 8)
        };
        let mut gb = BitReader::new(src, src_size);

        if slice_width == self.slice_width {
            self.slice_data.fill(0);
            self.interlaced_layout = hdr.interlaced;
        } else {
            if self.interlaced_layout != hdr.interlaced {
                self.change_layout(hdr.interlaced);
            }
            self.clear_narrow(slice_width);
        }
        for place in place_tables(hdr.interlaced) {
            decode_coeffs(&mut gb, &mut self.slice_data, place, slice_width)?;
        }
        Some((slice_width, Position { mb_x, y_pos, c_pos, interlaced: hdr.interlaced }))
    }
}

/// Where a slice goes in the picture.
struct Position {
    mb_x: usize,
    y_pos: usize,
    c_pos: usize,
    interlaced: bool,
}

/// The second half of `aic_decode_slice`: turn the coefficients of each
/// macroblock into four luma and two chroma blocks. In an interlaced
/// frame the luma blocks are fields: two on the even rows, two on the odd
/// rows.
#[inline(always)]
fn reconstruct(
    data: &[i16],
    slice_width: usize,
    at: &Position,
    frame: &mut Frame<'_>,
    block: impl Fn(&[i16; 64]) -> [u64; 8],
) {
    for (mb, coeffs) in data.as_chunks::<MB_COEFFS>().0.iter().take(slice_width).enumerate() {
        let x = (at.mb_x + mb) * 16;
        let (blocks, _) = coeffs.as_chunks::<64>();
        for (blk, c) in blocks.iter().enumerate() {
            let rows = block(c);
            if blk >= 4 {
                frame.put_rows(blk - 3, x / 2, at.c_pos, 1, &rows);
            } else if !at.interlaced {
                frame.put_rows(0, x + (blk & 1) * 8, at.y_pos + (blk >> 1) * 8, 1, &rows);
            } else {
                frame.put_rows(0, x + (blk & 1) * 8, at.y_pos + (blk >> 1), 2, &rows);
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
fn reconstruct_sse2(
    data: &[i16],
    slice_width: usize,
    at: &Position,
    frame: &mut Frame<'_>,
    factors: &Factors,
    kind: Idct,
) {
    let q = sse2::Quant::new(factors);
    reconstruct(data, slice_width, at, frame, |c| sse2::block(c, &q, kind));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits most significant first.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        n: usize,
    }

    impl BitWriter {
        fn put(&mut self, bits: u32, value: u32) {
            for i in (0..bits).rev() {
                if self.n % 8 == 0 {
                    self.bytes.push(0);
                }
                *self.bytes.last_mut().unwrap() |= ((value.checked_shr(i).unwrap_or(0) & 1) as u8) << (7 - self.n % 8);
                self.n += 1;
            }
        }
        fn unary(&mut self, v: u32) {
            self.put(v, 0);
            if v < 31 {
                self.put(1, 1);
            }
        }
        fn golomb(&mut self, v: u32) {
            let len = 32 - (v + 1).leading_zeros();
            self.put(len - 1, 0);
            self.put(len, v + 1);
        }
    }

    /// A band in which every coefficient is skipped: one unary skip code
    /// of 8 with three extra bits per macroblock, 64 or more.
    fn empty_band(w: &mut BitWriter, mbs: usize) {
        w.put(1, 1); // has_skips
        w.put(1, 0); // coefficients: unary
        w.put(3, 0);
        w.put(1, 0); // skips: unary
        w.put(3, 5);
        for _ in 0..mbs {
            w.unary(8);
            w.put(5, 0);
        }
    }

    /// A band with every coefficient coded as Exp-Golomb plus `bits` bits.
    fn full_band(w: &mut BitWriter, coeffs: &[u16], bits: u32) {
        w.put(1, 0);
        w.put(1, 1);
        w.put(3, bits);
        for &c in coeffs {
            w.golomb(u32::from(c) >> bits);
            w.put(bits, u32::from(c) & ((1 << bits) - 1));
        }
    }

    fn packet(width: u16, height: u16, quant: u8, interlaced: bool, slices: &[Vec<u8>]) -> Vec<u8> {
        let mut p = vec![0u8; HDR_SIZE];
        p[0] = 1;
        p[1] = 22;
        p[6..8].copy_from_slice(&width.to_be_bytes());
        p[8..10].copy_from_slice(&height.to_be_bytes());
        p[15] = quant;
        p[16] = if interlaced { 0x30 } else { 0 };
        for s in slices {
            p.extend_from_slice(&(s.len().div_ceil(4) as u16).to_le_bytes());
        }
        p.resize(p.len().next_multiple_of(4), 0);
        for s in slices {
            p.extend_from_slice(s);
            p.resize(p.len().next_multiple_of(4), 0);
        }
        let n = p.len() as u32;
        p[2..6].copy_from_slice(&n.to_be_bytes());
        p
    }

    struct Picture {
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        w: usize,
        h: usize,
    }

    fn decode(dec: &mut Decoder, pkt: &[u8], w: usize, h: usize) -> Result<Picture, Error> {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut pic = Picture { y: vec![7; w * h], u: vec![7; cw * ch], v: vec![7; cw * ch], w, h };
        dec.check(pkt, w, h)?;
        let mut frame = Frame::new([&mut pic.y, &mut pic.u, &mut pic.v], [w, cw, cw], w, h).unwrap();
        dec.decode(pkt, &mut frame)?;
        Ok(pic)
    }

    #[test]
    fn codes() {
        let mut w = BitWriter::default();
        for v in [0, 1, 2, 14, 15, 30, 31, 254, 8190] {
            w.golomb(v);
        }
        for v in [0, 1, 30, 31] {
            w.unary(v);
        }
        w.put(7, 0x55);
        let mut gb = BitReader::new(&w.bytes, w.bytes.len());
        for v in [0, 1, 2, 14, 15, 30, 31, 254, 8190] {
            assert_eq!(gb.get_ue_golomb(), v);
        }
        for v in [0, 1, 30, 31] {
            assert_eq!(gb.get_unary(), v);
        }
        assert_eq!(gb.get_bits(7), 0x55);
        // 13 or more zeros is not a code.
        let mut gb = BitReader::new(&[0, 4, 0, 0, 0, 0], 6);
        assert_eq!(gb.get_ue_golomb(), INVALID_AS_UNSIGNED);
    }

    #[test]
    fn reader_stops_after_the_end() {
        // Two bytes of slice; the reader may go 8 bits further and then
        // keeps returning what it finds there.
        let data = [0xff, 0xff, 0x00, 0xa5, 0xff];
        let mut gb = BitReader::new(&data, 2);
        assert_eq!(gb.get_bits(16), 0xffff);
        assert_eq!(gb.bits_left(), 0);
        assert_eq!(gb.get_bits(8), 0);
        assert_eq!(gb.index, 24);
        assert_eq!(gb.get_bits(8), 0xa5);
        assert_eq!(gb.get_bits(8), 0xa5);
        assert_eq!(gb.get_bits1(), 1);
        assert_eq!(gb.get_bits1(), 1);
        assert_eq!(gb.index, 24);
    }

    #[test]
    fn empty_frame_is_grey() {
        for (w, h) in [(16, 16), (1, 1), (17, 17), (33, 5)] {
            let mut dec = Decoder::new(w, h, Idct::X86);
            let mut slices = Vec::new();
            for _ in 0..dec.mb_height * dec.num_x_slices {
                let mut bw = BitWriter::default();
                for _ in 0..4 {
                    empty_band(&mut bw, dec.slice_width.min(dec.mb_width));
                }
                slices.push(bw.bytes);
            }
            let pkt = packet(w as u16, h as u16, 10, false, &slices);
            let pic = decode(&mut dec, &pkt, w, h).unwrap();
            assert!(pic.y.iter().chain(&pic.u).chain(&pic.v).all(|&p| p == 128), "{w}x{h}");
        }
    }

    #[test]
    fn dc_only_block() {
        // One macroblock; luma DC of 2 * 6 = 12 (sign bit clear), which
        // with quantiser 16 becomes 6 * 16 * 8 >> 4 = 48, and after the
        // IDCT 48 * 8 * 16383 >> 20 = 6 (rounded): 128 + 6.
        let mut dec = Decoder::new(16, 16, Idct::X86);
        let mut luma = [0u16; 64];
        for blk in 0..4 {
            // The first base coefficient of each block, in coding order.
            luma[C_SCAN.iter().position(|&s| usize::from(s) == blk * 16).unwrap()] = 12;
        }
        let mut bw = BitWriter::default();
        full_band(&mut bw, &luma, 2);
        for _ in 0..3 {
            empty_band(&mut bw, 1);
        }
        let pkt = packet(16, 16, 16, false, &[bw.bytes]);
        let pic = decode(&mut dec, &pkt, 16, 16).unwrap();
        assert!(pic.y.iter().all(|&p| p == 134), "{:?}", &pic.y[..16]);
        assert!(pic.u.iter().chain(&pic.v).all(|&p| p == 128));
        assert_eq!((pic.w, pic.h), (16, 16));
    }

    #[test]
    fn header_checks() {
        let dec = Decoder::new(16, 16, Idct::X86);
        let mut bw = BitWriter::default();
        for _ in 0..4 {
            empty_band(&mut bw, 1);
        }
        let good = packet(16, 16, 1, false, &[bw.bytes]);
        assert_eq!(dec.check(&good, 16, 16), Ok(Header { quant: 1, interlaced: false }));
        assert_eq!(dec.check(&good[..27], 16, 16), Err(Error::TooSmall));
        assert_eq!(dec.check(&good, 16, 17), Err(Error::Dimensions(16, 16)));
        let mut bad = good.clone();
        bad[0] = 2;
        assert_eq!(dec.check(&bad, 16, 16), Err(Error::Version(2)));
        bad = good.clone();
        bad[1] = 24;
        assert_eq!(dec.check(&bad, 16, 16), Err(Error::HeaderSize(24)));
        bad = good.clone();
        bad[5] += 1;
        assert_eq!(dec.check(&bad, 16, 16), Err(Error::FrameSize(good.len() as u32 + 1)));
    }

    #[test]
    fn slice_errors() {
        let mut dec = Decoder::new(16, 16, Idct::X86);
        let mut bw = BitWriter::default();
        for _ in 0..4 {
            empty_band(&mut bw, 1);
        }
        let good = packet(16, 16, 1, false, &[bw.bytes]);
        let mut bad = good.clone();
        bad[24] = 0;
        assert_eq!(decode(&mut dec, &bad, 16, 16).err(), Some(Error::SliceSize(0, 0)));
        bad[24] = good[24] + 1;
        assert_eq!(decode(&mut dec, &bad, 16, 16).err(), Some(Error::SliceSize(0, 0)));
        // Golomb skips and a slice of zeros: the first code is invalid.
        let mut bw = BitWriter::default();
        bw.put(9, 0b1_0_000_1_000);
        bw.put(64, 0);
        let pkt = packet(16, 16, 1, false, &[bw.bytes]);
        assert_eq!(decode(&mut dec, &pkt, 16, 16).err(), Some(Error::Slice(0, 0)));
    }

    /// The C decoder clears only part of its coefficient buffer when a
    /// slice is narrower than the full slice width, which is always the
    /// case for pictures up to 16 pixels wide. Skipped coefficients of the
    /// chroma and extension bands then keep the values of the previous
    /// slice, here of the previous frame.
    #[test]
    fn stale_coefficients_are_kept() {
        let (w, h) = (16, 16);
        let mut chroma = [0u16; 32];
        chroma[0] = 40;
        let mut first = BitWriter::default();
        empty_band(&mut first, 1);
        full_band(&mut first, &chroma, 0);
        empty_band(&mut first, 1);
        empty_band(&mut first, 1);
        let first = packet(w, h, 16, false, &[first.bytes]);
        let mut second = BitWriter::default();
        for _ in 0..4 {
            empty_band(&mut second, 1);
        }
        let second = packet(w, h, 16, false, &[second.bytes]);

        let mut dec = Decoder::new(16, 16, Idct::X86);
        let alone = decode(&mut dec, &second, 16, 16).unwrap();
        assert!(alone.u.iter().all(|&p| p == 128));
        let a = decode(&mut dec, &first, 16, 16).unwrap();
        assert!(a.u.iter().all(|&p| p == 148), "{:?}", &a.u[..8]);
        let after = decode(&mut dec, &second, 16, 16).unwrap();
        assert_eq!(after.u, a.u);
        assert!(after.y.iter().all(|&p| p == 128));

        // A picture 32 wide has one slice of full width: nothing is kept.
        let mut chroma = [0u16; 64];
        chroma[0] = 40;
        let mut first = BitWriter::default();
        empty_band(&mut first, 2);
        full_band(&mut first, &chroma, 0);
        empty_band(&mut first, 2);
        empty_band(&mut first, 2);
        let first = packet(32, 16, 16, false, &[first.bytes]);
        let mut second = BitWriter::default();
        for _ in 0..4 {
            empty_band(&mut second, 2);
        }
        let second = packet(32, 16, 16, false, &[second.bytes]);
        let mut dec = Decoder::new(32, 16, Idct::X86);
        decode(&mut dec, &first, 32, 16).unwrap();
        let after = decode(&mut dec, &second, 32, 16).unwrap();
        assert!(after.u.iter().all(|&p| p == 128));
    }

    /// The two IDCTs agree on ordinary coefficients and differ on large
    /// ones.
    #[test]
    fn idct_kinds() {
        let mut block = [0i16; 64];
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 48) as i16
        };
        for _ in 0..2000 {
            for b in block.iter_mut() {
                *b = next() >> 6;
            }
            let (mut c, mut x) = (block, block);
            idct(&mut c, Idct::C);
            idct(&mut x, Idct::X86);
            assert_eq!(c, x);
        }
        // Row 0 sums to more than 32767: the assembly saturates.
        let mut block = [0i16; 64];
        block[0] = 32767;
        block[1] = 32767;
        let (mut c, mut x) = (block, block);
        idct(&mut c, Idct::C);
        idct(&mut x, Idct::X86);
        assert_ne!(c, x);
    }

    #[test]
    fn interlaced_rows_alternate() {
        // Luma DC only in the first block of the macroblock: in an
        // interlaced frame it covers the even rows of the left half.
        let mut dec = Decoder::new(16, 16, Idct::X86);
        let mut luma = [0u16; 64];
        luma[0] = 12; // y_scan[0] = 0
        let mut bw = BitWriter::default();
        full_band(&mut bw, &luma, 2);
        for _ in 0..3 {
            empty_band(&mut bw, 1);
        }
        let pkt = packet(16, 16, 16, true, &[bw.bytes]);
        let pic = decode(&mut dec, &pkt, 16, 16).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                let want = if y % 2 == 0 && x < 8 { 134 } else { 128 };
                assert_eq!(pic.y[y * 16 + x], want, "({x},{y})");
            }
        }
    }
}
