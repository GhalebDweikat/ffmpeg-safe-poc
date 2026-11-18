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

/// For every index byte of a four-colour block (two bits per pixel,
/// leftmost pixel in the top bits): the pixels whose low index bit is set
/// and the pixels whose high index bit is set, as 16-bit lanes of a `u64`.
static INDEX_LANES: [[u64; 2]; 256] = index_lanes();

const fn index_lanes() -> [[u64; 2]; 256] {
    let mut table = [[0u64; 2]; 256];
    let mut bits = 0;
    while bits < 256 {
        let mut px = 0;
        while px < 4 {
            let index = (bits >> (6 - 2 * px)) & 3;
            if index & 1 != 0 {
                table[bits][0] |= 0xffff << PIXEL_SHIFT[px];
            }
            if index & 2 != 0 {
                table[bits][1] |= 0xffff << PIXEL_SHIFT[px];
            }
            px += 1;
        }
        bits += 1;
    }
    table
}

/// The palette of a four-colour block, arranged so that a whole row of four
/// pixels can be selected with a few bitwise operations.
#[derive(Clone, Copy)]
struct Palette {
    /// Colour 0 in all four pixel lanes.
    base: u64,
    /// XOR differences that turn colour 0 into colours 1, 2 and 3.
    low: u64,
    high: u64,
    both: u64,
}

impl Palette {
    /// Colours of a two-colour block, in index order: B, two blends, A.
    #[inline(always)]
    fn new(a: u16, b: u16) -> Self {
        // Spread the three 5-bit channels into 10-bit lanes so that both
        // blends can be computed for all channels at once. The largest
        // lane value is 32 * 31, which fits in 10 bits.
        const fn spread(c: u16) -> u32 {
            let c = c as u32;
            (c & 0x7c00) << 10 | (c & 0x03e0) << 5 | (c & 0x001f)
        }
        const fn gather(lanes: u32) -> u16 {
            let l = (lanes >> 5) & 0x01f0_7c1f;
            (l >> 10 & 0x7c00 | l >> 5 & 0x03e0 | l & 0x001f) as u16
        }
        let (sa, sb) = (spread(a), spread(b));
        let c1 = gather(11 * sa + 21 * sb);
        let c2 = gather(21 * sa + 11 * sb);

        const LANES: u64 = 0x0001_0001_0001_0001;
        Self {
            base: u64::from(b) * LANES,
            low: u64::from(b ^ c1) * LANES,
            high: u64::from(b ^ c2) * LANES,
            both: u64::from(b ^ c1 ^ c2 ^ a) * LANES,
        }
    }

    /// One row of a block from its index byte.
    #[inline(always)]
    fn row(&self, bits: u8) -> Row {
        let [low, high] = INDEX_LANES[usize::from(bits)];
        (self.base ^ (low & self.low) ^ (high & self.high) ^ (low & high & self.both)).to_ne_bytes()
    }

    /// All four rows of a block.
    #[inline(always)]
    fn block(&self, quad: &[u8; 4]) -> [Row; 4] {
        quad.map(|bits| self.row(bits))
    }
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
        })
    }
}

/// One row of blocks ("band"): its four pixel rows, each cut down to the
/// visible pixels and viewed as 8-byte block cells.
///
/// A block at column `bx` occupies `cells[row][bx]`. If the image width is
/// not a multiple of 4, the last column is a partial block whose visible
/// bytes are `edge[row]`. Pixel rows below the bottom of the image have no
/// cells and no edge, so writes to them fall through to nothing.
#[derive(Default)]
struct Band<'a> {
    cells: [&'a mut [Row]; 4],
    edge: [&'a mut [u8]; 4],
}

impl<'a> Band<'a> {
    fn new(band: &'a mut [u8], stride: usize, row_len: usize) -> Self {
        let mut out = Band::default();
        let rows = out.cells.iter_mut().zip(out.edge.iter_mut());
        for ((cells, edge), row) in rows.zip(band.chunks_mut(stride)) {
            let visible = row.len().min(row_len);
            (*cells, *edge) = row[..visible].as_chunks_mut::<8>();
        }
        out
    }

    /// Write one block at column `bx`, clipped to the image.
    #[inline(always)]
    fn put(&mut self, bx: usize, px: &[Row; 4]) {
        let rows = self.cells.iter_mut().zip(self.edge.iter_mut());
        for ((cells, edge), src) in rows.zip(px) {
            match cells.get_mut(bx) {
                Some(dst) => *dst = *src,
                None => {
                    for (d, s) in edge.iter_mut().zip(src) {
                        *d = *s;
                    }
                }
            }
        }
    }
}

/// Writes blocks left to right, top to bottom. It walks the image one band
/// at a time and only ever holds the current band, so a write can never
/// land outside the image.
struct Writer<'a> {
    bands: core::slice::ChunksMut<'a, u8>,
    band: Band<'a>,
    stride: usize,
    row_len: usize,
    /// Column of the next block in the current band.
    bx: usize,
    blocks_w: usize,
    /// Blocks of the frame not yet written or skipped.
    remaining: usize,
}

