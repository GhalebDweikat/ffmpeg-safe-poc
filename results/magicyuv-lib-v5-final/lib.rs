//! Safe Rust port of FFmpeg's MagicYUV decoder (`magicyuv.c`), with the
//! parts of `vlc.c`, `bitstream_template.h` and `lossless_videodsp.c` it
//! uses.
//!
//! The port is bit-exact with the C decoder, including on damaged input,
//! where the C decoder still outputs a frame. That makes three things part
//! of the format as far as this port is concerned:
//!
//! * The Huffman tables are built from the code lengths in the packet by
//!   FFmpeg's table builder, quirks included (see [`vlc`]).
//! * A slice that runs out of bits leaves the rest of its rows as they
//!   were in the frame buffer, and the predictor then runs over them.
//!   What the C bit reader returns at and past the end of a slice is
//!   reproduced exactly (see [`bits`]).
//! * A joint table hit stores eight bytes, not just its symbols; the extra
//!   bytes stay in the picture if the slice ends right there.

#![forbid(unsafe_code)]
#![no_std]

extern crate alloc;

use alloc::vec::Vec;

mod bits;
pub mod pred;
mod vlc;

use bits::BitReader;
use vlc::{Multi, Vlc};

const LEFT: u8 = 1;
const GRADIENT: u8 = 2;
const MEDIAN: u8 = 3;

/// Why a packet was refused before any frame was allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Invalid data, nothing more to say.
    Invalid,
    /// The header size field does not fit the packet.
    HeaderSize(u32),
    /// Unsupported bitstream version.
    Version(u8),
    /// Unsupported pixel format code.
    Format(u8),
    /// Slices narrower than the picture are not supported.
    SliceWidth(u32),
    SliceHeight(i32),
    /// Slice height not a multiple of the chroma subsampling.
    SliceHeightAlign(i32),
    SliceCount(i32),
    ImpossibleSliceHeight,
    ImpossibleHeight,
    HuffmanCodes,
    HuffmanBuild,
    HuffmanShort,
}

/// The decoder was called with planes that do not fit the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPlanes;

/// What a pixel format code means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    /// The code in the packet (0x65 and up).
    pub code: u8,
    pub planes: usize,
    /// Bits per sample: 8, 10, 12 or 14.
    pub bps: u32,
    /// log2 of the horizontal and vertical chroma subsampling.
    pub hshift: u32,
    pub vshift: u32,
    /// Planes are G, B, R(, A) and blue and red are stored minus green.
    pub rgb: bool,
}

impl Format {
    fn from_code(code: u8) -> Option<Format> {
        let f = |planes, bps, hshift, vshift, rgb| Some(Format { code, planes, bps, hshift, vshift, rgb });
        match code {
            0x65 => f(3, 8, 0, 0, true),    // GBRP
            0x66 => f(4, 8, 0, 0, true),    // GBRAP
            0x67 => f(3, 8, 0, 0, false),   // YUV444P
            0x68 => f(3, 8, 1, 0, false),   // YUV422P
            0x69 => f(3, 8, 1, 1, false),   // YUV420P
            0x6a => f(4, 8, 0, 0, false),   // YUVA444P
            0x6b => f(1, 8, 0, 0, false),   // GRAY8
            0x6c => f(3, 10, 1, 0, false),  // YUV422P10
            0x76 => f(3, 10, 0, 0, false),  // YUV444P10
            0x6d => f(3, 10, 0, 0, true),   // GBRP10
            0x6e => f(4, 10, 0, 0, true),   // GBRAP10
            0x6f => f(3, 12, 0, 0, true),   // GBRP12
            0x70 => f(4, 12, 0, 0, true),   // GBRAP12
            0x71 => f(3, 14, 0, 0, true),   // GBRP14
            0x72 => f(4, 14, 0, 0, true),   // GBRAP14
            0x73 => f(1, 10, 0, 0, false),  // GRAY10
            0x7b => f(3, 10, 1, 1, false),  // YUV420P10
            _ => None,
        }
    }

