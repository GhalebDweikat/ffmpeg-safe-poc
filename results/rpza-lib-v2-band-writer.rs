//! Safe Rust port of FFmpeg's Apple RPZA ("Road Pizza") video decoder,
//! `libavcodec/rpza.c` at release n8.1.3.
//!
//! The whole decoder (bitstream parsing and every pixel write) lives in this
//! crate, which contains no `unsafe` and needs no allocator. It is meant to
//! produce exactly the picture the C decoder produces for every input,
//! including malformed ones, where the C decoder stops part-way and leaves
//! the frame partially updated.
//!
//! One deliberate difference: the C decoder always writes whole 4x4 blocks
//! and relies on FFmpeg's frame allocator padding the buffer when the width
//! or height is not a multiple of 4. This port clips edge blocks to the
//! visible image, so it never depends on that padding existing.

#![no_std]
#![forbid(unsafe_code)]

/// Why a packet was rejected. The C decoder returns `AVERROR_INVALIDDATA`
/// for all of these; the variants only preserve its distinct log messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The packet is too small to plausibly hold a frame of this size.
    PacketTooSmall,
    /// A four-colour run announced more index bytes than the packet holds.
    FourColorTruncated,
    /// A sixteen-colour block needs 30 more bytes than the packet holds.
    SixteenColorTruncated,
    /// A sixteen-colour block arrived after the last block of the frame.
    NoBlocksLeft,
    /// An opcode with its top three bits set (0xE0..=0xFF).
    UnknownOpcode(u8),
}

/// The two header fields the C decoder inspects only to print warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// First byte of the packet (0 if the packet is empty). Should be 0xE1.
    pub first_byte: u8,
    /// The 24-bit chunk size stored in the packet.
    pub chunk_size: u32,
    /// Bytes that follow the 4-byte header.
    pub payload_len: usize,
}

/// Mirrors FFmpeg's checked `bytestream2` reader: reading past the end
/// yields 0 and consumes whatever was left.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    #[inline(always)]
    fn left(&self) -> usize {
        self.buf.len()
    }

    #[inline(always)]
    fn peek_u8(&self) -> u8 {
        self.buf.first().copied().unwrap_or(0)
    }

    #[inline(always)]
    fn take<const N: usize>(&mut self) -> Option<&'a [u8; N]> {
        let (head, rest) = self.buf.split_first_chunk::<N>()?;
        self.buf = rest;
        Some(head)
    }

    #[inline(always)]
    fn take_slice(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.buf.len() {
            return None;
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Some(head)
    }

    /// Checked read: on a short buffer, consume the remainder and return 0.
    #[inline(always)]
    fn get<const N: usize>(&mut self) -> [u8; N] {
        match self.take::<N>() {
            Some(bytes) => *bytes,
            None => {
                self.buf = &[];
                [0; N]
            }
        }
    }

    #[inline(always)]
    fn get_u8(&mut self) -> u8 {
        self.get::<1>()[0]
    }

    #[inline(always)]
    fn get_be16(&mut self) -> u16 {
        u16::from_be_bytes(self.get())
    }

    #[inline(always)]
    fn get_be32(&mut self) -> u32 {
        u32::from_be_bytes(self.get())
    }
}

/// Read the packet header fields the C decoder warns about.
pub fn header(packet: &[u8]) -> Header {
    let mut r = Reader::new(packet);
    let first_byte = r.peek_u8();
    let chunk_size = r.get_be32() & 0x00FF_FFFF;
    Header {
        first_byte,
        chunk_size,
        payload_len: r.left(),
    }
}

/// Number of 4x4 blocks in a frame, or `None` if it does not fit in `usize`.
fn block_count(width: usize, height: usize) -> Option<usize> {
    width.div_ceil(4).checked_mul(height.div_ceil(4))
}

