//! Safe Rust port of FFmpeg's Apple ProRes decoder (`proresdec.c`, with
//! `proresdsp.c`, `simple_idct_template.c` and the x86 assembly in
//! `x86/proresdsp.asm`).
//!
//! A frame is a header, then one picture (progressive) or two (the fields
//! of an interlaced frame). A picture is a table of slice sizes and the
//! slices; a slice is up to eight macroblocks of one row, with one run of
//! entropy-coded DCT coefficients per colour plane and, optionally,
//! run-length coded alpha.
//!
//! The decoder keeps nothing from one frame to the next, so there is no
//! decoder object: [`FrameHeader::parse`] reads the header, the caller sets
//! up a picture of that size and layout, and [`decode_frame`] fills it.
//! Samples the bitstream does not reach (slices that fail to decode) are
//! left as they were, as in the C decoder.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

mod bits;
mod idct;

use bits::{decode_ac_coeffs, decode_dc_coeffs, BitReader};
pub use idct::Idct;

/// Byte `pos` of the packet. FFmpeg pads packets with zero bytes, and the C
/// decoder reads a few of them in several places.
#[inline(always)]
fn byte(packet: &[u8], pos: usize) -> u8 {
    packet.get(pos).copied().unwrap_or(0)
}

fn rb16(packet: &[u8], pos: usize) -> usize {
    usize::from(byte(packet, pos)) << 8 | usize::from(byte(packet, pos + 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Too short, not a ProRes frame, or a header longer than the packet.
    Invalid,
    /// A bitstream version above 1.
    UnsupportedVersion,
}

/// The frame header, as far as the decoder uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    /// Bytes from the start of the header to the first picture. Taken from
    /// the stream as it is: it may be shorter than the header really is.
    pub hdr_size: usize,
    pub width: usize,
    pub height: usize,
    /// 0 progressive, 1 interlaced top field first, 2 and 3 interlaced
    /// bottom field first.
    pub frame_type: u8,
    /// 4:4:4 chroma; otherwise 4:2:2.
    pub chroma444: bool,
    pub color_primaries: u8,
    pub color_trc: u8,
    pub colorspace: u8,
    /// 0 none, 1 alpha of 8 bits, 2 of 16 bits; above 2 is invalid, which
    /// the caller has to reject (the C decoder does so only after it has
    /// changed the picture size).
    pub alpha_info: u8,
    /// The header announces a quantisation matrix that does not fit in the
    /// packet. The caller has to reject the frame (the C decoder does so
    /// after it has set the pixel format and colour properties).
    pub truncated: bool,
    qmat_luma: [u8; 64],
    qmat_chroma: [u8; 64],
}

impl FrameHeader {
    /// `decode_frame` up to the picture, and `decode_frame_header`.
    pub fn parse(packet: &[u8]) -> Result<Self, HeaderError> {
        if packet.len() < 28 || packet[4..8] != *b"icpf" {
            return Err(HeaderError::Invalid);
        }
        let buf = &packet[8..];
        let hdr_size = rb16(buf, 0);
        if hdr_size > buf.len() {
            return Err(HeaderError::Invalid);
        }
        if rb16(buf, 2) > 1 {
            return Err(HeaderError::UnsupportedVersion);
        }

        let mut truncated = false;
        let flags = buf[19];
        let mut pos = 20;
        let mut qmat_luma = [4u8; 64];
        if flags & 2 != 0 {
            match buf.get(pos..).and_then(|b| b.first_chunk::<64>()) {
                Some(m) => qmat_luma = *m,
                None => truncated = true,
            }
            pos += 64;
        }
        let mut qmat_chroma = qmat_luma;
        if flags & 1 != 0 && !truncated {
            match buf.get(pos..).and_then(|b| b.first_chunk::<64>()) {
                Some(m) => qmat_chroma = *m,
                None => truncated = true,
            }
        }

        Ok(FrameHeader {
            hdr_size,
            width: rb16(buf, 8),
            height: rb16(buf, 10),
            frame_type: (buf[12] >> 2) & 3,
            chroma444: buf[12] & 0xC0 == 0xC0,
            color_primaries: buf[14],
            color_trc: buf[15],
            colorspace: buf[16],
            alpha_info: buf[17] & 0xf,
            truncated,
            qmat_luma,
            qmat_chroma,
        })
    }
}

/// The visible samples of one plane.
pub struct Plane<'a> {
    data: &'a mut [u16],
    stride: usize,
    width: usize,
    height: usize,
}