    /// Subsampling shifts of plane `i`: only planes 1 and 2 are chroma.
    fn shifts(&self, i: usize) -> (u32, u32) {
        if i == 1 || i == 2 {
            (self.hshift, self.vshift)
        } else {
            (0, 0)
        }
    }
}

/// The first part of the header: everything the caller needs to set up the
/// picture size and format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub format: Format,
    pub color_matrix: u8,
    pub flags: u8,
    /// As stored: 32 bits, reinterpreted as signed like the C decoder does.
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Slice {
    start: usize,
    size: u32,
}

/// One plane of the visible picture: rows `stride` samples apart, the last
/// one cut off after the last visible sample.
pub struct Plane<'a, T> {
    pub data: &'a mut [T],
    pub stride: usize,
}

/// A slice ended early. The frame is output anyway.
struct SliceError;

fn le32(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

fn ceil_rshift(a: usize, b: u32) -> usize {
    (a + (1 << b) - 1) >> b
}

#[cfg(test)]
fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// The row functions a decode uses: the portable ones or their SIMD
/// versions.
#[derive(Clone, Copy)]
struct Kernels<A, B, C, D> {
    left8: A,
    gradient8: B,
    median8: C,
    left16: D,
}

pub struct Decoder {
    format: Format,
    interlaced: bool,
    coded_width: usize,
    coded_height: usize,
    slice_height: usize,
    nb_slices: usize,
    slices: [Vec<Slice>; 4],
    vlc: [Vlc; 4],
    scratch: vlc::Scratch,
    ready: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            format: Format { code: 0, planes: 0, bps: 8, hshift: 0, vshift: 0, rgb: false },
            interlaced: false,
            coded_width: 0,
            coded_height: 0,
            slice_height: 0,
            nb_slices: 0,
            slices: Default::default(),
            vlc: Default::default(),
            scratch: vlc::Scratch::new(),
            ready: false,
        }
    }

    /// The checks the C decoder makes before it sets the picture size.
    pub fn header(&mut self, packet: &[u8]) -> Result<Header, Error> {
        self.ready = false;
        if packet.len() < 36 {
            return Err(Error::Invalid);
        }
        if &packet[..4] != b"MAGY" {
            return Err(Error::Invalid);
        }
        let header_size = le32(packet, 4);
        if header_size < 32 || header_size as u64 >= packet.len() as u64 {
            return Err(Error::HeaderSize(header_size));
        }
        if packet[8] != 7 {
            return Err(Error::Version(packet[8]));
        }
        let format = Format::from_code(packet[9]).ok_or(Error::Format(packet[9]))?;
        Ok(Header {
            format,
            color_matrix: packet[11],
            flags: packet[12],
            width: le32(packet, 16) as i32,
            height: le32(packet, 20) as i32,
        })
    }

    /// The rest of the header: slice positions and Huffman tables.
    /// `coded_width` and `coded_height` are what the caller made of the
    /// size in the header. After this succeeds the packet can be decoded.
    pub fn tables(&mut self, packet: &[u8], coded_width: i32, coded_height: i32) -> Result<(), Error> {
        let header = self.header(packet)?;
        let format = header.format;
        let size = packet.len() as u32;         // C: int avpkt->size
        let header_size = le32(packet, 4);
        if coded_width <= 0 || coded_height <= 0 {
            return Err(Error::Invalid);
        }

        let slice_width = le32(packet, 24);
        if slice_width != coded_width as u32 {
            return Err(Error::SliceWidth(slice_width));
        }
        let slice_height = le32(packet, 28) as i32;
        if slice_height <= 0 || slice_height > i32::MAX - coded_height {
            return Err(Error::SliceHeight(slice_height));
        }
        if format.vshift != 0 && slice_height & ((1 << format.vshift) - 1) != 0 {
            return Err(Error::SliceHeightAlign(slice_height));
        }
        let nb_slices = (coded_height + slice_height - 1) / slice_height;
        if nb_slices > i32::MAX / 20 {
            return Err(Error::SliceCount(nb_slices));
        }
        let interlaced = header.flags & 2 != 0;
        if (slice_height >> format.vshift) <= interlaced as i32 {
            return Err(Error::ImpossibleSliceHeight);
        }
        if interlaced {
            let rem = coded_height % slice_height;
            if rem != 0 && (rem >> format.vshift) < 2 {
                return Err(Error::ImpossibleHeight);
            }
        }

        let planes = format.planes;
        let nb = nb_slices as usize;
        if (packet.len() - 36) as u64 <= (nb * planes * 5) as u64 {
            return Err(Error::Invalid);
        }
        let limit = size.wrapping_sub(header_size);
        let mut at = 36;
        let mut first_offset = 0;
        for i in 0..planes {
            let slices = &mut self.slices[i];
            slices.clear();
            let mut offset = le32(packet, at);
            at += 4;
            if offset >= limit {
                return Err(Error::Invalid);
            }
            if i == 0 {
                first_offset = offset;
            }
            for _ in 0..nb - 1 {
                let next_offset = le32(packet, at);
                at += 4;
                if next_offset <= offset || next_offset >= limit {
                    return Err(Error::Invalid);
                }
                let slice_size = next_offset - offset;
                if slice_size < 2 {
                    return Err(Error::Invalid);
                }
                slices.push(Slice { start: (offset + header_size) as usize, size: slice_size });
                offset = next_offset;
            }
            let start = offset + header_size;
            let slice_size = size - start;
            if slice_size < 2 {
                return Err(Error::Invalid);
            }
            slices.push(Slice { start: start as usize, size: slice_size });
        }

        if packet[at] as usize != planes {
            return Err(Error::Invalid);
        }
        at += 1 + nb * planes;

        let table_size = header_size.wrapping_add(first_offset).wrapping_sub(at as u32) as i32;
        if table_size < 2 {
            return Err(Error::Invalid);
        }
        let table = &packet[at..at + table_size as usize];
        self.build_huffman(table, planes, 1 << format.bps)?;

        self.format = format;
        self.interlaced = interlaced;
        self.coded_width = coded_width as usize;
        self.coded_height = coded_height as usize;
        self.slice_height = slice_height as usize;
        self.nb_slices = nb;
        self.ready = true;
        Ok(())
    }

    /// Read the run-length coded code lengths of every plane and build its
    /// tables.
    fn build_huffman(&mut self, table: &[u8], planes: usize, max: usize) -> Result<(), Error> {
        let mut length_count = [0u16; 33];
        let (mut i, mut j) = (0, 0);
        let mut pos = 0;

        while pos < table.len() {
            let byte = table[pos];
            pos += 1;
            let x = (byte & 0x7f) as usize;
            let mut l = 1;
            if byte & 0x80 != 0 {
                if pos >= table.len() {
                    break;
                }
                l += table[pos] as usize;
                pos += 1;
            }
            let k = j + l;
            if k > max || x == 0 || x > 32 {
                return Err(Error::HuffmanCodes);
            }
            length_count[x] += l as u16;
            self.scratch.len[j..k].fill(x as u8);
            j = k;

            if j == max {
                j = 0;
                if !self.vlc[i].build(&mut self.scratch, &mut length_count, max) {
                    return Err(Error::HuffmanBuild);
                }
                i += 1;
                if i == planes {
                    break;
                }
                length_count = [0; 33];
            }
        }
        if i != planes {
            return Err(Error::HuffmanShort);
        }
        Ok(())
    }

    /// Plane `i`: visible width and height, and the number of rows a full
    /// slice has in it.
    fn plane_size(&self, i: usize) -> (usize, usize, usize) {
        let (hs, vs) = self.format.shifts(i);
        (
            ceil_rshift(self.coded_width, hs),
            ceil_rshift(self.coded_height, vs),
            ceil_rshift(self.slice_height, vs),
        )
    }

    /// Rows of slice `j` in plane `i`.
    fn slice_rows(&self, i: usize, j: usize) -> usize {
        let (_, vs) = self.format.shifts(i);
        ceil_rshift(self.slice_height.min(self.coded_height - j * self.slice_height), vs)
    }

    fn check_planes<T>(&self, planes: &[Plane<'_, T>]) -> Result<(), BadPlanes> {
        if !self.ready || planes.len() != self.format.planes {
            return Err(BadPlanes);
        }
        for (i, p) in planes.iter().enumerate() {
            let (width, height, _) = self.plane_size(i);
            let need = p.stride.checked_mul(height - 1).and_then(|n| n.checked_add(width));
            if p.stride < width || need.is_none_or(|n| p.data.len() < n) {
                return Err(BadPlanes);
            }
        }
        Ok(())
    }

    /// Bits per sample of the packet [`Decoder::tables`] accepted.
    pub fn bps(&self) -> u32 {
        self.format.bps
    }

    /// Decode a packet [`Decoder::tables`] accepted, 8 bits per sample.
    /// Returns whether a slice asked for an unknown predictor.
    pub fn decode8(&self, packet: &[u8], planes: &mut [Plane<'_, u8>]) -> Result<bool, BadPlanes> {
        let k = Kernels { left8: pred::left8, gradient8: pred::gradient8, median8: pred::median8, left16: pred::left16 };
        self.decode8_with(packet, planes, k)
    }

    /// Decode a packet [`Decoder::tables`] accepted, more than 8 bits per
    /// sample.
    pub fn decode16(&self, packet: &[u8], planes: &mut [Plane<'_, u16>]) -> Result<bool, BadPlanes> {
        let k = Kernels { left8: pred::left8, gradient8: pred::gradient8, median8: pred::median8, left16: pred::left16 };
        self.decode16_with(packet, planes, k)
    }

    /// [`Decoder::decode8`] for CPUs with AVX2. Same output.
    ///
    /// Safe code can only call this from another function compiled for
    /// AVX2; anyone else has to check the CPU and use `unsafe`.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub fn decode8_avx2(&self, packet: &[u8], planes: &mut [Plane<'_, u8>]) -> Result<bool, BadPlanes> {
        let k = Kernels {
            left8: |row: &mut [u8], acc: u8| pred::avx2::left8(row, acc),
            gradient8: |row: &mut [u8], top: &[u8]| pred::avx2::gradient8(row, top),
            median8: |row: &mut [u8], top: &[u8]| pred::avx2::median8(row, top),
            left16: |row: &mut [u16], mask: u16, acc: u16| pred::avx2::left16(row, mask, acc),
        };
        self.decode8_with(packet, planes, k)
    }

    /// [`Decoder::decode16`] for CPUs with AVX2. Same output.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub fn decode16_avx2(&self, packet: &[u8], planes: &mut [Plane<'_, u16>]) -> Result<bool, BadPlanes> {
        let k = Kernels {
            left8: |row: &mut [u8], acc: u8| pred::avx2::left8(row, acc),
            gradient8: |row: &mut [u8], top: &[u8]| pred::avx2::gradient8(row, top),
            median8: |row: &mut [u8], top: &[u8]| pred::avx2::median8(row, top),
            left16: |row: &mut [u16], mask: u16, acc: u16| pred::avx2::left16(row, mask, acc),
        };
        self.decode16_with(packet, planes, k)
    }

    fn decode8_with<A, B, C, D>(
        &self,
        packet: &[u8],
        planes: &mut [Plane<'_, u8>],
        kern: Kernels<A, B, C, D>,
    ) -> Result<bool, BadPlanes>
    where
        A: Fn(&mut [u8], u8) + Copy,
        B: Fn(&mut [u8], &[u8]) + Copy,
        C: Fn(&mut [u8], &[u8]) + Copy,
        D: Copy,
    {
        self.check_planes(planes)?;
        if self.format.bps != 8 {
            return Err(BadPlanes);
        }
        let mut unknown = false;
        for j in 0..self.nb_slices {
            let _ = self.slice8(packet, planes, j, &mut unknown, kern);
        }
        Ok(unknown)
    }

    fn decode16_with<A, B, C, D>(
        &self,
        packet: &[u8],
        planes: &mut [Plane<'_, u16>],
        kern: Kernels<A, B, C, D>,
    ) -> Result<bool, BadPlanes>
    where
        A: Copy,
        B: Copy,
        C: Copy,
        D: Fn(&mut [u16], u16, u16) + Copy,
    {
        self.check_planes(planes)?;
        if self.format.bps == 8 {
            return Err(BadPlanes);
        }
        let mut unknown = false;
        for j in 0..self.nb_slices {
            let _ = self.slice16(packet, planes, j, &mut unknown, kern);
        }
        Ok(unknown)
    }

    fn slice8<A, B, C, D>(
        &self,
        packet: &[u8],
        planes: &mut [Plane<'_, u8>],
        j: usize,
        unknown: &mut bool,
        kern: Kernels<A, B, C, D>,
    ) -> Result<(), SliceError>
    where
        A: Fn(&mut [u8], u8) + Copy,
        B: Fn(&mut [u8], &[u8]) + Copy,
        C: Fn(&mut [u8], &[u8]) + Copy,
    {
        let step = 1 + self.interlaced as usize;        // rows between a row and its "top"

        for i in 0..self.format.planes {
            let (width, _, sheight) = self.plane_size(i);
            let height = self.slice_rows(i, j);
            let stride = planes[i].stride;
            let data = &mut planes[i].data[j * sheight * stride..];
            let sl = self.slices[i][j];
            let slice = &packet[sl.start..];
            let (flags, pred) = (slice[0], slice[1]);
            let slice = &slice[2..];

            let known = matches!(pred, LEFT | GRADIENT | MEDIAN);
            // Run the predictor over row `k`; the rows above it are done.
            let predict = |data: &mut [u8], k: usize| {
                if k < step {
                    (kern.left8)(&mut data[k * stride..][..width], 0);
                } else {
                    let (above, cur) = data.split_at_mut(k * stride);
                    let top = &above[(k - step) * stride..][..width];
                    let cur = &mut cur[..width];
                    match pred {
                        LEFT => (kern.left8)(cur, top[0]),
                        GRADIENT => (kern.gradient8)(cur, top),
                        _ => (kern.median8)(cur, top),
                    }
                }
            };

            if flags & 1 != 0 {
                if sl.size - 2 < (width as u32).wrapping_mul(height as u32) {
                    return Err(SliceError);
                }
                // Nothing can go wrong from here, so each row is predicted
                // as soon as it is copied, while it is still in the cache.
                // (The C decoder copies all rows first.)
                for k in 0..height {
                    data[k * stride..][..width].copy_from_slice(&slice[k * width..][..width]);
                    if known {
                        predict(data, k);
                    }
                }
            } else {
                let mut gb = BitReader::new(slice, sl.size - 2).ok_or(SliceError)?;
                let vlc = &self.vlc[i];
                for k in 0..height {
                    let dst = &mut data[k * stride..][..width];
                    let mut x = 0;
                    while x + 7 < width && gb.left() > 0 {
                        match gb.read_vlc_multi(vlc) {
                            Multi::Joint(elem, num) => {
                                dst[x..x + 8].copy_from_slice(&elem.to_le_bytes());
                                x += num;
                            }
                            Multi::Single(code, ok) => {
                                dst[x] = code as u8;
                                if !ok {
                                    return Err(SliceError);
                                }
                                x += 1;
                            }
                        }
                    }
                    while x < width && gb.left() > 0 {
                        dst[x] = gb.read_vlc(vlc) as u8;
                        x += 1;
                    }
                }
                // A slice can still end in an error up to here, and then
                // none of its rows are predicted.
                if known {
                    for k in 0..height {
                        predict(data, k);
                    }
                }
            }
            if !known {
                *unknown = true;
            }
        }

        if self.format.rgb {
            let height = self.slice_rows(0, j);
            let width = self.coded_width;
            let [b, g, r, ..] = planes else { return Err(SliceError) };
            for row in j * self.slice_height..j * self.slice_height + height {
                let g = &g.data[row * g.stride..][..width];
                pred::add8(&mut b.data[row * b.stride..][..width], g);
                pred::add8(&mut r.data[row * r.stride..][..width], g);
            }
        }
        Ok(())
    }

    fn slice16<A, B, C, D>(
        &self,
        packet: &[u8],
        planes: &mut [Plane<'_, u16>],
        j: usize,
        unknown: &mut bool,
        kern: Kernels<A, B, C, D>,
    ) -> Result<(), SliceError>
    where
        D: Fn(&mut [u16], u16, u16) + Copy,
    {
        let step = 1 + self.interlaced as usize;
        let bps = self.format.bps;
        let max = ((1u32 << bps) - 1) as u16;

        for i in 0..self.format.planes {
            let (width, _, sheight) = self.plane_size(i);
            let height = self.slice_rows(i, j);
            let stride = planes[i].stride;
            let data = &mut planes[i].data[j * sheight * stride..];
            let sl = self.slices[i][j];
            let mut gb = BitReader::new(&packet[sl.start..], sl.size).ok_or(SliceError)?;
            let flags = gb.read(8);
            let pred = gb.read(8) as u8;

            let known = matches!(pred, LEFT | GRADIENT | MEDIAN);
            let predict = |data: &mut [u16], k: usize| {
                if k < step {
                    (kern.left16)(&mut data[k * stride..][..width], max, 0);
                } else {
                    let (above, cur) = data.split_at_mut(k * stride);
                    let top = &above[(k - step) * stride..][..width];
                    let cur = &mut cur[..width];
                    match pred {
                        LEFT => (kern.left16)(cur, max, top[0]),
                        GRADIENT => pred::gradient16(cur, top, max),
                        _ => pred::median16(cur, top, max),
                    }
                }
            };

            if flags & 1 != 0 {
                if (gb.left() as i64) < bps as i64 * width as i64 * height as i64 {
                    return Err(SliceError);
                }
                // Nothing can go wrong from here, so each row is predicted
                // as soon as it is read, while it is still in the cache.
                for k in 0..height {
                    for d in &mut data[k * stride..][..width] {
                        *d = gb.read(bps) as u16;
                    }
                    if known {
                        predict(data, k);
                    }
                }
            } else {
                let vlc = &self.vlc[i];
                for k in 0..height {
                    let dst = &mut data[k * stride..][..width];
                    let mut x = 0;
                    while x + 3 < width && gb.left() > 0 {
                        match gb.read_vlc_multi(vlc) {
                            Multi::Joint(elem, num) => {
                                for (d, s) in dst[x..x + 4].iter_mut().zip(elem.to_le_bytes().as_chunks::<2>().0) {
                                    *d = u16::from_le_bytes(*s);
                                }
                                x += num;
                            }
                            Multi::Single(code, ok) => {
                                dst[x] = code as u16;
                                if !ok {
                                    return Err(SliceError);
                                }
                                x += 1;
                            }
                        }
                    }
                    while x < width && gb.left() > 0 {
                        dst[x] = gb.read_vlc(vlc) as u16;
                        x += 1;
                    }
                }
                // A slice can still end in an error up to here, and then
                // none of its rows are predicted.
                if known {
                    for k in 0..height {
                        predict(data, k);
                    }
                }
            }
            if !known {
                *unknown = true;
            }
        }

        if self.format.rgb {
            let height = self.slice_rows(0, j);
            let width = self.coded_width;
            let [r, g, b, ..] = planes else { return Err(SliceError) };
            for row in j * self.slice_height..j * self.slice_height + height {
                let g = &g.data[row * g.stride..][..width];
                pred::add16(&mut b.data[row * b.stride..][..width], g, max);
                pred::add16(&mut r.data[row * r.stride..][..width], g, max);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