/// The early size check the C decoder makes before touching the frame:
/// a packet must carry at least one byte per 32 blocks.
pub fn packet_is_plausible(packet_len: usize, width: usize, height: usize) -> bool {
    match block_count(width, height) {
        Some(blocks) => blocks / 32 <= packet_len.saturating_sub(4),
        None => false,
    }
}

/// One row of a block: four RGB555 pixels in native byte order.
type Row = [u8; 8];

/// Bit position of each of a row's four pixels inside a native-endian `u64`.
#[cfg(target_endian = "little")]
const PIXEL_SHIFT: [u32; 4] = [0, 16, 32, 48];
#[cfg(target_endian = "big")]
const PIXEL_SHIFT: [u32; 4] = [48, 32, 16, 0];

#[inline(always)]
fn row_bytes(px: [u16; 4]) -> Row {
    (u64::from(px[0]) << PIXEL_SHIFT[0]
        | u64::from(px[1]) << PIXEL_SHIFT[1]
        | u64::from(px[2]) << PIXEL_SHIFT[2]
        | u64::from(px[3]) << PIXEL_SHIFT[3])
        .to_ne_bytes()
}

/// One row of a four-colour block: two index bits per pixel, leftmost first.
#[inline(always)]
fn indexed_row(pal: &[u16; 4], bits: u8) -> Row {
    row_bytes([
        pal[usize::from(bits >> 6)],
        pal[usize::from((bits >> 4) & 3)],
        pal[usize::from((bits >> 2) & 3)],
        pal[usize::from(bits & 3)],
    ])
}

/// The four colours of a two-colour block: B, two blends, A.
#[inline(always)]
fn four_colors(a: u16, b: u16) -> [u16; 4] {
    let mut c1 = 0u32;
    let mut c2 = 0u32;
    for shift in [10u32, 5, 0] {
        let ta = (u32::from(a) >> shift) & 0x1f;
        let tb = (u32::from(b) >> shift) & 0x1f;
        c1 |= ((11 * ta + 21 * tb) >> 5) << shift;
        c2 |= ((21 * ta + 11 * tb) >> 5) << shift;
    }
    // Each blended channel is at most 31, so both values fit in 15 bits.
    [b, c1 as u16, c2 as u16, a]
}

/// A mutable view of the visible image: `height` rows of `width` RGB555
/// pixels (native byte order), `stride` bytes apart.
pub struct Frame<'a> {
    data: &'a mut [u8],
    stride: usize,
    /// Bytes of visible pixels per row (`width * 2`).
    row_len: usize,
    /// Block columns and rows covering the image (rounded up).
    blocks_w: usize,
    blocks_h: usize,
    /// Block columns that lie entirely inside the image.
    full_w: usize,
}

impl<'a> Frame<'a> {
    /// `data` must hold at least `stride * (height - 1) + width * 2` bytes.
    /// Returns `None` for an empty image or a buffer that is too small.
    pub fn new(data: &'a mut [u8], stride: usize, width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let row_len = width.checked_mul(2)?;
        if stride < row_len {
            return None;
        }
        let needed = stride.checked_mul(height - 1)?.checked_add(row_len)?;
        // Keep only the visible image, so no later index can reach past it.
        let data = data.get_mut(..needed)?;
        block_count(width, height)?;
        Some(Self {
            data,
            stride,
            row_len,
            blocks_w: width.div_ceil(4),
            blocks_h: height.div_ceil(4),
            full_w: width / 4,
        })
    }
}

/// The four pixel rows of one row of blocks, each cut down to its visible
/// pixels. Rows below the bottom edge of the image are empty.
type Rows<'a> = [&'a mut [u8]; 4];

fn split_band(band: &mut [u8], stride: usize, row_len: usize) -> Rows<'_> {
    let mut rows: Rows<'_> = Default::default();
    for (dst, row) in rows.iter_mut().zip(band.chunks_mut(stride)) {
        let visible = row.len().min(row_len);
        *dst = &mut row[..visible];
    }
    rows
}

