//! Safe Rust port of FFmpeg's Cinepak video decoder (`cinepak.c`).
//!
//! The port is bit-exact with the C decoder, including two things the C
//! decoder gets from the layout of FFmpeg's frame buffers rather than from
//! the bitstream:
//!
//! * A strip may start at any x, so its last 4-pixel block can stick out
//!   past the end of a row. The C code writes it anyway, through a plain
//!   pointer: the extra pixels land in the row's padding or, when the row
//!   has no padding, at the start of the next row. The port writes to the
//!   same byte offsets, so the same pixels change, but through a bounds
//!   check: what would fall outside the visible picture is dropped.
//! * Blocks below the bottom edge are written to rows the C code expects
//!   the allocator to have added. The port drops those rows.

#![forbid(unsafe_code)]
#![no_std]

const MAX_STRIPS: usize = 32;

/// One codebook entry: four pixels as RGB triples. In palette mode only the
/// first byte of each triple is used.
type Entry = [u8; 12];
type Codebook = [Entry; 256];

struct Strip {
    v4: Codebook,
    v1: Codebook,
}

/// What to do with a packet, decided before a frame is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precheck {
    /// Go on and decode.
    Decode,
    /// Shorter than a frame header: invalid.
    TooSmall,
    /// A frame with no strips: valid, but there is no picture to output.
    Empty,
    /// The header is inconsistent with the packet: invalid.
    Invalid,
    /// The first frame claims an encoded size of zero: unsupported.
    ZeroEncodedSize,
}

/// A strip or chunk was damaged. The picture is output anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Damaged;

/// The visible picture: `height` rows, `stride` bytes apart, the last one
/// cut off after the last visible pixel. Row 0 is the top row.
pub struct Frame<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_len: usize,
    height: usize,
}

impl<'a> Frame<'a> {
    /// `row_len` is the number of bytes in a visible row.
    pub fn new(data: &'a mut [u8], stride: usize, row_len: usize, height: usize) -> Option<Self> {
        if row_len == 0 || height == 0 || stride < row_len {
            return None;
        }
        let need = stride.checked_mul(height - 1)?.checked_add(row_len)?;
        let data = data.get_mut(..need)?;
        Some(Frame { data, stride, row_len, height })
    }

    /// Write `src` at byte offset `off`, as the C code does through a raw
    /// pointer, except that bytes past the end of the picture are dropped.
    fn put(&mut self, off: usize, src: &[u8]) {
        if let Some(dst) = self.data.get_mut(off..) {
            let n = dst.len().min(src.len());
            dst[..n].copy_from_slice(&src[..n]);
        }
    }
}

pub struct Decoder {
    /// Picture size rounded up to whole 4x4 blocks; strips must fit in it.
    width: usize,
    height: usize,
    palette_video: bool,
    strips: [Strip; MAX_STRIPS],
    /// Extra bytes after the frame header in Sega FILM files; decided on
    /// the first frame.
    sega_film_skip_bytes: Option<usize>,
}

fn be16(d: &[u8], at: usize) -> usize {
    usize::from(d[at]) << 8 | usize::from(d[at + 1])
}

fn be24(d: &[u8], at: usize) -> usize {
    usize::from(d[at]) << 16 | usize::from(d[at + 1]) << 8 | usize::from(d[at + 2])
}

/// The flag bits that say which blocks or codebook entries are coded.
struct Flags {
    flag: u32,
    mask: u32,
}

impl Flags {
    /// Move to the next bit, reading another 32 from the stream when the
    /// current word is used up. `None` if the stream has run out.
    fn advance(&mut self, data: &mut &[u8]) -> Option<()> {
        self.mask >>= 1;
        if self.mask == 0 {
            let (word, rest) = data.split_first_chunk::<4>()?;
            *data = rest;
            self.flag = u32::from_be_bytes(*word);
            self.mask = 0x8000_0000;
        }
        Some(())
    }

    fn set(&self) -> bool {
        self.flag & self.mask != 0
    }
}