impl<'a> Plane<'a> {
    /// `data` must hold `stride * (height - 1) + width` samples.
    pub fn new(data: &'a mut [u16], stride: usize, width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 || stride < width {
            return None;
        }
        let need = stride.checked_mul(height - 1)?.checked_add(width)?;
        Some(Plane { data: data.get_mut(..need)?, stride, width, height })
    }

    /// Store the visible part of an 8x8 block whose top left sample is at
    /// column `x` of row `y` of a field.
    fn put_block(&mut self, x: usize, y: usize, field: Field, block: &[i16; 64]) {
        if x >= self.width {
            return;
        }
        let n = (self.width - x).min(8);
        for (r, src) in block.as_chunks::<8>().0.iter().enumerate() {
            let row = field.row(y + r);
            if row >= self.height {
                break;
            }
            let dst = &mut self.data[row * self.stride + x..][..n];
            for (d, s) in dst.iter_mut().zip(src) {
                *d = *s as u16;
            }
        }
    }

    /// Store the visible part of a row of samples.
    fn put_row(&mut self, x: usize, row: usize, src: &[u16]) {
        if x >= self.width || row >= self.height {
            return;
        }
        let n = (self.width - x).min(src.len());
        self.data[row * self.stride + x..][..n].copy_from_slice(&src[..n]);
    }
}

/// Which rows of the frame a picture covers.
#[derive(Clone, Copy)]
struct Field {
    /// 1 for a progressive picture, 2 for a field.
    step: usize,
    /// 1 for the field that starts on the second row.
    first: usize,
}

impl Field {
    fn row(self, y: usize) -> usize {
        y * self.step + self.first
    }
}

/// The picture to decode into. Its size and layout must be those of the
/// frame header: chroma planes of the full width for 4:4:4 and of
/// `(width + 1) / 2` otherwise, and an alpha plane exactly when the header
/// has alpha and it is not skipped.
pub struct Picture<'a> {
    pub luma: Plane<'a>,
    pub cb: Plane<'a>,
    pub cr: Plane<'a>,
    pub alpha: Option<Plane<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// The inverse DCT to reproduce; this also selects 10- or 12-bit
    /// output.
    pub idct: Idct,
    /// Ignore the alpha of the stream.
    pub skip_alpha: bool,
    /// Leave the chroma planes alone.
    pub gray: bool,
    /// The frame is flagged top field first. Decides which field of an
    /// interlaced frame comes first in the packet.
    pub top_field_first: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The picture given does not match the frame header.
    Layout,
    /// A broken picture header or slice table.
    InvalidData,
    /// A picture without slices.
    NoSlices,
    /// Every slice of a picture failed to decode.
    AllSlicesFailed,
}

/// A decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// Some slices failed to decode and were left as they were.
    pub slice_errors: bool,
}

struct Frame<'p, 'a> {
    packet: &'p [u8],
    hdr: &'p FrameHeader,
    opts: Options,
    /// Alpha depth in the stream, 0 when there is none to decode.
    alpha_bits: u32,
    pic: &'p mut Picture<'a>,
}