/// Copy as much of `src` as fits in `row` at byte offset `x`. Used for
/// blocks that hang over the right or bottom edge of the image.
#[inline]
fn put_clipped(row: &mut [u8], x: usize, src: &Row) {
    let end = x.saturating_add(8).min(row.len());
    if let Some(dst) = row.get_mut(x..end) {
        for (d, s) in dst.iter_mut().zip(src) {
            *d = *s;
        }
    }
}

/// Writes blocks left to right, top to bottom. It walks the image one row
/// of blocks ("band") at a time and only ever holds the current band, so a
/// write can never land outside the image.
struct Writer<'a> {
    bands: core::slice::ChunksMut<'a, u8>,
    rows: Rows<'a>,
    stride: usize,
    row_len: usize,
    /// Column of the next block in the current band.
    bx: usize,
    blocks_w: usize,
    full_w: usize,
    /// Blocks of the frame not yet written or skipped.
    remaining: usize,
}

impl<'a> Writer<'a> {
    fn new(frame: &'a mut Frame<'_>) -> Self {
        let mut w = Writer {
            bands: frame.data.chunks_mut(frame.stride.saturating_mul(4)),
            rows: Default::default(),
            stride: frame.stride,
            row_len: frame.row_len,
            bx: 0,
            blocks_w: frame.blocks_w,
            full_w: frame.full_w,
            remaining: frame.blocks_w * frame.blocks_h,
        };
        w.next_band();
        w
    }

    fn next_band(&mut self) {
        self.rows = match self.bands.next() {
            Some(band) => split_band(band, self.stride, self.row_len),
            None => Default::default(),
        };
    }

    /// Move past `n` blocks; callers keep `n <= self.remaining`.
    #[inline(always)]
    fn advance(&mut self, n: usize) {
        self.remaining -= n;
        self.bx += n;
        while self.bx >= self.blocks_w {
            self.bx -= self.blocks_w;
            self.next_band();
        }
    }

    /// How many of the next `n` blocks are in the current band.
    #[inline(always)]
    fn in_band(&self, n: usize) -> usize {
        n.min(self.blocks_w - self.bx)
    }

    /// Fill the next `n` blocks with one colour.
    #[inline(always)]
    fn fill(&mut self, mut n: usize, color: u16) {
        let px = color.to_ne_bytes();
        while n > 0 {
            let seg = self.in_band(n);
            let x0 = self.bx * 8;
            for row in self.rows.iter_mut() {
                let x1 = (x0 + seg * 8).min(row.len());
                if let Some(dst) = row.get_mut(x0..x1) {
                    for p in dst.chunks_exact_mut(2) {
                        p.copy_from_slice(&px);
                    }
                }
            }
            self.advance(seg);
            n -= seg;
        }
    }

    /// Write blocks from four index bytes each, using the palette `pal`.
    #[inline(always)]
    fn four_color(&mut self, mut indices: &[u8], pal: &[u16; 4]) {
        while !indices.is_empty() {
            let seg = self.in_band(indices.len() / 4);
            let (now, later) = indices.split_at(seg * 4);
            indices = later;
            let x0 = self.bx * 8;

            // Blocks wholly inside the image: no bounds checks in the loop.
            let whole = if self.rows[3].is_empty() {
                0
            } else {
                seg.min(self.full_w.saturating_sub(self.bx))
            };
            let (fast, slow) = now.split_at(whole * 4);
            {
                let [r0, r1, r2, r3] = &mut self.rows;
                let blocks = fast
                    .chunks_exact(4)
                    .zip(r0.get_mut(x0..).unwrap_or_default().chunks_exact_mut(8))
                    .zip(r1.get_mut(x0..).unwrap_or_default().chunks_exact_mut(8))
                    .zip(r2.get_mut(x0..).unwrap_or_default().chunks_exact_mut(8))
                    .zip(r3.get_mut(x0..).unwrap_or_default().chunks_exact_mut(8));
                for ((((quad, d0), d1), d2), d3) in blocks {
                    d0.copy_from_slice(&indexed_row(pal, quad[0]));
                    d1.copy_from_slice(&indexed_row(pal, quad[1]));
                    d2.copy_from_slice(&indexed_row(pal, quad[2]));
                    d3.copy_from_slice(&indexed_row(pal, quad[3]));
                }
            }

            // Blocks on the right or bottom edge: clipped to the image.
            for (i, quad) in slow.chunks_exact(4).enumerate() {
                let x = (self.bx + whole + i) * 8;
                for (row, &bits) in self.rows.iter_mut().zip(quad) {
                    put_clipped(row, x, &indexed_row(pal, bits));
                }
            }
            self.advance(seg);
        }
    }

