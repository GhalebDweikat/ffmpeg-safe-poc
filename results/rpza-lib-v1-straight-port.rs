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

#[inline(always)]
fn row_bytes(px: [u16; 4]) -> Row {
    let mut out = [0u8; 8];
    for (dst, p) in out.chunks_exact_mut(2).zip(px) {
        dst.copy_from_slice(&p.to_ne_bytes());
    }
    out
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
    /// Block columns and rows that lie entirely inside the image.
    full_w: usize,
    full_h: usize,
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
            full_h: height / 4,
        })
    }

    fn blocks(&self) -> usize {
        self.blocks_w * self.blocks_h
    }

    /// Write one 4x4 block at block coordinates (`bx`, `by`).
    #[inline(always)]
    fn put_block(&mut self, bx: usize, by: usize, rows: &[Row; 4]) {
        if bx < self.full_w && by < self.full_h {
            let x = bx * 8;
            let band = &mut self.data[by * 4 * self.stride..];
            for (dst, src) in band.chunks_mut(self.stride).zip(rows) {
                dst[x..x + 8].copy_from_slice(src);
            }
        } else {
            self.put_block_clipped(bx, by, rows);
        }
    }

    /// Edge blocks: write only the part that falls inside the image.
    #[cold]
    #[inline(never)]
    fn put_block_clipped(&mut self, bx: usize, by: usize, rows: &[Row; 4]) {
        let x = bx * 8;
        let Some(visible) = self.row_len.checked_sub(x) else {
            return;
        };
        let n = visible.min(8);
        let Some(band) = self.data.get_mut(by * 4 * self.stride..) else {
            return;
        };
        // `data` ends at the last visible pixel, so `chunks_mut` yields only
        // rows that exist; the final row is `row_len` bytes long.
        for (dst, src) in band.chunks_mut(self.stride).zip(rows) {
            dst[x..x + n].copy_from_slice(&src[..n]);
        }
    }
}

/// Position of the next block to write, in block units, plus how many
/// blocks of the frame are still unwritten.
struct Cursor {
    bx: usize,
    by: usize,
    remaining: usize,
    blocks_w: usize,
}

impl Cursor {
    #[inline(always)]
    fn advance(&mut self) {
        self.bx += 1;
        if self.bx >= self.blocks_w {
            self.bx = 0;
            self.by += 1;
        }
        self.remaining -= 1;
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

    let mut cur = Cursor {
        bx: 0,
        by: 0,
        remaining: frame.blocks(),
        blocks_w: frame.blocks_w,
    };
    if cur.remaining / 32 > r.left() {
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

        n_blocks = n_blocks.min(cur.remaining);

        match opcode & 0xe0 {
            // Skip blocks: leave the previous picture in place.
            0x80 => {
                for _ in 0..n_blocks {
                    cur.advance();
                }
            }

            // Fill blocks with one colour.
            0xa0 => {
                color_a = r.get_be16();
                let rows = [row_bytes([color_a; 4]); 4];
                for _ in 0..n_blocks {
                    frame.put_block(cur.bx, cur.by, &rows);
                    cur.advance();
                }
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
                for quad in indices.chunks_exact(4) {
                    let mut rows = [[0u8; 8]; 4];
                    for (row, &bits) in rows.iter_mut().zip(quad) {
                        *row = row_bytes([
                            pal[usize::from(bits >> 6)],
                            pal[usize::from((bits >> 4) & 3)],
                            pal[usize::from((bits >> 2) & 3)],
                            pal[usize::from(bits & 3)],
                        ]);
                    }
                    frame.put_block(cur.bx, cur.by, &rows);
                    cur.advance();
                }
            }

            // One block with sixteen explicit colours; the first is `color_a`.
            0x00 => {
                let raw = r.take::<30>().ok_or(Error::SixteenColorTruncated)?;
                if cur.remaining == 0 {
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
                frame.put_block(cur.bx, cur.by, &rows);
                cur.advance();
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
    fn edge_blocks_stay_inside_the_image() {
        // 5x5 image in a buffer with a guard byte pattern around it.
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
                // row padding between width*2 and stride is untouched
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