fn decode_codebook(codebook: &mut Codebook, chunk_id: u8, mut data: &[u8]) {
    // 4-element vectors are greyscale; 6-element ones add two chroma bytes.
    let grey = chunk_id & 0x04 != 0;
    let selective = chunk_id & 0x01 != 0;
    let mut flags = Flags { flag: 0, mask: 0 };

    for entry in codebook.iter_mut() {
        if selective && flags.advance(&mut data).is_none() {
            break;
        }
        if selective && !flags.set() {
            continue;
        }
        if grey {
            let Some((y, rest)) = data.split_first_chunk::<4>() else {
                break;
            };
            data = rest;
            *entry = [y[0], y[0], y[0], y[1], y[1], y[1], y[2], y[2], y[2], y[3], y[3], y[3]];
        } else {
            let Some((v, rest)) = data.split_first_chunk::<6>() else {
                break;
            };
            data = rest;
            // y + chroma offsets, clipped to a byte. Everything fits in 16
            // bits, which lets the compiler do all twelve at once.
            let u = i16::from(v[4] as i8);
            let w = i16::from(v[5] as i8);
            let d = [w * 2, -(u / 2) - w, u * 2];
            let mut out = [0u8; 12];
            for (i, o) in out.iter_mut().enumerate() {
                *o = (i16::from(v[i / 3]) + d[i % 3]).clamp(0, 255) as u8;
            }
            *entry = out;
        }
    }
}

struct Rect {
    x1: usize,
    y1: usize,
    x2: usize,
    y2: usize,
}

impl Decoder {
    /// `bits` is the stream's bits per coded sample: 8 means palette video.
    pub fn new(width: usize, height: usize, bits: i32) -> Self {
        const EMPTY: Strip = Strip { v4: [[0; 12]; 256], v1: [[0; 12]; 256] };
        Decoder {
            width: width.saturating_add(3) & !3,
            height: height.saturating_add(3) & !3,
            palette_video: bits == 8,
            strips: [EMPTY; MAX_STRIPS],
            sega_film_skip_bytes: None,
        }
    }

    pub fn palette_video(&self) -> bool {
        self.palette_video
    }

    /// The checks the C decoder makes before it asks for a frame.
    ///
    /// `discard_damaged_percentage` is the codec context option of that
    /// name; `has_palette` says whether the packet carries a new palette.
    pub fn precheck(
        &mut self,
        packet: &[u8],
        discard_damaged_percentage: i32,
        has_palette: bool,
    ) -> Precheck {
        if packet.len() < 10 {
            return Precheck::TooSmall;
        }
        let num_strips = be16(packet, 8);
        // Empty frame: nothing to do unless it delivers a palette.
        if num_strips == 0 && !(self.palette_video && has_palette) {
            return Precheck::Empty;
        }
        let size = packet.len() as i64;
        let encoded = be24(packet, 1);

        if size < encoded as i64 * i64::from(100 - discard_damaged_percentage) / 100 {
            return Precheck::Invalid;
        }

        let skip = match self.sega_film_skip_bytes {
            Some(skip) => skip,
            None => {
                if encoded == 0 {
                    return Precheck::ZeroEncodedSize;
                }
                // A container frame that is not a whole number of encoded
                // frames comes from a Sega FILM file, which has 2 (or, in
                // two known files, 6) extra bytes after the header.
                let skip = if encoded != packet.len() && packet.len() % encoded != 0 {
                    if packet.get(10..16) == Some(&[0xfe, 0x00, 0x00, 0x06, 0x00, 0x00]) {
                        6
                    } else {
                        2
                    }
                } else {
                    0
                };
                self.sega_film_skip_bytes = Some(skip);
                skip
            }
        };

        if packet.len() < 10 + skip + num_strips * 12 {
            return Precheck::Invalid;
        }
        if num_strips != 0 {
            let strip_size = be24(packet, 10 + skip + 1);
            if strip_size < 12 || strip_size > encoded {
                return Precheck::Invalid;
            }
        }
        Precheck::Decode
    }

    /// Decode a packet that passed [`Decoder::precheck`] onto `frame`,
    /// which holds the previous picture. Returns whether the frame is a
    /// key frame and whether the data was damaged.
    pub fn decode(&mut self, packet: &[u8], frame: &mut Frame<'_>) -> (bool, Result<(), Damaged>) {
        let mut key = false;
        let result = self.decode_strips(packet, frame, &mut key);
        (key, result.ok_or(Damaged))
    }

