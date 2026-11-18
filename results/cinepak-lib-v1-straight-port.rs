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
        Some(Frame { data, stride, height })
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

fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn decode_codebook(codebook: &mut Codebook, chunk_id: u8, mut data: &[u8]) {
    // 4-element vectors are greyscale; 6-element ones add two chroma bytes.
    let n = if chunk_id & 0x04 != 0 { 4 } else { 6 };
    let selective = chunk_id & 0x01 != 0;
    let mut flags = Flags { flag: 0, mask: 0 };

    for entry in codebook.iter_mut() {
        if selective && flags.advance(&mut data).is_none() {
            break;
        }
        if selective && !flags.set() {
            continue;
        }
        let Some((v, rest)) = data.split_at_checked(n) else {
            break;
        };
        data = rest;
        for (px, &y) in entry.chunks_exact_mut(3).zip(v) {
            if n == 6 {
                let y = i32::from(y);
                let u = i32::from(v[4] as i8);
                let w = i32::from(v[5] as i8);
                px[0] = clip_u8(y + w * 2);
                px[1] = clip_u8(y - u / 2 - w);
                px[2] = clip_u8(y + u * 2);
            } else {
                px.fill(y);
            }
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

/// The four rows of a block, top to bottom, in the frame's pixel format.
fn block_rows<const N: usize>(top: [&[u8]; 2], bottom: [&[u8]; 2]) -> [[u8; N]; 4] {
    let mut rows = [[0u8; N]; 4];
    let half = N / 2;
    if N == 12 {
        // RGB: each source slice is two pixels wide (6 bytes).
        for (r, pair) in [(0, top), (2, bottom)] {
            rows[r][..half].copy_from_slice(&pair[0][..half]);
            rows[r][half..].copy_from_slice(&pair[1][..half]);
            rows[r + 1][..half].copy_from_slice(&pair[0][half..]);
            rows[r + 1][half..].copy_from_slice(&pair[1][half..]);
        }
    } else {
        // Palette: the index is the first byte of each RGB triple.
        for (r, pair) in [(0, top), (2, bottom)] {
            for (c, cb) in pair.iter().enumerate() {
                rows[r][2 * c] = cb[0];
                rows[r][2 * c + 1] = cb[3];
                rows[r + 1][2 * c] = cb[6];
                rows[r + 1][2 * c + 1] = cb[9];
            }
        }
    }
    rows
}

/// A block of four doubled pixels from one V1 codebook entry.
fn v1_rows<const N: usize>(p: &Entry) -> [[u8; N]; 4] {
    let mut rows = [[0u8; N]; 4];
    let px = N / 4;
    for (r, pair) in [(0, [&p[0..3], &p[3..6]]), (2, [&p[6..9], &p[9..12]])] {
        for (c, colour) in pair.iter().enumerate() {
            for k in 0..2 {
                let at = (2 * c + k) * px;
                rows[r][at..at + px].copy_from_slice(&colour[..px]);
            }
        }
        rows[r + 1] = rows[r];
    }
    rows
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
        vectors::<4>(strip, rect, chunk_id, data, frame)
    } else {
        vectors::<12>(strip, rect, chunk_id, data, frame)
    }
}

/// `N` is the number of bytes in one row of a block: 4 for palette video,
/// 12 for RGB.
fn vectors<const N: usize>(
    strip: &Strip,
    rect: &Rect,
    chunk_id: u8,
    mut data: &[u8],
    frame: &mut Frame<'_>,
) -> Option<()> {
    let selective = chunk_id & 0x01 != 0;
    let all_v1 = chunk_id & 0x02 != 0;
    let mut flags = Flags { flag: 0, mask: 0 };
    let bpp = N / 4;

    for y in (rect.y1..rect.y2).step_by(4) {
        for x in (rect.x1..rect.x2).step_by(4) {
            if selective {
                flags.advance(&mut data)?;
                if !flags.set() {
                    continue;
                }
            }
            if !all_v1 {
                flags.advance(&mut data)?;
            }
            let rows: [[u8; N]; 4] = if all_v1 || !flags.set() {
                let (&index, rest) = data.split_first()?;
                data = rest;
                v1_rows(&strip.v1[usize::from(index)])
            } else {
                let (index, rest) = data.split_first_chunk::<4>()?;
                data = rest;
                let cb = index.map(|i| &strip.v4[usize::from(i)]);
                block_rows([cb[0], cb[1]], [cb[2], cb[3]])
            };
            // Bottom row first, as the C code does: when a block sticks out
            // past the end of a row, the order decides which write wins.
            for r in (0..4).rev() {
                if y + r < frame.height {
                    frame.put((y + r) * frame.stride + x * bpp, &rows[r]);
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