    /// Write one block from sixteen explicit pixels.
    #[inline(always)]
    fn block(&mut self, px: &[Row; 4]) {
        let x = self.bx * 8;
        for (row, src) in self.rows.iter_mut().zip(px) {
            match row.get_mut(x..x + 8) {
                Some(dst) => dst.copy_from_slice(src),
                None => put_clipped(row, x, src),
            }
        }
        self.advance(1);
    }
}

/// Decode one RPZA packet into `frame`, which must still hold the previous
/// picture (skipped blocks keep their old contents).
///
/// On error the frame keeps whatever was written before the error, exactly
/// as the C decoder leaves it.
pub fn decode(packet: &[u8], frame: &mut Frame<'_>) -> Result<(), Error> {
    let mut r = Reader::new(packet);
    r.get_be32(); // marker byte + 24-bit chunk size, see `header`

    let mut w = Writer::new(frame);
    if w.remaining / 32 > r.left() {
        return Err(Error::PacketTooSmall);
    }

    let mut color_a: u16 = 0;

    while r.left() > 0 {
        let mut opcode = r.get_u8();
        let mut n_blocks = usize::from(opcode & 0x1f) + 1;

        // With the top bit clear, the opcode byte is the high byte of a colour
        // and the next byte decides between a 4-colour and a 16-colour block.
        if opcode & 0x80 == 0 {
            color_a = u16::from(opcode) << 8 | u16::from(r.get_u8());
            opcode = 0;
            if r.peek_u8() & 0x80 != 0 {
                opcode = 0x20;
                n_blocks = 1;
            }
        }

        n_blocks = n_blocks.min(w.remaining);

        match opcode & 0xe0 {
            // Skip blocks: leave the previous picture in place.
            0x80 => w.advance(n_blocks),

            // Fill blocks with one colour.
            0xa0 => {
                color_a = r.get_be16();
                w.fill(n_blocks, color_a);
            }

            // Fill blocks with four colours, two bits per pixel.
            0xc0 | 0x20 => {
                if opcode & 0xe0 == 0xc0 {
                    color_a = r.get_be16();
                }
                let color_b = r.get_be16();
                let pal = four_colors(color_a, color_b);
                let indices = r
                    .take_slice(n_blocks * 4)
                    .ok_or(Error::FourColorTruncated)?;
                w.four_color(indices, &pal);
            }

            // One block with sixteen explicit colours; the first is `color_a`.
            0x00 => {
                let raw = r.take::<30>().ok_or(Error::SixteenColorTruncated)?;
                if w.remaining == 0 {
                    return Err(Error::NoBlocksLeft);
                }
                let mut px = [color_a; 16];
                for (p, be) in px[1..].iter_mut().zip(raw.chunks_exact(2)) {
                    *p = u16::from_be_bytes([be[0], be[1]]);
                }
                let mut rows = [[0u8; 8]; 4];
                for (row, quad) in rows.iter_mut().zip(px.chunks_exact(4)) {
                    *row = row_bytes([quad[0], quad[1], quad[2], quad[3]]);
                }
                w.block(&rows);
            }

            _ => return Err(Error::UnknownOpcode(opcode)),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], stride: usize, x: usize, y: usize) -> u16 {
        let o = y * stride + x * 2;
        u16::from_ne_bytes([buf[o], buf[o + 1]])
    }

    #[test]
    fn fill_then_skip_keeps_old_pixels() {
        let (w, h, stride) = (8, 4, 16);
        let mut buf = [0u8; 64];
        // header, fill 1 block with 0x1234, skip 1 block
        let pkt = [0xe1, 0, 0, 8, 0xa0, 0x12, 0x34, 0x80];
        let mut f = Frame::new(&mut buf, stride, w, h).unwrap();
        assert_eq!(decode(&pkt, &mut f), Ok(()));
        assert_eq!(px(&buf, stride, 0, 0), 0x1234);
        assert_eq!(px(&buf, stride, 3, 3), 0x1234);
        assert_eq!(px(&buf, stride, 4, 0), 0);
    }

    #[test]
    fn four_colour_block_uses_two_bits_per_pixel() {
        let (w, h, stride) = (4, 4, 8);
        let mut buf = [0u8; 32];
        // colour A = 0x7fff, colour B = 0x0000, then one index byte per row
        let pkt = [0xe1, 0, 0, 13, 0xc0, 0x7f, 0xff, 0x00, 0x00, 0b11_10_01_00, 0xff, 0x00, 0x1b];
        let mut f = Frame::new(&mut buf, stride, w, h).unwrap();
        assert_eq!(decode(&pkt, &mut f), Ok(()));
        let pal = four_colors(0x7fff, 0x0000);
        assert_eq!(pal, [0x0000, 0x294a, 0x5294, 0x7fff]);
        let row0: [u16; 4] = core::array::from_fn(|x| px(&buf, stride, x, 0));
        assert_eq!(row0, [pal[3], pal[2], pal[1], pal[0]]);
        let row3: [u16; 4] = core::array::from_fn(|x| px(&buf, stride, x, 3));
        assert_eq!(row3, [pal[0], pal[1], pal[2], pal[3]]);
    }

    #[test]
    fn edge_blocks_stay_inside_the_image() {
        // 5x5 image in a buffer with a marker pattern in the row padding.
        let (w, h, stride) = (5, 5, 12);
        let mut buf = [0xEEu8; 12 * 4 + 10];
        // fill 4 blocks (2x2 block grid) with 0x7fff
        let pkt = [0xe1, 0, 0, 7, 0xa3, 0x7f, 0xff];
        let mut f = Frame::new(&mut buf, stride, w, h).unwrap();
        assert_eq!(decode(&pkt, &mut f), Ok(()));
        for y in 0..h {
            for x in 0..w {
                assert_eq!(px(&buf, stride, x, y), 0x7fff);
            }
            if y + 1 < h {
                assert!(buf[y * stride + 10..(y + 1) * stride].iter().all(|&b| b == 0xEE));
            }
        }
    }

    #[test]
    fn rejects_bad_input_without_panicking() {
        let mut buf = [0u8; 64];
        let mut f = Frame::new(&mut buf, 16, 8, 4).unwrap();
        assert_eq!(decode(&[0xe1, 0, 0, 5, 0xe0], &mut f), Err(Error::UnknownOpcode(0xe0)));
        assert_eq!(decode(&[0xe1, 0, 0, 7, 0xc1, 0, 0, 0, 0, 1], &mut f), Err(Error::FourColorTruncated));
        assert_eq!(decode(&[0xe1, 0, 0, 6, 0x12, 0x34], &mut f), Err(Error::SixteenColorTruncated));
        assert_eq!(decode(&[], &mut f), Ok(()));
    }

    #[test]
    fn frame_rejects_short_buffers() {
        // 8x4 at stride 16 needs 3 full rows plus 16 visible bytes = 64.
        let mut buf = [0u8; 63];
        assert!(Frame::new(&mut buf, 16, 8, 4).is_none());
        let mut buf = [0u8; 64];
        assert!(Frame::new(&mut buf, 16, 8, 4).is_some());
        assert!(Frame::new(&mut buf, 15, 8, 4).is_none());
        assert!(Frame::new(&mut buf, 16, 0, 4).is_none());
    }
}