    fn decode_strips(&mut self, packet: &[u8], frame: &mut Frame<'_>, key: &mut bool) -> Option<()> {
        let skip = self.sega_film_skip_bytes.unwrap_or(0);
        let header = packet.get(..10)?;
        let frame_flags = header[0];
        let num_strips = be16(header, 8).min(MAX_STRIPS);
        let mut data = packet.get(10 + skip..)?;
        let mut y0 = 0;

        for i in 0..num_strips {
            let (head, rest) = data.split_first_chunk::<12>()?;
            data = rest;

            // A zero y1 means "relative to the previous strip". The C code
            // keeps these in 16-bit fields, so the sum wraps.
            let mut rect = Rect {
                x1: be16(head, 6),
                y1: be16(head, 4),
                x2: be16(head, 10),
                y2: be16(head, 8),
            };
            if rect.y1 == 0 {
                rect.y1 = y0;
                rect.y2 = (y0 + rect.y2) & 0xffff;
            }
            if head[0] == 0x10 {
                *key = true;
            }

            let strip_size = be24(head, 1).checked_sub(12)?.min(data.len());
            let (strip_data, rest) = data.split_at(strip_size);

            if i > 0 && frame_flags & 0x01 == 0 {
                let (before, from) = self.strips.split_at_mut(i);
                from[0].v4 = before[i - 1].v4;
                from[0].v1 = before[i - 1].v1;
            }

            self.decode_strip(i, &rect, strip_data, frame)?;

            data = rest;
            y0 = rect.y2;
        }
        Some(())
    }

    fn decode_strip(&mut self, i: usize, rect: &Rect, mut data: &[u8], frame: &mut Frame<'_>) -> Option<()> {
        if rect.x2 > self.width || rect.y2 > self.height || rect.x1 >= rect.x2 || rect.y1 >= rect.y2 {
            return None;
        }
        let palette_video = self.palette_video;
        let strip = &mut self.strips[i];

        while let Some((head, rest)) = data.split_first_chunk::<4>() {
            let chunk_id = head[0];
            let chunk_size = be24(head, 1).checked_sub(4)?.min(rest.len());
            let (chunk, rest) = rest.split_at(chunk_size);

            match chunk_id {
                0x20 | 0x21 | 0x24 | 0x25 => decode_codebook(&mut strip.v4, chunk_id, chunk),
                0x22 | 0x23 | 0x26 | 0x27 => decode_codebook(&mut strip.v1, chunk_id, chunk),
                0x30..=0x32 => {
                    return decode_vectors(strip, rect, chunk_id, chunk, frame, palette_video);
                }
                _ => {}
            }
            data = rest;
        }
        // A strip without a vector chunk.
        None
    }
}

/// How a block is laid out in one of the two pixel formats.
trait Format {
    /// One row of a 4-pixel block.
    type Row: Copy + AsRef<[u8]>;
    const ZERO: Self::Row;
    /// Bytes per pixel.
    const BPP: usize;
    /// A row of the picture as whole blocks; a partial block at the end is
    /// left out.
    fn blocks(row: &mut [u8]) -> &mut [Self::Row];
    /// Write a block of four doubled pixels from one entry.
    fn put_v1(p: &Entry, rows: [&mut Self::Row; 4]);
    /// Write a block of sixteen pixels from four entries.
    fn put_v4(cb: [&Entry; 4], rows: [&mut Self::Row; 4]);
}

struct Rgb;
struct Pal;

/// Store twelve bytes as a 64-bit and a 32-bit little-endian word: two
/// stores instead of four 3-byte copies.
#[inline(always)]
fn store12(dst: &mut [u8; 12], lo: u64, hi: u32) {
    let (a, b) = dst.split_at_mut(8);
    a.copy_from_slice(&lo.to_le_bytes());
    b.copy_from_slice(&hi.to_le_bytes());
}

/// The little-endian word at bytes `at .. at + 4` / `at .. at + 8` of an entry.
#[inline(always)]
fn le32(e: &Entry, at: usize) -> u32 {
    u32::from_le_bytes([e[at], e[at + 1], e[at + 2], e[at + 3]])
}

#[inline(always)]
fn le64(e: &Entry, at: usize) -> u64 {
    u64::from(le32(e, at)) | u64::from(le32(e, at + 4)) << 32
}

impl Format for Rgb {
    type Row = [u8; 12];
    const ZERO: Self::Row = [0; 12];
    const BPP: usize = 3;

    fn blocks(row: &mut [u8]) -> &mut [Self::Row] {
        row.as_chunks_mut::<12>().0
    }

    #[inline(always)]
    fn put_v1(p: &Entry, [r0, r1, r2, r3]: [&mut Self::Row; 4]) {
        // Two RGB pixels (24 bits each), each doubled: a a b b.
        let doubled = |a: u32, b: u32| {
            let (a, b) = (u64::from(a), u64::from(b));
            (a | a << 24 | b << 48, (b >> 16 | b << 8) as u32)
        };
        let (lo, hi) = doubled(le32(p, 0) & 0xff_ffff, le32(p, 3) & 0xff_ffff);
        store12(r0, lo, hi);
        store12(r1, lo, hi);
        let (lo, hi) = doubled(le32(p, 6) & 0xff_ffff, le32(p, 8) >> 8);
        store12(r2, lo, hi);
        store12(r3, lo, hi);
    }