/// `decode_frame` from the first picture on.
pub fn decode_frame(
    packet: &[u8],
    hdr: &FrameHeader,
    opts: &Options,
    pic: &mut Picture<'_>,
) -> Result<Decoded, Error> {
    let alpha_bits = match (opts.skip_alpha, hdr.alpha_info) {
        (true, _) | (_, 0) => 0,
        (_, 1) => 8,
        (_, 2) => 16,
        _ => return Err(Error::Layout),
    };
    let cw = if hdr.chroma444 { hdr.width } else { hdr.width.div_ceil(2) };
    let fits = |p: &Plane<'_>, w: usize| p.width == w && p.height == hdr.height;
    if hdr.truncated
        || !fits(&pic.luma, hdr.width)
        || !fits(&pic.cb, cw)
        || !fits(&pic.cr, cw)
        || pic.alpha.is_some() != (alpha_bits != 0)
        || pic.alpha.as_ref().is_some_and(|a| !fits(a, hdr.width))
    {
        return Err(Error::Layout);
    }
    let Some(mut buf_size) = packet.len().checked_sub(8 + hdr.hdr_size) else {
        return Err(Error::Layout);
    };
    let mut buf = 8 + hdr.hdr_size;

    let mut frame = Frame { packet, hdr, opts: *opts, alpha_bits, pic };
    let mut slice_errors = false;
    let mut first_field = true;
    loop {
        let pic_size = frame.decode_picture(buf, buf_size, first_field, &mut slice_errors)?;
        buf += pic_size;
        buf_size -= pic_size;
        if hdr.frame_type != 0 && buf_size > 0 && first_field {
            first_field = false;
            continue;
        }
        return Ok(Decoded { slice_errors });
    }
}

struct Slice {
    /// Offset of the slice in the packet.
    data: usize,
    data_size: usize,
    mb_x: usize,
    mb_y: usize,
    mb_count: usize,
}

impl Frame<'_, '_> {
    /// `decode_picture_header` and `decode_picture`: returns the size of
    /// the picture in the packet.
    fn decode_picture(
        &mut self,
        buf: usize,
        buf_size: usize,
        first_field: bool,
        slice_errors: &mut bool,
    ) -> Result<usize, Error> {
        let packet = self.packet;
        let hdr_size = usize::from(byte(packet, buf) >> 3);
        if hdr_size < 8 || hdr_size > buf_size {
            return Err(Error::InvalidData);
        }
        let pic_data_size = rb16(packet, buf + 1) << 16 | rb16(packet, buf + 3);
        if pic_data_size > buf_size {
            return Err(Error::InvalidData);
        }
        let log2_slice_mb_width = byte(packet, buf + 7) >> 4;
        let log2_slice_mb_height = byte(packet, buf + 7) & 0xF;
        if log2_slice_mb_width > 3 || log2_slice_mb_height != 0 {
            return Err(Error::InvalidData);
        }
        let slice_mb_width = 1usize << log2_slice_mb_width;

        let mb_width = (self.hdr.width + 15) >> 4;
        let mb_height = if self.hdr.frame_type != 0 {
            (self.hdr.height + 31) >> 5
        } else {
            (self.hdr.height + 15) >> 4
        };

        // The count written in the header is ignored.
        let slice_count = mb_height
            * ((mb_width >> log2_slice_mb_width)
                + (mb_width & (slice_mb_width - 1)).count_ones() as usize);
        if slice_count == 0 {
            return Err(Error::NoSlices);
        }
        if hdr_size + slice_count * 2 > buf_size {
            return Err(Error::InvalidData);
        }

        // The table of slice sizes, then the slices. The whole table is
        // checked before the first slice is decoded.
        let index = buf + hdr_size;
        let first_slice = index + slice_count * 2;
        let slices = move || {
            let mut data = first_slice;
            let mut slice_mb_count = slice_mb_width;
            let (mut mb_x, mut mb_y) = (0, 0);
            (0..slice_count).map(move |i| {
                let data_size = rb16(packet, index + i * 2);
                while mb_width - mb_x < slice_mb_count {
                    slice_mb_count >>= 1;
                }
                let slice = Slice { data, data_size, mb_x, mb_y, mb_count: slice_mb_count };
                data += data_size;
                mb_x += slice_mb_count;
                if mb_x == mb_width {
                    slice_mb_count = slice_mb_width;
                    mb_x = 0;
                    mb_y += 1;
                }
                slice
            })
        };
        for slice in slices() {
            if slice.data_size < 6 || slice.data + slice.data_size > buf + buf_size {
                return Err(Error::InvalidData);
            }
        }

        let field = Field {
            step: if self.hdr.frame_type != 0 { 2 } else { 1 },
            first: usize::from(self.hdr.frame_type != 0 && first_field != self.opts.top_field_first),
        };
        let mut failed = 0;
        for slice in slices() {
            if self.decode_slice(&slice, field).is_err() {
                failed += 1;
            }
        }
        if failed != 0 {
            *slice_errors = true;
        }
        if failed == slice_count {
            return Err(Error::AllSlicesFailed);
        }
        Ok(pic_data_size)
    }

