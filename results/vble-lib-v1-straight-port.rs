//! Safe Rust port of FFmpeg's VBLE decoder (`vble.c`): a lossless codec
//! for 8-bit 4:2:0 pictures.
//!
//! A packet is a 32-bit version number followed by a little-endian
//! bitstream in two parts. The first part holds one code length (0 to 8)
//! per sample, written as that many zero bits and a one bit. The second
//! part holds, for every sample with a non-zero length, that many bits of
//! residual. Residuals are added to a prediction: the sample to the left in
//! the first row of a plane, the median of left, top and left + top -
//! top-left in the other rows.
//!
//! FFmpeg runs the median predictor through hand-written SSE2 assembly
//! (`ff_add_median_pred_sse2`). This crate has no assembly and no `unsafe`.

#![forbid(unsafe_code)]
#![no_std]

extern crate alloc;

use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Fewer than four bytes, or more than `i32::MAX / 8` after the version.
    PacketSize,
    /// A code length above 8, or fewer residual bits than the lengths ask for.
    InvalidCode,
    /// `restore` without a successful `unpack`, or a picture of another size.
    BadCall,
}

/// The number of code lengths in a packet: one per sample of a 4:2:0
/// picture whose chroma planes are rounded up, which is what
/// `av_image_get_buffer_size(AV_PIX_FMT_YUV420P, width, height, 1)` returns.
///
/// The decoder itself uses chroma planes that are rounded down, so for odd
/// sizes the last codes of a packet are read and then ignored.
pub fn symbol_count(width: usize, height: usize) -> Option<usize> {
    let luma = width.checked_mul(height)?;
    let chroma = width.div_ceil(2).checked_mul(height.div_ceil(2))?;
    luma.checked_add(chroma.checked_mul(2)?)
}

/// The version number at the start of a packet (1 in every known file; the
/// C decoder warns about anything else and decodes regardless).
pub fn version(packet: &[u8]) -> Option<u32> {
    packet.first_chunk::<4>().map(|v| u32::from_le_bytes(*v))
}

/// The visible part of one plane: `stride * (rows - 1) + cols` bytes.
pub struct Plane<'a> {
    data: &'a mut [u8],
    stride: usize,
    cols: usize,
    rows: usize,
}

impl<'a> Plane<'a> {
    pub fn new(data: &'a mut [u8], stride: usize, cols: usize, rows: usize) -> Option<Self> {
        if cols == 0 || rows == 0 || stride < cols {
            return None;
        }
        let need = stride.checked_mul(rows - 1)?.checked_add(cols)?;
        Some(Plane { data: data.get_mut(..need)?, stride, cols, rows })
    }
}

/// The three planes of a picture. Luma is `width` by `height`, chroma
/// `(width + 1) / 2` by `(height + 1) / 2`.
pub struct Picture<'a> {
    pub y: Plane<'a>,
    pub u: Plane<'a>,
    pub v: Plane<'a>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// `AV_CODEC_FLAG_GRAY`: leave the chroma planes alone.
    pub gray: bool,
    /// Reproduce what FFmpeg's SSE2 median predictor does to the sample
    /// after the end of a row (see `restore_plane`).
    pub sse2_overwrite: bool,
}

/// FFmpeg's bit reader (`get_bits.h`, little-endian, checked): the position
/// never moves more than 8 bits past the end, and bits past the end read
/// as zero (FFmpeg requires zeroed padding after a packet).
struct BitReader<'a> {
    buf: &'a [u8],
    index: usize,
    size_in_bits: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8], index: usize) -> Self {
        BitReader { buf, index, size_in_bits: buf.len() * 8 }
    }

    /// The 32 bits at the current position, or as many as a 32-bit load at
    /// the current byte gives.
    fn cache(&self) -> u32 {
        let at = self.index >> 3;
        let mut bytes = [0u8; 4];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = self.buf.get(at + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(bytes) >> (self.index & 7)
    }

    fn show_bits(&self, n: u32) -> u32 {
        self.cache() & ((1 << n) - 1)
    }

    fn skip_bits(&mut self, n: u32) {
        self.index = (self.index + n as usize).min(self.size_in_bits + 8);
    }

    fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.show_bits(n);
        self.skip_bits(n);
        v
    }

    fn bits_left(&self) -> i64 {
        self.size_in_bits as i64 - self.index as i64
    }
}