    #[inline(always)]
    fn put_v4(cb: [&Entry; 4], [r0, r1, r2, r3]: [&mut Self::Row; 4]) {
        // The first and the second half of an entry, in the low 48 bits.
        let first = |e: &Entry| le64(e, 0) & 0xffff_ffff_ffff;
        let second = |e: &Entry| le64(e, 4) >> 16;
        let join = |dst: &mut Self::Row, a: u64, b: u64| store12(dst, a | b << 48, (b >> 16) as u32);
        join(r0, first(cb[0]), first(cb[1]));
        join(r1, second(cb[0]), second(cb[1]));
        join(r2, first(cb[2]), first(cb[3]));
        join(r3, second(cb[2]), second(cb[3]));
    }
}

/// Palette video: the index is the first byte of each RGB triple.
impl Format for Pal {
    type Row = [u8; 4];
    const ZERO: Self::Row = [0; 4];
    const BPP: usize = 1;

    fn blocks(row: &mut [u8]) -> &mut [Self::Row] {
        row.as_chunks_mut::<4>().0
    }

    #[inline(always)]
    fn put_v1(p: &Entry, [r0, r1, r2, r3]: [&mut Self::Row; 4]) {
        let pair = |a: u8, b: u8| (u32::from(a) * 0x0101 | u32::from(b) * 0x0101_0000).to_le_bytes();
        let top = pair(p[0], p[3]);
        let bottom = pair(p[6], p[9]);
        *r0 = top;
        *r1 = top;
        *r2 = bottom;
        *r3 = bottom;
    }

    #[inline(always)]
    fn put_v4(cb: [&Entry; 4], [r0, r1, r2, r3]: [&mut Self::Row; 4]) {
        let row = |a: &Entry, b: &Entry, at: usize| {
            u32::from_le_bytes([a[at], a[at + 3], b[at], b[at + 3]]).to_le_bytes()
        };
        *r0 = row(cb[0], cb[1], 0);
        *r1 = row(cb[0], cb[1], 6);
        *r2 = row(cb[2], cb[3], 0);
        *r3 = row(cb[2], cb[3], 6);
    }
}

fn decode_vectors(
    strip: &Strip,
    rect: &Rect,
    chunk_id: u8,
    data: &[u8],
    frame: &mut Frame<'_>,
    palette_video: bool,
) -> Option<()> {
    if palette_video {
        vectors::<Pal>(strip, rect, chunk_id, data, frame)
    } else {
        vectors::<Rgb>(strip, rect, chunk_id, data, frame)
    }
}

/// What the stream says about one block.
enum Block<'a> {
    /// Not coded: the block keeps the previous frame's pixels.
    Skip,
    V1(&'a Entry),
    V4([&'a Entry; 4]),
}

impl Block<'_> {
    #[inline(always)]
    fn put<F: Format>(&self, rows: [&mut F::Row; 4]) {
        match self {
            Block::Skip => {}
            Block::V1(p) => F::put_v1(p, rows),
            Block::V4(cb) => F::put_v4(*cb, rows),
        }
    }
}

/// The vector chunk as the decoder reads it: flag words and codebook
/// indices interleaved.
struct Vectors<'a> {
    data: &'a [u8],
    flags: Flags,
    selective: bool,
    all_v1: bool,
    strip: &'a Strip,
}

impl<'a> Vectors<'a> {
    /// The next block, or `None` if the stream has run out.
    #[inline(always)]
    fn next(&mut self) -> Option<Block<'a>> {
        if self.selective {
            self.flags.advance(&mut self.data)?;
            if !self.flags.set() {
                return Some(Block::Skip);
            }
        }
        if !self.all_v1 {
            self.flags.advance(&mut self.data)?;
        }
        if self.all_v1 || !self.flags.set() {
            let (&index, rest) = self.data.split_first()?;
            self.data = rest;
            Some(Block::V1(&self.strip.v1[usize::from(index)]))
        } else {
            let (index, rest) = self.data.split_first_chunk::<4>()?;
            self.data = rest;
            Some(Block::V4(index.map(|i| &self.strip.v4[usize::from(i)])))
        }
    }
}

/// The whole blocks of the visible part of `row`, from byte `from` on.
fn whole_blocks<F: Format>(row: &mut [u8], from: usize, row_len: usize) -> &mut [F::Row] {
    F::blocks(&mut row[from..row_len])
}