    /// `decode_slice_thread`.
    fn decode_slice(&mut self, slice: &Slice, field: Field) -> Result<(), bits::Damaged> {
        let packet = self.packet;
        let buf = slice.data;
        let data_size = slice.data_size as i32;

        // Slice header.
        let hdr_size = i32::from(byte(packet, buf) >> 3);
        let qscale = i32::from(byte(packet, buf + 1)).clamp(1, 224);
        let qscale = if qscale > 128 { (qscale - 96) << 2 } else { qscale };
        let y_data_size = rb16(packet, buf + 2) as i32;
        let u_data_size = rb16(packet, buf + 4) as i32;
        let mut v_data_size = data_size - y_data_size - u_data_size - hdr_size;
        if hdr_size > 7 {
            v_data_size = rb16(packet, buf + 6) as i32;
        }
        let a_data_size = data_size - y_data_size - u_data_size - v_data_size - hdr_size;
        if v_data_size < 0 || hdr_size + y_data_size + u_data_size + v_data_size > data_size {
            return Err(bits::Damaged);
        }
        let y_data = buf + hdr_size as usize;
        let u_data = y_data + y_data_size as usize;
        let v_data = u_data + u_data_size as usize;
        let a_data = v_data + v_data_size as usize;

        // The products are kept in 16 bits, as in C; they can overflow.
        let scaled = |m: &[u8; 64]| m.map(|q| (i32::from(q) * qscale) as i16);
        let qmat_luma = scaled(&self.hdr.qmat_luma);
        let qmat_chroma = scaled(&self.hdr.qmat_chroma);

        let scan = if self.hdr.frame_type == 0 { &PROGRESSIVE_SCAN } else { &INTERLACED_SCAN };
        let idct = self.opts.idct;
        let x = slice.mb_x << 4;
        let y = slice.mb_y << 4;
        let mut blocks = [0i16; 8 * 4 * 64];

        // Luma: four blocks per macroblock, in reading order.
        let count = slice.mb_count << 2;
        let coeffs = &mut blocks[..count * 64];
        let mut r = BitReader::new(packet.get(y_data..).unwrap_or(&[]), y_data_size as usize);
        decode_dc_coeffs(&mut r, coeffs, count)?;
        decode_ac_coeffs(&mut r, coeffs, count, scan)?;
        for (i, block) in coeffs.as_chunks_mut::<64>().0.iter_mut().enumerate() {
            idct::idct_block(idct, block, &qmat_luma);
            let bx = x + (i >> 2 << 4) + ((i & 1) << 3);
            self.pic.luma.put_block(bx, y + ((i & 2) << 2), field, block);
        }

        // Chroma: two blocks per macroblock for 4:2:2, four for 4:4:4, in
        // columns.
        if !self.opts.gray {
            let log2_blocks_per_mb = if self.hdr.chroma444 { 2 } else { 1 };
            let count = slice.mb_count << log2_blocks_per_mb;
            let cx = if self.hdr.chroma444 { x } else { x >> 1 };
            for (plane, data, size) in [
                (&mut self.pic.cb, u_data, u_data_size as usize),
                (&mut self.pic.cr, v_data, v_data_size as usize),
            ] {
                let coeffs = &mut blocks[..count * 64];
                coeffs.fill(0);
                // Some encodes have empty chroma scans to simulate grayscale.
                if size != 0 {
                    let mut r = BitReader::new(packet.get(data..).unwrap_or(&[]), size);
                    decode_dc_coeffs(&mut r, coeffs, count)?;
                    decode_ac_coeffs(&mut r, coeffs, count, scan)?;
                }
                for (i, block) in coeffs.as_chunks_mut::<64>().0.iter_mut().enumerate() {
                    idct::idct_block(idct, block, &qmat_chroma);
                    plane.put_block(cx + (i >> 1 << 3), y + ((i & 1) << 3), field, block);
                }
            }
        }

        // Alpha: sixteen rows of samples, no transform.
        if let (Some(plane), true) = (self.pic.alpha.as_mut(), a_data_size != 0) {
            let mut samples = [0u16; 8 * 16 * 16];
            let width = slice.mb_count * 16;
            let samples = &mut samples[..width * 16];
            let mut r = BitReader::new(packet.get(a_data..).unwrap_or(&[]), a_data_size as usize);
            unpack_alpha(&mut r, samples, self.alpha_bits, idct == Idct::C12);
            for (i, row) in samples.chunks_exact(width).enumerate() {
                plane.put_row(x, field.row(y + i), row);
            }
        }
        Ok(())
    }
}