fn mid_pred(a: u8, b: u8, c: u8) -> u8 {
    a.min(b).max(a.max(b).min(c))
}

/// Decoder state: the values buffer of the C decoder, which first holds
/// the code lengths of a packet and then, row by row, the residuals.
pub struct Decoder {
    width: usize,
    height: usize,
    val: Vec<u8>,
    /// Where the residual bits start, after a successful `unpack`.
    pos: Option<usize>,
}

impl Decoder {
    pub fn new(width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let size = symbol_count(width, height)?;
        let mut val = Vec::new();
        val.try_reserve_exact(size).ok()?;
        val.resize(size, 0);
        Some(Decoder { width, height, val, pos: None })
    }

    /// Codes per packet.
    pub fn symbols(&self) -> usize {
        self.val.len()
    }

    /// Read all the code lengths of a packet.
    pub fn unpack(&mut self, packet: &[u8]) -> Result<(), Error> {
        self.pos = None;
        let Some((_version, data)) = packet.split_first_chunk::<4>() else {
            return Err(Error::PacketSize);
        };
        if data.len() > i32::MAX as usize / 8 {
            return Err(Error::PacketSize);
        }
        let mut gb = BitReader::new(data, 0);
        let mut allbits = 0usize;

        for v in self.val.iter_mut() {
            // At most 9 bits are needed for lengths up to 8.
            let bits = gb.show_bits(8);
            let len = if bits != 0 {
                let len = bits.trailing_zeros();
                gb.skip_bits(len + 1);
                len
            } else {
                gb.skip_bits(8);
                if gb.get_bits(1) == 0 {
                    return Err(Error::InvalidCode);
                }
                8
            };
            *v = len as u8;
            allbits += len as usize;
        }

        // Check we have enough bits left.
        if gb.bits_left() < allbits as i64 {
            return Err(Error::InvalidCode);
        }
        self.pos = Some(gb.index);
        Ok(())
    }

    /// Decode the residuals of the packet last given to `unpack` and
    /// rebuild the picture.
    pub fn restore(&mut self, packet: &[u8], pic: &mut Picture<'_>, opts: Options) -> Result<(), Error> {
        let (width, height) = (self.width, self.height);
        let (width_uv, height_uv) = (width / 2, height / 2);
        let pos = self.pos.take().ok_or(Error::BadCall)?;
        let data = packet.get(4..).ok_or(Error::BadCall)?;
        let (cols_uv, rows_uv) = (width.div_ceil(2), height.div_ceil(2));
        if (pic.y.cols, pic.y.rows) != (width, height)
            || (pic.u.cols, pic.u.rows) != (cols_uv, rows_uv)
            || (pic.v.cols, pic.v.rows) != (cols_uv, rows_uv)
        {
            return Err(Error::BadCall);
        }
        let mut gb = BitReader::new(data, pos);

        restore_plane(&mut gb, &mut pic.y, &mut self.val, width, height, opts);
        if !opts.gray {
            let mut offset = width * height;
            let val = self.val.get_mut(offset..).unwrap_or(&mut []);
            restore_plane(&mut gb, &mut pic.u, val, width_uv, height_uv, opts);
            offset += width_uv * height_uv;
            let val = self.val.get_mut(offset..).unwrap_or(&mut []);
            restore_plane(&mut gb, &mut pic.v, val, width_uv, height_uv, opts);
        }
        Ok(())
    }
}

