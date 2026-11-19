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

static QUANT_MATRIX: [u8; 64] = [
     8, 16, 19, 22, 22, 26, 26, 27,
    16, 16, 22, 22, 26, 27, 27, 29,
    19, 22, 26, 26, 27, 29, 29, 35,
    22, 24, 27, 27, 29, 32, 34, 38,
    26, 27, 29, 29, 32, 35, 38, 46,
    27, 29, 34, 34, 35, 40, 46, 56,
    29, 34, 34, 37, 40, 48, 56, 69,
    34, 37, 38, 40, 48, 58, 69, 83,
];

static Y_SCAN: [u8; 64] = [
     0,  4,  1,  2,  5,  8, 12,  9,
     6,  3,  7, 10, 13, 14, 11, 15,
    47, 43, 46, 45, 42, 39, 35, 38,
    41, 44, 40, 37, 34, 33, 36, 32,
    16, 20, 17, 18, 21, 24, 28, 25,
    22, 19, 23, 26, 29, 30, 27, 31,
    63, 59, 62, 61, 58, 55, 51, 54,
    57, 60, 56, 53, 50, 49, 52, 48,
];

static Y_EXT_SCAN: [u8; 192] = [
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

static C_SCAN: [u8; 64] = [
     0,  4,  1,  2,  5,  8, 12,  9,
     6,  3,  7, 10, 13, 14, 11, 15,
    31, 27, 30, 29, 26, 23, 19, 22,
    25, 28, 24, 21, 18, 17, 20, 16,
    32, 36, 33, 34, 37, 40, 44, 41,
    38, 35, 39, 42, 45, 46, 43, 47,
    63, 59, 62, 61, 58, 55, 51, 54,
    57, 60, 56, 53, 50, 49, 52, 48,
];

static C_EXT_SCAN: [u8; 192] = [
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

/// `aic_scan`: indexed by band, or by `band | 1` for progressive frames.
static SCANS: [&[u8]; 4] = [&Y_SCAN, &C_SCAN, &Y_EXT_SCAN, &C_EXT_SCAN];

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

    /// `put_signed_pixels_clamped`: store an 8x8 block at (x, y), on every
    /// `step`-th row. The parts outside the visible plane are dropped (the
    /// C decoder writes them into row padding and into rows below the
    /// picture).
    fn put_block(&mut self, p: usize, x: usize, y: usize, step: usize, block: &[i16; 64]) {
        let (w, h) = self.dims[p];
        let stride = self.strides[p];
        let n = w.saturating_sub(x).min(8);
        if n == 0 {
            return;
        }
        for (r, row) in block.chunks_exact(8).enumerate() {
            let yy = y + r * step;
            if yy >= h {
                break;
            }
            let dst = &mut self.planes[p][yy * stride + x..][..n];
            for (d, &s) in dst.iter_mut().zip(row) {
                *d = (s.clamp(-128, 127) + 128) as u8;
            }
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
    index: usize,
    size_in_bits: usize,
    size_in_bits_plus8: usize,
}

/// `AVERROR_INVALIDDATA` as the C code sees it in an `unsigned`.
const INVALID_AS_UNSIGNED: u32 = 0xBEBB_B1B7;

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8], size: usize) -> Self {
        BitReader { buf, index: 0, size_in_bits: size * 8, size_in_bits_plus8: size * 8 + 8 }
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

    /// `GET_CODE`.
    fn get_code(&mut self, golomb: bool, add_bits: u32) -> u32 {
        let mut val = if golomb { self.get_ue_golomb() } else { self.get_unary() };
        if add_bits != 0 {
            val = (val << add_bits).wrapping_add(self.get_bits(add_bits));
        }
        val
    }
}

/// `aic_decode_coeffs`: one band of a slice into `dst`, `num_coeffs` values
/// per macroblock.
fn decode_coeffs(
    gb: &mut BitReader<'_>,
    dst: &mut [i16],
    band: usize,
    slice_width: usize,
    force_chroma: bool,
) -> Option<()> {
    let num_coeffs = NUM_BAND_COEFFS[band];
    let scan = SCANS[band | usize::from(force_chroma)];

    if gb.bits_left() < 5 {
        return None;
    }
    let has_skips = gb.get_bits1() != 0;
    let coeff_type = gb.get_bits1() != 0;
    let coeff_bits = gb.get_bits(3);

    if has_skips {
        let skip_type = gb.get_bits1() != 0;
        let skip_bits = gb.get_bits(3);

        for mb in dst.chunks_exact_mut(num_coeffs).take(slice_width) {
            let mut idx = usize::MAX; // -1
            loop {
                let val = gb.get_code(skip_type, skip_bits);
                if val >= 0x10000 {
                    return None;
                }
                idx = idx.wrapping_add(val as usize + 1);
                if idx >= num_coeffs {
                    break;
                }
                let val = gb.get_code(coeff_type, coeff_bits).wrapping_add(1);
                if val >= 0x10000 {
                    return None;
                }
                mb[usize::from(scan[idx])] = val as i16;
                if idx >= num_coeffs - 1 {
                    break;
                }
            }
        }
    } else {
        for mb in dst.chunks_exact_mut(num_coeffs).take(slice_width) {
            for idx in 0..num_coeffs {
                let val = gb.get_code(coeff_type, coeff_bits);
                if val >= 0x10000 {
                    return None;
                }
                mb[usize::from(scan[idx])] = val as i16;
            }
        }
    }
    Some(())
}

/// `recombine_block`: the low-frequency quarter of a block comes from the
/// base band, the rest from the extension band. Coefficients are stored
/// here in natural order (the C code stores them in the order its IDCT
/// wants, which for the x86 one is transposed).
fn recombine_block(dst: &mut [i16; 64], data: &[i16], base: &mut usize, ext: &mut usize) {
    for i in 0..4 {
        for j in 0..4 {
            dst[i * 8 + j] = data[*base + j];
        }
        for j in 0..4 {
            dst[i * 8 + j + 4] = data[*ext + j];
        }
        *base += 4;
        *ext += 4;
    }
    for i in 4..8 {
        for j in 0..8 {
            dst[i * 8 + j] = data[*ext + j];
        }
        *ext += 8;
    }
}

/// `recombine_block_il`, for the luma blocks of an interlaced frame.
fn recombine_block_il(dst: &mut [i16; 64], data: &[i16], base: &mut usize, ext: &mut usize, block_no: usize) {
    if block_no < 2 {
        for i in 0..8 {
            for j in 0..4 {
                dst[i * 8 + j] = data[*base + j];
            }
            for j in 0..4 {
                dst[i * 8 + j + 4] = data[*ext + j];
            }
            *base += 4;
            *ext += 4;
        }
    } else {
        for i in 0..64 {
            dst[i] = data[*ext + i];
        }
        *ext += 64;
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

/// A decoder for one stream. It holds the coefficient buffer, which the C
/// decoder does not clear completely between slices.
pub struct Decoder {
    mb_width: usize,
    mb_height: usize,
    num_x_slices: usize,
    slice_width: usize,
    idct: Idct,
    /// `slice_width` macroblocks of each band, one band after the other.
    slice_data: Vec<i16>,
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
            slice_data: vec![0; slice_width * BAND_COEFFS],
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
        let hdr = self.check(packet, frame.dims[0].0, frame.dims[0].1)?;
        let mut off = self.data_offset();
        let mut factors = [0i32; 64];
        for (f, &m) in factors.iter_mut().zip(&QUANT_MATRIX) {
            *f = i32::from(hdr.quant) * i32::from(m);
        }

        let mut sizes = packet[HDR_SIZE..].chunks_exact(2);
        for y in 0..self.mb_height {
            for x in (0..self.mb_width).step_by(self.slice_width) {
                let size = sizes.next().map_or(0, |s| usize::from(u16::from_le_bytes([s[0], s[1]]))) * 4;
                if size + off > packet.len() || size == 0 {
                    return Err(Error::SliceSize(x, y));
                }
                self.decode_slice(&hdr, &factors, x, y, &packet[off..], size, frame)
                    .ok_or(Error::Slice(x, y))?;
                off += size;
            }
        }
        Ok(())
    }

    /// `aic_decode_slice`. `src` starts at the slice and runs to the end
    /// of the packet.
    #[allow(clippy::too_many_arguments)]
    fn decode_slice(
        &mut self,
        hdr: &Header,
        factors: &[i32; 64],
        mb_x: usize,
        mb_y: usize,
        src: &[u8],
        src_size: usize,
        frame: &mut Frame<'_>,
    ) -> Option<()> {
        let slice_width = self.slice_width.min(self.mb_width - mb_x);
        let last_row = mb_y != 0 && mb_y == self.mb_height - 1;
        let (y_pos, c_pos) = if last_row {
            // The last row is placed against the bottom of the picture and
            // overlaps the row above it.
            (frame.height.saturating_sub(16), frame.height.div_ceil(2).saturating_sub(8))
        } else {
            (mb_y * 16, mb_y * 8)
        };
        let mut gb = BitReader::new(src, src_size);

        // Only the first `slice_width` macroblocks' worth of the buffer is
        // cleared, but the bands start where they would for a slice of
        // full width.
        self.slice_data[..slice_width * BAND_COEFFS].fill(0);
        for band in 0..4 {
            decode_coeffs(
                &mut gb,
                &mut self.slice_data[self.slice_width * BAND_OFF[band]..],
                band,
                slice_width,
                !hdr.interlaced,
            )?;
        }

        let data = &self.slice_data[..];
        let mut base_y = self.slice_width * BAND_OFF[0];
        let mut base_c = self.slice_width * BAND_OFF[1];
        let mut ext_y = self.slice_width * BAND_OFF[2];
        let mut ext_c = self.slice_width * BAND_OFF[3];
        let mut block = [0i16; 64];

        for mb in 0..slice_width {
            let x = (mb_x + mb) * 16;
            for blk in 0..4 {
                if !hdr.interlaced {
                    recombine_block(&mut block, data, &mut base_y, &mut ext_y);
                } else {
                    recombine_block_il(&mut block, data, &mut base_y, &mut ext_y, blk);
                }
                unquant_block(&mut block, factors);
                idct(&mut block, self.idct);

                if !hdr.interlaced {
                    frame.put_block(0, x + (blk & 1) * 8, y_pos + (blk >> 1) * 8, 1, &block);
                } else {
                    frame.put_block(0, x + (blk & 1) * 8, y_pos + (blk >> 1), 2, &block);
                }
            }

            for blk in 0..2 {
                recombine_block(&mut block, data, &mut base_c, &mut ext_c);
                unquant_block(&mut block, factors);
                idct(&mut block, self.idct);
                frame.put_block(1 + blk, x / 2, c_pos, 1, &block);
            }
        }
        Some(())
    }
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
                *self.bytes.last_mut().unwrap() |= ((value >> i & 1) as u8) << (7 - self.n % 8);
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