/// `unpack_alpha`: run-length coded differences, for alpha of 8 or 16 bits
/// in the stream and 10 or 12 bits in the picture.
fn unpack_alpha(r: &mut BitReader<'_>, dst: &mut [u16], num_bits: u32, to_12_bits: bool) {
    let mask = (1i32 << num_bits) - 1;
    let convert = |v: i32| -> u16 {
        (match (num_bits, to_12_bits) {
            (16, false) => v >> 6,
            (16, true) => v >> 4,
            (_, false) => v << 2 | v >> 6,
            (_, true) => v << 4 | v >> 4,
        }) as u16
    };
    let num_coeffs = dst.len();
    let mut idx = 0;
    let mut alpha_val = mask;
    loop {
        loop {
            let val = if r.get_bit() != 0 {
                r.get_bits(num_bits) as i32
            } else {
                let v = r.get_bits(if num_bits == 16 { 7 } else { 4 }) as i32;
                let magnitude = (v + 2) >> 1;
                if v & 1 != 0 {
                    -magnitude
                } else {
                    magnitude
                }
            };
            alpha_val = (alpha_val + val) & mask;
            dst[idx] = convert(alpha_val);
            idx += 1;
            if idx >= num_coeffs || !(r.bits_left() > 0 && r.get_bit() != 0) {
                break;
            }
        }
        let mut run = r.get_bits(4) as usize;
        if run == 0 {
            run = r.get_bits(11) as usize;
        }
        let run = run.min(num_coeffs - idx);
        dst[idx..idx + run].fill(convert(alpha_val));
        idx += run;
        if idx >= num_coeffs {
            return;
        }
    }
}