/// Rebuild `height` rows of `width` samples. `val` starts at the plane's
/// first code length and runs to the end of the values buffer.
///
/// `width` may be smaller than the plane's visible width (chroma of a
/// picture with an odd width), and may be zero. Two things the C decoder
/// does then are visible and are reproduced here:
///
/// * It stores the first value of the first row without looking at the
///   width, so a plane of zero width still gets one sample: the code
///   length found at the plane's position in the values buffer.
/// * The SSE2 median predictor works in blocks of 16 samples and stores
///   whole blocks. If `width` is not a multiple of 16 (or is zero), the
///   sample after the end of each predicted row is overwritten with a
///   prediction too, using as its residual whatever follows the row in the
///   values buffer: the code length of the next row's first sample. The C
///   version of the predictor does not do this, so FFmpeg's output for odd
///   widths depends on whether it runs with assembly.
fn restore_plane(gb: &mut BitReader<'_>, plane: &mut Plane<'_>, val: &mut [u8], width: usize, height: usize, opts: Options) {
    let stride = plane.stride;
    let overwrite = opts.sse2_overwrite && plane.cols > width && (width % 16 != 0 || width == 0);

    for i in 0..height {
        let Some(row) = val.get_mut(i * width..(i + 1) * width) else {
            return;
        };
        for v in row.iter_mut() {
            // get_bits can't take a length of 0
            if *v != 0 {
                let n = u32::from(*v);
                let x = (1 << n) + gb.get_bits(n) - 1;
                *v = ((x >> 1) ^ (x & 1).wrapping_neg()) as u8;
            }
        }
        let row = &val[i * width..(i + 1) * width];
        if i > 0 {
            let (above, dst) = plane.data.split_at_mut(i * stride);
            let top = &above[(i - 1) * stride..];
            let mut left = 0u8;
            let mut left_top = top[0];
            for j in 0..width {
                let t = top[j];
                left = mid_pred(left, t, left.wrapping_add(t).wrapping_sub(left_top)).wrapping_add(row[j]);
                left_top = t;
                dst[j] = left;
            }
            if overwrite {
                let t = top[width];
                let diff = val.get((i + 1) * width).copied().unwrap_or(0);
                dst[width] = mid_pred(left, t, left.wrapping_add(t).wrapping_sub(left_top)).wrapping_add(diff);
            }
        } else {
            let dst = &mut plane.data[..];
            dst[0] = val.first().copied().unwrap_or(0);
            for j in 1..width {
                dst[j] = row[j].wrapping_add(dst[j - 1]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    struct BitWriter {
        out: Vec<u8>,
        acc: u64,
        n: u32,
    }

    impl BitWriter {
        fn put(&mut self, v: u32, n: u32) {
            self.acc |= u64::from(v) << self.n;
            self.n += n;
            while self.n >= 8 {
                self.out.push(self.acc as u8);
                self.acc >>= 8;
                self.n -= 8;
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.out.push(self.acc as u8);
            }
            self.out
        }
    }

    /// A packet from (length, bits) pairs.
    fn packet(codes: &[(u32, u32)]) -> Vec<u8> {
        let mut w = BitWriter { out: vec![1, 0, 0, 0], acc: 0, n: 0 };
        for &(len, _) in codes {
            w.put(1 << len, len + 1);
        }
        for &(len, bits) in codes {
            w.put(bits, len);
        }
        w.finish()
    }

    /// The code for a residual.
    fn code(r: u8) -> (u32, u32) {
        let s = i32::from(r as i8);
        let v = (if s >= 0 { 2 * s } else { -2 * s - 1 }) as u32;
        let len = 31 - (v + 1).leading_zeros();
        (len, v + 1 - (1 << len))
    }

    /// Residuals of a plane, the way the decoder undoes them.
    fn residuals(p: &[u8], w: usize, h: usize, out: &mut Vec<(u32, u32)>) {
        for y in 0..h {
            for x in 0..w {
                let cur = p[y * w + x];
                let pred = if y == 0 {
                    if x == 0 { 0 } else { p[x - 1] }
                } else {
                    let t = p[(y - 1) * w + x];
                    let (l, lt) = if x == 0 { (0, t) } else { (p[y * w + x - 1], p[(y - 1) * w + x - 1]) };
                    mid_pred(l, t, l.wrapping_add(t).wrapping_sub(lt))
                };
                out.push(code(cur.wrapping_sub(pred)));
            }
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u8 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 56) as u8
        }
    }

    fn decode(pkt: &[u8], w: usize, h: usize, opts: Options) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), Error> {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        // Strides wider than the rows, and a marker value in the padding.
        let (ys, cs) = (w + 3, cw + 5);
        let mut y = vec![0xaa; ys * (h - 1) + w];
        let mut u = vec![0xaa; cs * (ch - 1) + cw];
        let mut v = vec![0xaa; cs * (ch - 1) + cw];
        let mut dec = Decoder::new(w, h).unwrap();
        dec.unpack(pkt)?;
        let mut pic = Picture {
            y: Plane::new(&mut y, ys, w, h).unwrap(),
            u: Plane::new(&mut u, cs, cw, ch).unwrap(),
            v: Plane::new(&mut v, cs, cw, ch).unwrap(),
        };
        dec.restore(pkt, &mut pic, opts)?;
        let rows = |p: &[u8], s: usize, w: usize, h: usize| -> Vec<u8> {
            (0..h).flat_map(|r| p[r * s..r * s + w].to_vec()).collect()
        };
        // Nothing outside the visible rows may change.
        for r in 0..h - 1 {
            assert!(y[r * ys + w..(r + 1) * ys].iter().all(|&b| b == 0xaa));
        }
        Ok((rows(&y, ys, w, h), rows(&u, cs, cw, ch), rows(&v, cs, cw, ch)))
    }

    #[test]
    fn round_trip_even_sizes() {
        let mut rng = Rng(1);
        for (w, h) in [(2, 2), (4, 2), (16, 8), (34, 6), (64, 4), (18, 20)] {
            for smooth in [false, true] {
                let mut gen = |n: usize| -> Vec<u8> {
                    (0..n).map(|i| if smooth { (i as u8).wrapping_add(rng.next() & 3) } else { rng.next() }).collect()
                };
                let (y, u, v) = (gen(w * h), gen(w * h / 4), gen(w * h / 4));
                let mut codes = Vec::new();
                residuals(&y, w, h, &mut codes);
                residuals(&u, w / 2, h / 2, &mut codes);
                residuals(&v, w / 2, h / 2, &mut codes);
                let pkt = packet(&codes);
                for sse2_overwrite in [false, true] {
                    let got = decode(&pkt, w, h, Options { gray: false, sse2_overwrite }).unwrap();
                    assert_eq!(got, (y.clone(), u.clone(), v.clone()), "{w}x{h}");
                }
            }
        }
    }

    #[test]
    fn lengths_and_values() {
        // 2x1: no chroma rows are decoded, but four codes are read.
        // Length 8 with all bits set is 510: (510 >> 1) ^ 0 = 255.
        // Length 8 with bits 254 is 509: (509 >> 1) ^ -1 = 0x01 as a byte.
        let pkt = packet(&[(8, 255), (8, 254), (0, 0), (3, 5)]);
        let (y, u, v) = decode(&pkt, 2, 1, Options::default()).unwrap();
        assert_eq!(y, [255, 0]);
        assert_eq!((u, v), (vec![0xaa], vec![0xaa]));
    }

    #[test]
    fn errors() {
        let mut dec = Decoder::new(2, 2).unwrap();
        assert_eq!(dec.symbols(), 6);
        assert_eq!(dec.unpack(&[1, 0, 0]), Err(Error::PacketSize));
        // Nine zero bits: a length above 8.
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0, 0, 0]), Err(Error::InvalidCode));
        // Six lengths of 0 need six bits.
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0x3f]), Ok(()));
        assert_eq!(dec.unpack(&[9, 0, 0, 0, 0x3f]), Ok(()));
        assert_eq!((version(&[9, 0, 0, 1, 0x3f]), version(&[9, 0, 0])), (Some(0x0100_0009), None));
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0x1f]), Err(Error::InvalidCode));
        // Six lengths of 1 take 12 bits and need 6 more: 18 bits do not fit
        // in two bytes, but fit in three.
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0xaa, 0x0a]), Err(Error::InvalidCode));
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0xaa, 0x0a, 0]), Ok(()));
        // Five lengths of 0 and one of 8, which needs 8 more bits.
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0x1f, 0x20, 0]), Ok(()));
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0x1f, 0x20]), Err(Error::InvalidCode));
        // Ten zero bits before the one.
        assert_eq!(dec.unpack(&[1, 0, 0, 0, 0x1f, 0x80, 0, 0]), Err(Error::InvalidCode));
        let mut y = [0u8; 4];
        let (mut u, mut v) = ([0u8; 1], [0u8; 1]);
        let mut pic = Picture {
            y: Plane::new(&mut y, 2, 2, 2).unwrap(),
            u: Plane::new(&mut u, 1, 1, 1).unwrap(),
            v: Plane::new(&mut v, 1, 1, 1).unwrap(),
        };
        assert_eq!(dec.restore(&[1, 0, 0, 0, 0x3f], &mut pic, Options::default()), Err(Error::BadCall));
    }

    #[test]
    fn odd_width() {
        // 3x4: luma 12 codes, chroma 1x2 each, and 2 * (2 * 2 - 1 * 2) = 4
        // codes that are read and ignored.
        let mut codes = vec![(0, 0); 12];
        codes.extend([code(10), code(5)]); // U
        codes.extend([(3, 0), code(7)]); // V: the first has length 3
        codes.extend([(4, 0), (0, 0), (0, 0), (0, 0)]);
        let pkt = packet(&codes);
        let plain = decode(&pkt, 3, 4, Options::default()).unwrap();
        let sse2 = decode(&pkt, 3, 4, Options { gray: false, sse2_overwrite: true }).unwrap();
        // The second chroma column is never decoded. Its first row keeps
        // what was in the frame; with the SSE2 overwrite, its second row is
        // predicted from left (5, or 7), top (0xaa) and top-left (10, or
        // 3 = code(3,0) decoded: (8 + 0 - 1) -> -4 = 0xfc), plus the code
        // length that follows the row in the values buffer (3, then 4).
        assert_eq!(plain.1, [10, 0xaa, 5, 0xaa]);
        assert_eq!(plain.2, [0xfc, 0xaa, 7, 0xaa]);
        let m = |l: u8, t: u8, lt: u8| mid_pred(l, t, l.wrapping_add(t).wrapping_sub(lt));
        assert_eq!(sse2.1, [10, 0xaa, 5, m(5, 0xaa, 10) + 3]);
        assert_eq!(sse2.2, [0xfc, 0xaa, 7, m(7, 0xaa, 0xfc).wrapping_add(4)]);
        assert_eq!(plain.0, sse2.0);
    }

    #[test]
    fn width_one() {
        // 1x4: luma 4 codes; chroma is 0 wide and 2 rows; 4 more codes.
        let codes = [code(1), code(1), code(1), code(1), (6, 0), (2, 0), (0, 0), (0, 0)];
        let pkt = packet(&codes);
        let plain = decode(&pkt, 1, 4, Options::default()).unwrap();
        let sse2 = decode(&pkt, 1, 4, Options { gray: false, sse2_overwrite: true }).unwrap();
        assert_eq!(plain.0, [1, 1, 1, 1]);
        // Both chroma planes start at the same place in the values buffer
        // and get the code length found there.
        assert_eq!((&plain.1, &plain.2), (&vec![6, 0xaa], &vec![6, 0xaa]));
        assert_eq!((&sse2.1, &sse2.2), (&vec![6, 6], &vec![6, 6]));
        let gray = decode(&pkt, 1, 4, Options { gray: true, sse2_overwrite: true }).unwrap();
        assert_eq!((gray.1, gray.2), (vec![0xaa, 0xaa], vec![0xaa, 0xaa]));
    }
}