fn vectors<F: Format>(
    strip: &Strip,
    rect: &Rect,
    chunk_id: u8,
    data: &[u8],
    frame: &mut Frame<'_>,
) -> Option<()> {
    let mut vectors = Vectors {
        data,
        flags: Flags { flag: 0, mask: 0 },
        selective: chunk_id & 0x01 != 0,
        all_v1: chunk_id & 0x02 != 0,
        strip,
    };
    let blocks_per_row = (rect.x2 - rect.x1).div_ceil(4);
    let (stride, row_len) = (frame.stride, frame.row_len);
    let from = rect.x1 * F::BPP;

    for y in (rect.y1..rect.y2).step_by(4) {
        let mut done = 0;

        // Blocks that lie wholly inside the visible picture are written
        // straight into the four rows.
        if y + 4 <= frame.height && from <= row_len {
            let band = &mut frame.data[y * stride..];
            let (r0, rest) = band.split_at_mut(stride);
            let (r1, rest) = rest.split_at_mut(stride);
            let (r2, r3) = rest.split_at_mut(stride);
            let rows = whole_blocks::<F>(r0, from, row_len)
                .iter_mut()
                .zip(whole_blocks::<F>(r1, from, row_len))
                .zip(whole_blocks::<F>(r2, from, row_len))
                .zip(whole_blocks::<F>(r3, from, row_len));
            for (((b0, b1), b2), b3) in rows.take(blocks_per_row) {
                done += 1;
                vectors.next()?.put::<F>([b0, b1, b2, b3]);
            }
        }

        // The rest (a block that sticks out past the end of a row, or a
        // band at the bottom edge) is written the way the C code does it,
        // by byte offset, bottom row first: when a block runs past the end
        // of a row the order decides which write wins.
        for i in done..blocks_per_row {
            let x = rect.x1 + 4 * i;
            let block = vectors.next()?;
            if matches!(block, Block::Skip) {
                continue;
            }
            let mut rows = [F::ZERO; 4];
            block.put::<F>(rows.each_mut());
            for r in (0..4).rev() {
                if y + r < frame.height {
                    frame.put((y + r) * stride + x * F::BPP, rows[r].as_ref());
                }
            }
        }
    }
    Some(())
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;
    use std::vec;
    use std::vec::Vec;

    fn be24_bytes(v: usize) -> [u8; 3] {
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    }

    /// One strip covering a 4x4 picture: a greyscale V4 codebook with
    /// entries 0 and 1, then one V1 block.
    fn tiny_frame() -> Vec<u8> {
        let mut strip = vec![];
        let cb = [1u8, 2, 3, 4, 5, 6, 7, 8];
        strip.push(0x24);
        strip.extend(be24_bytes(4 + cb.len()));
        strip.extend(cb);
        strip.push(0x32); // every block V1
        strip.extend(be24_bytes(4 + 1));
        strip.push(0);

        let mut pkt = vec![0x00];
        pkt.extend(be24_bytes(10 + 12 + strip.len()));
        pkt.extend([0, 4, 0, 4, 0, 1]);
        pkt.push(0x10);
        pkt.extend(be24_bytes(12 + strip.len()));
        pkt.extend([0, 0, 0, 0, 0, 4, 0, 4]);
        pkt.extend(strip);
        pkt
    }

    #[test]
    fn empty_and_short_packets() {
        let mut d = Box::new(Decoder::new(4, 4, 24));
        assert_eq!(d.precheck(&[0; 9], 95, false), Precheck::TooSmall);
        assert_eq!(d.precheck(&[0; 10], 95, false), Precheck::Empty);
    }

    #[test]
    fn one_v1_block_key_frame() {
        let pkt = tiny_frame();
        let mut d = Box::new(Decoder::new(4, 4, 24));
        assert_eq!(d.precheck(&pkt, 95, false), Precheck::Decode);
        let mut buf = [0u8; 64];
        let mut frame = Frame::new(&mut buf, 16, 12, 4).unwrap();
        // The V1 codebook is empty, so block 0 is black; the frame is a key
        // frame and decodes cleanly.
        let (key, result) = d.decode(&pkt, &mut frame);
        assert!(key);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn v4_codebook_is_grey() {
        let mut cb = Box::new([[0u8; 12]; 256]);
        decode_codebook(&mut cb, 0x24, &[9, 8, 7, 6]);
        assert_eq!(cb[0], [9, 9, 9, 8, 8, 8, 7, 7, 7, 6, 6, 6]);
    }
}