/// Coefficient order of progressive pictures: index into a block in
/// natural order.
const PROGRESSIVE_SCAN: [u8; 64] = [
    0, 1, 8, 9, 2, 3, 10, 11, 16, 17, 24, 25, 18, 19, 26, 27, 4, 5, 12, 20, 13, 6, 7, 14, 21, 28,
    29, 22, 15, 23, 30, 31, 32, 33, 40, 48, 41, 34, 35, 42, 49, 56, 57, 50, 43, 36, 37, 44, 51, 58,
    59, 52, 45, 38, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Coefficient order of fields.
const INTERLACED_SCAN: [u8; 64] = [
    0, 8, 1, 9, 16, 24, 17, 25, 2, 10, 3, 11, 18, 26, 19, 27, 32, 40, 33, 34, 41, 48, 56, 49, 42,
    35, 43, 50, 57, 58, 51, 59, 4, 12, 5, 6, 13, 20, 28, 21, 14, 7, 15, 22, 29, 36, 44, 37, 30, 23,
    31, 38, 45, 52, 60, 53, 46, 39, 47, 54, 61, 62, 55, 63,
];

#[cfg(test)]
mod tests {
    use super::*;
    use bits::tests::BitWriter;

    /// A frame of `w` x `h` with one slice per macroblock, every plane a
    /// flat block of the DC value `dc`. Options: 4:4:4, field count.
    fn flat_frame(w: usize, h: usize, dc: i16, chroma444: bool, fields: usize) -> std::vec::Vec<u8> {
        let mut p = std::vec![0u8; 8];
        p[4..8].copy_from_slice(b"icpf");
        let mut hdr = [0u8; 20];
        hdr[1] = 20;
        hdr[8..10].copy_from_slice(&(w as u16).to_be_bytes());
        hdr[10..12].copy_from_slice(&(h as u16).to_be_bytes());
        hdr[12] = if chroma444 { 0xC0 } else { 0x80 } | if fields == 2 { 4 } else { 0 };
        p.extend_from_slice(&hdr);

        let mb_w = (w + 15) / 16;
        let mb_h = if fields == 2 { (h + 31) / 32 } else { (h + 15) / 16 };
        for _ in 0..fields {
            let mut slices = std::vec::Vec::new();
            let mut index = std::vec::Vec::new();
            for _ in 0..mb_w * mb_h {
                let plane = |blocks: usize| {
                    let mut coeffs = std::vec![0i16; blocks * 64];
                    for b in 0..blocks {
                        coeffs[b * 64] = dc;
                    }
                    let mut wr = BitWriter::new();
                    wr.plane(&coeffs, blocks, &PROGRESSIVE_SCAN);
                    wr.finish()
                };
                let (y, c) = (plane(4), plane(if chroma444 { 4 } else { 2 }));
                let mut s = std::vec![6 << 3, 4];
                s.extend_from_slice(&(y.len() as u16).to_be_bytes());
                s.extend_from_slice(&(c.len() as u16).to_be_bytes());
                s.extend_from_slice(&y);
                s.extend_from_slice(&c);
                s.extend_from_slice(&c);
                index.extend_from_slice(&(s.len() as u16).to_be_bytes());
                slices.extend_from_slice(&s);
            }
            let size = 8 + index.len() + slices.len();
            p.push(8 << 3);
            p.extend_from_slice(&(size as u32).to_be_bytes());
            p.extend_from_slice(&[0, 0, 0]);
            p.extend_from_slice(&index);
            p.extend_from_slice(&slices);
        }
        p
    }

    fn decode(packet: &[u8], idct: Idct) -> (Result<Decoded, Error>, [std::vec::Vec<u16>; 3]) {
        let hdr = FrameHeader::parse(packet).unwrap();
        let cw = if hdr.chroma444 { hdr.width } else { hdr.width.div_ceil(2) };
        let mut y = std::vec![0u16; hdr.width * hdr.height];
        let mut u = std::vec![0u16; cw * hdr.height];
        let mut v = std::vec![0u16; cw * hdr.height];
        let mut pic = Picture {
            luma: Plane::new(&mut y, hdr.width, hdr.width, hdr.height).unwrap(),
            cb: Plane::new(&mut u, cw, cw, hdr.height).unwrap(),
            cr: Plane::new(&mut v, cw, cw, hdr.height).unwrap(),
            alpha: None,
        };
        let opts = Options { idct, skip_alpha: false, gray: false, top_field_first: false };
        let r = decode_frame(packet, &hdr, &opts, &mut pic);
        (r, [y, u, v])
    }

    #[test]
    fn header_checks() {
        let p = flat_frame(16, 16, 0, false, 1);
        assert_eq!(FrameHeader::parse(&p[..27]), Err(HeaderError::Invalid));
        let mut q = p.clone();
        q[4] = b'x';
        assert_eq!(FrameHeader::parse(&q), Err(HeaderError::Invalid));
        let mut q = p.clone();
        q[11] = 2;
        assert_eq!(FrameHeader::parse(&q), Err(HeaderError::UnsupportedVersion));
        let mut q = p.clone();
        q[8 + 19] = 2; // a luma matrix that is not there
        q.truncate(8 + 20 + 63);
        assert!(FrameHeader::parse(&q).unwrap().truncated);
        let h = FrameHeader::parse(&p).unwrap();
        assert_eq!((h.width, h.height, h.frame_type, h.chroma444, h.truncated), (16, 16, 0, false, false));
    }

    #[test]
    fn flat_frames_of_odd_sizes() {
        for (w, h) in [(1, 1), (16, 16), (17, 15), (33, 31), (130, 3)] {
            for chroma444 in [false, true] {
                for fields in [1, 2] {
                    let p = flat_frame(w, h, 640, chroma444, fields);
                    let (r, planes) = decode(&p, Idct::Asm10);
                    assert_eq!(r, Ok(Decoded { slice_errors: false }), "{w}x{h}");
                    // 640 * 4 (the default matrix) * 4 (qscale) / 32 on top of 512.
                    for plane in &planes {
                        assert!(plane.iter().all(|&s| s == 512 + 320), "{w}x{h} {chroma444} {fields}");
                    }
                    let (_, planes) = decode(&p, Idct::C12);
                    assert!(planes[0].iter().all(|&s| s == 2048 + 1280));
                }
            }
        }
    }

    #[test]
    fn a_damaged_slice_is_left_alone() {
        let mut p = flat_frame(32, 16, 640, false, 1);
        // The first slice starts after the picture header (8) and the
        // table (2 * 2); zero its luma data: an endless code.
        let slice = 8 + 20 + 8 + 4;
        let y_size = usize::from(p[slice + 3]);
        p[slice + 6..slice + 6 + y_size].fill(0);
        let (r, planes) = decode(&p, Idct::Asm10);
        assert_eq!(r, Ok(Decoded { slice_errors: true }));
        for row in planes[0].chunks(32) {
            assert!(row[..16].iter().all(|&s| s == 0) && row[16..].iter().all(|&s| s == 832));
        }
        // With every slice damaged there is no frame.
        let second = slice + rb16(&p, 8 + 20 + 8);
        let y_size = usize::from(p[second + 3]);
        p[second + 6..second + 6 + y_size].fill(0);
        assert_eq!(decode(&p, Idct::Asm10).0, Err(Error::AllSlicesFailed));
    }

    #[test]
    fn alpha_runs() {
        // Raw 8-bit value 0x40 (added to 0xff: 0x3f), a run of 5, then a
        // difference of +2 and a run to the end.
        let mut w = BitWriter::new();
        w.put(1, 1);
        w.put(8, 0x40);
        w.put(1, 0);
        w.put(4, 5);
        w.put(1, 0);
        w.put(4, 0b0010);
        w.put(1, 0);
        w.put(4, 0);
        w.put(11, 2047);
        let bytes = w.finish();
        let mut dst = [0u16; 256];
        unpack_alpha(&mut BitReader::new(&bytes, bytes.len()), &mut dst, 8, false);
        assert!(dst[..6].iter().all(|&s| s == 0x3f << 2));
        assert!(dst[6..].iter().all(|&s| s == (0x41 << 2 | 1)));
        let mut dst = [0u16; 256];
        unpack_alpha(&mut BitReader::new(&bytes, bytes.len()), &mut dst, 8, true);
        assert!(dst[6..].iter().all(|&s| s == (0x41 << 4 | 4)));
    }
}