impl<'a> Writer<'a> {
    fn new(frame: &'a mut Frame<'_>) -> Self {
        let mut w = Writer {
            bands: frame.data.chunks_mut(frame.stride.saturating_mul(4)),
            band: Band::default(),
            stride: frame.stride,
            row_len: frame.row_len,
            bx: 0,
            blocks_w: frame.blocks_w,
            remaining: frame.blocks_w * frame.blocks_h,
        };
        w.next_band();
        w
    }

    #[inline(never)]
    fn next_band(&mut self) {
        self.band = match self.bands.next() {
            Some(band) => Band::new(band, self.stride, self.row_len),
            None => Band::default(),
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

    /// Write one block.
    #[inline(always)]
    fn block(&mut self, px: &[Row; 4]) {
        self.band.put(self.bx, px);
        self.advance(1);
    }

    /// Fill the next `n` blocks with rows of `row`.
    #[inline(always)]
    fn fill(&mut self, mut n: usize, row: Row) {
        while n > 0 {
            let seg = self.in_band(n);
            let (bx, end) = (self.bx, self.bx + seg);
            let rows = self.band.cells.iter_mut().zip(self.band.edge.iter_mut());
            for (cells, edge) in rows {
                let whole = end.min(cells.len());
                if let Some(dst) = cells.get_mut(bx..whole) {
                    dst.fill(row);
                }
                if end > whole {
                    for (d, s) in edge.iter_mut().zip(&row) {
                        *d = *s;
                    }
                }
            }
            self.advance(seg);
            n -= seg;
        }
    }

    /// Write a run of four-colour blocks, four index bytes per block.
    #[inline(always)]
    fn four_color(&mut self, mut quads: &[[u8; 4]], pal: &Palette) {
        while !quads.is_empty() {
            let seg = self.in_band(quads.len());
            let (now, later) = quads.split_at(seg);
            quads = later;
            let bx = self.bx;

            // Blocks wholly inside the image: no bounds checks in the loop.
            let [c0, c1, c2, c3] = &mut self.band.cells;
            let whole = seg.min(c3.len().saturating_sub(bx));
            let (fast, slow) = now.split_at(whole);
            let blocks = fast
                .iter()
                .zip(c0.get_mut(bx..).unwrap_or_default())
                .zip(c1.get_mut(bx..).unwrap_or_default())
                .zip(c2.get_mut(bx..).unwrap_or_default())
                .zip(c3.get_mut(bx..).unwrap_or_default());
            for ((((quad, d0), d1), d2), d3) in blocks {
                *d0 = pal.row(quad[0]);
                *d1 = pal.row(quad[1]);
                *d2 = pal.row(quad[2]);
                *d3 = pal.row(quad[3]);
            }

            // Blocks on the right or bottom edge: clipped to the image.
            for (i, quad) in slow.iter().enumerate() {
                self.band.put(bx + whole + i, &pal.block(quad));
            }
            self.advance(seg);
        }
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
                let row = row_bytes([color_a; 4]);
                if n_blocks == 1 {
                    w.block(&[row; 4]);
                } else {
                    w.fill(n_blocks, row);
                }
            }

            // Fill blocks with four colours, two bits per pixel.
            0xc0 | 0x20 => {
                if opcode & 0xe0 == 0xc0 {
                    color_a = r.get_be16();
                }
                let color_b = r.get_be16();
                let pal = Palette::new(color_a, color_b);
                let indices = r
                    .take_slice(n_blocks * 4)
                    .ok_or(Error::FourColorTruncated)?;
                let (quads, _) = indices.as_chunks::<4>();
                if let [quad] = quads {
                    w.block(&pal.block(quad));
                } else {
                    w.four_color(quads, &pal);
                }
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
        let pal = [0x0000u16, 0x294a, 0x5294, 0x7fff];
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
