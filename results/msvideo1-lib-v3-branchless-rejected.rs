//! Safe Rust port of FFmpeg's Microsoft Video 1 decoder (`msvideo1.c`).
//!
//! The picture is coded as 4x4 blocks, bottom block row first, and inside a
//! block the bottom pixel row first. A block is skipped (it keeps the
//! previous frame's pixels), filled with one colour, or painted with two or
//! eight colours chosen by sixteen flag bits. There are two variants: 8-bit
//! palette video and 16-bit RGB555.
//!
//! The port is bit-exact with the C decoder. Two things in it come from how
//! the C code is written rather than from the format:
//!
//! * A skip code with a count of 0 skips every remaining block of the
//!   frame: the C code computes a count of -1 and counts down until it
//!   reaches 0, which it never does.
//! * When a packet runs out, the C decoder returns early. The blocks
//!   decoded so far stay in the picture, and for palette video the frame is
//!   output without the current palette (see [`Outcome::Truncated`]).
//!
//! The C code also tests for an end marker (two zero bytes once every block
//! is done). That test cannot succeed, because two code bytes are only ever
//! read for a block that is still to be done, so it is not ported: two zero
//! bytes are a two-colour block.

#![forbid(unsafe_code)]
#![no_std]

/// Which of the two variants a stream is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// One byte per pixel, an index into a palette.
    Pal8,
    /// Two bytes per pixel, RGB555 in native byte order.
    Rgb555,
}

/// How decoding a packet ended. A picture is output either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every block of the picture was decoded or skipped.
    Finished,
    /// The packet ran out first. The blocks decoded so far stay in the
    /// picture. The C decoder returns early here, which for palette video
    /// means the frame does not get the current palette.
    Truncated,
}

/// The visible picture: `height` rows, `stride` bytes apart, the last one
/// cut off after the last visible pixel. Row 0 is the top row.
pub struct Frame<'a> {
    data: &'a mut [u8],
    /// Bytes from one row to the next.
    pitch: usize,
    width: usize,
    height: usize,
    depth: Depth,
}

impl<'a> Frame<'a> {
    pub fn new(
        data: &'a mut [u8],
        stride: usize,
        width: usize,
        height: usize,
        depth: Depth,
    ) -> Option<Self> {
        let bpp = match depth {
            Depth::Pal8 => 1,
            Depth::Rgb555 => 2,
        };
        let row_len = width.checked_mul(bpp)?;
        if row_len == 0 || height == 0 || stride < row_len {
            return None;
        }
        let need = stride.checked_mul(height - 1)?.checked_add(row_len)?;
        let data = data.get_mut(..need)?;
        // The C code counts 16-bit rows in pixels: it halves the stride.
        let pitch = stride / bpp * bpp;
        Some(Frame { data, pitch, width, height, depth })
    }
}

/// The C decoder's check before it asks for a frame: a packet shorter than
/// this cannot code the picture even with the longest skip codes.
pub fn packet_is_plausible(len: usize, width: usize, height: usize) -> bool {
    len >= (width / 4).saturating_mul(height / 4) / 512
}

/// Decode one packet onto `frame`, which holds the previous picture.
pub fn decode(packet: &[u8], frame: &mut Frame<'_>) -> Outcome {
    match frame.depth {
        Depth::Pal8 => decode_blocks::<Pal8>(packet, frame),
        Depth::Rgb555 => decode_blocks::<Rgb555>(packet, frame),
    }
}

/// How blocks are coded and laid out in one of the two variants.
trait Format {
    /// One row of a block: four pixels.
    type Row: Copy;
    /// Bytes in a `Row`.
    const ROW: usize;
    /// A picture row as whole blocks.
    fn blocks(row: &mut [u8]) -> &mut [Self::Row];
    /// The four rows of a painted block, bottom row first, from its two
    /// code bytes and the colours that follow them in `data`. `None` if
    /// the packet has run out.
    fn paint(byte_a: u8, byte_b: u8, data: &mut &[u8]) -> Option<[Self::Row; 4]>;
}

fn decode_blocks<F: Format>(mut data: &[u8], frame: &mut Frame<'_>) -> Outcome {
    let blocks_wide = frame.width / 4;
    let blocks_high = frame.height / 4;
    let pitch = frame.pitch;
    let row_len = blocks_wide * F::ROW;
    // Blocks still to be skipped. Signed, as in the C code: a skip code
    // with a count of 0 makes it -1, which never counts down to 0.
    let mut skip = 0i32;

    // The first block row in the stream is the bottom one.
    for block_y in (0..blocks_high).rev() {
        if skip < 0 {
            return Outcome::Finished;
        }
        if skip as usize >= blocks_wide {
            skip -= blocks_wide as i32;
            continue;
        }

        let band = &mut frame.data[block_y * 4 * pitch..];
        let (r0, rest) = band.split_at_mut(pitch);
        let (r1, rest) = rest.split_at_mut(pitch);
        let (r2, r3) = rest.split_at_mut(pitch);
        let blocks = F::blocks(&mut r0[..row_len])
            .iter_mut()
            .zip(F::blocks(&mut r1[..row_len]))
            .zip(F::blocks(&mut r2[..row_len]))
            .zip(F::blocks(&mut r3[..row_len]));

        for (((b0, b1), b2), b3) in blocks {
            if skip != 0 {
                skip = skip.wrapping_sub(1);
                continue;
            }
            let Some((&[byte_a, byte_b], rest)) = data.split_first_chunk::<2>() else {
                return Outcome::Truncated;
            };
            data = rest;

            if byte_b & 0xFC == 0x84 {
                // A skip code. Its count includes this block.
                skip = (i32::from(byte_b - 0x84) << 8) + i32::from(byte_a) - 1;
                continue;
            }
            let Some(rows) = F::paint(byte_a, byte_b, &mut data) else {
                return Outcome::Truncated;
            };
            *b3 = rows[0];
            *b2 = rows[1];
            *b1 = rows[2];
            *b0 = rows[3];
        }
    }
    Outcome::Finished
}

/// For each value of four flag bits, a word with all bits set in the bytes
/// (`MASK8`) or 16-bit lanes (`MASK16`) whose flag is set; flag 0 is the
/// lowest byte or lane.
const fn masks<const LANE: u32>() -> [u64; 16] {
    let mut table = [0u64; 16];
    let mut n = 0;
    while n < 16 {
        let mut i = 0;
        while i < 4 {
            if n >> i & 1 != 0 {
                table[n] |= ((1u64 << LANE) - 1) << (LANE * i);
            }
            i += 1;
        }
        n += 1;
    }
    table
}

static MASK8: [u64; 16] = masks::<8>();
static MASK16: [u64; 16] = masks::<16>();

/// The next `N` bytes of the stream without consuming them, padded with
/// zeros if fewer are left. Whoever uses bytes that are not there must
/// find out when consuming them (`data.get(used..)`).
#[inline(always)]
fn peek<const N: usize>(data: &[u8]) -> [u8; N] {
    match data.first_chunk::<N>() {
        Some(next) => *next,
        None => {
            let mut next = [0u8; N];
            for (d, s) in next.iter_mut().zip(data) {
                *d = *s;
            }
            next
        }
    }
}

/// `if cond { a } else { b }` as a conditional move. Which kind of block
/// comes next is close to random, so branching on it is slow.
#[inline(always)]
fn pick<T>(cond: bool, a: T, b: T) -> T {
    core::hint::select_unpredictable(cond, a, b)
}

// The three kinds of painted block are decoded by the same straight-line
// code. Each kind is reduced to a "set" and a "clear" colour for every
// pixel: a pixel takes its set colour where its flag is 1, else its clear
// colour. A one-colour block has both the same; an eight-colour block has
// a pair for each 2x2 quadrant, in the order bottom left, bottom right,
// top left, top right.

struct Pal8;

impl Format for Pal8 {
    type Row = [u8; 4];
    const ROW: usize = 4;

    fn blocks(row: &mut [u8]) -> &mut [Self::Row] {
        row.as_chunks_mut::<4>().0
    }

    #[inline(always)]
    fn paint(byte_a: u8, byte_b: u8, data: &mut &[u8]) -> Option<[Self::Row; 4]> {
        const ONES: u64 = 0x0101_0101_0101_0101;
        const EVEN: u64 = 0x00FF_00FF_00FF_00FF;
        let flags = u16::from_le_bytes([byte_a, byte_b]);
        let two = byte_b < 0x80;
        let eight = byte_b >= 0x90;

        // Eight pixels per word: the two bottom rows' colours in the low
        // half, the two top rows' in the high half.
        let next = u64::from_le_bytes(peek::<8>(data));
        let fill = u64::from(byte_a) * ONES;
        let set = pick(two, (next & 0xFF) * ONES, pick(eight, (next & EVEN) * 0x0101, fill));
        let clear =
            pick(two, (next >> 8 & 0xFF) * ONES, pick(eight, (next >> 8 & EVEN) * 0x0101, fill));
        let used = pick(two, 2, pick(eight, 8, 0));
        *data = data.get(used..)?;

        let mask = |y: usize| MASK8[usize::from(flags >> (4 * y)) & 15];
        // Rows 0 and 2, and rows 1 and 3, bottom row first.
        let even = clear ^ (set ^ clear) & (mask(0) | mask(2) << 32);
        let odd = clear ^ (set ^ clear) & (mask(1) | mask(3) << 32);
        Some([
            (even as u32).to_le_bytes(),
            (odd as u32).to_le_bytes(),
            ((even >> 32) as u32).to_le_bytes(),
            ((odd >> 32) as u32).to_le_bytes(),
        ])
    }
}

struct Rgb555;

impl Rgb555 {
    /// Four pixels, pixel 0 in the lowest 16 bits, as they are stored: each
    /// pixel in native byte order.
    #[inline(always)]
    fn row(word: u64) -> [u8; 8] {
        if cfg!(target_endian = "big") {
            let low = 0x00FF_00FF_00FF_00FF;
            ((word & low) << 8 | (word >> 8) & low).to_le_bytes()
        } else {
            word.to_le_bytes()
        }
    }
}

impl Format for Rgb555 {
    type Row = [u8; 8];
    const ROW: usize = 8;

    fn blocks(row: &mut [u8]) -> &mut [Self::Row] {
        row.as_chunks_mut::<8>().0
    }

    #[inline(always)]
    fn paint(byte_a: u8, byte_b: u8, data: &mut &[u8]) -> Option<[Self::Row; 4]> {
        const ONES: u64 = 0x0001_0001_0001_0001;
        const EVEN: u64 = 0x0000_FFFF_0000_FFFF;
        let flags = u16::from_le_bytes([byte_a, byte_b]);

        // Up to eight colours, four to a word.
        let next = u128::from_le_bytes(peek::<16>(data));
        let (bottom, top) = (next as u64, (next >> 64) as u64);
        // Colours follow the flags; the top bit of the first one says
        // whether there are two or eight. Otherwise the two code bytes are
        // the one colour.
        let coded = byte_b < 0x80;
        let eight = coded && bottom & 0x8000 != 0;
        let fill = u64::from(flags) * ONES;
        let set2 = pick(coded, (bottom & 0xFFFF) * ONES, fill);
        let clear2 = pick(coded, (bottom >> 16 & 0xFFFF) * ONES, fill);

        // Four pixels per word, one word for the two bottom rows and one
        // for the two top rows.
        let set = [
            pick(eight, (bottom & EVEN) * 0x0001_0001, set2),
            pick(eight, (top & EVEN) * 0x0001_0001, set2),
        ];
        let clear = [
            pick(eight, (bottom >> 16 & EVEN) * 0x0001_0001, clear2),
            pick(eight, (top >> 16 & EVEN) * 0x0001_0001, clear2),
        ];
        let used = pick(eight, 16, pick(coded, 4, 0));
        *data = data.get(used..)?;

        let row = |y: usize| {
            let mask = MASK16[usize::from(flags >> (4 * y)) & 15];
            Self::row(clear[y / 2] ^ (set[y / 2] ^ clear[y / 2]) & mask)
        };
        Some([row(0), row(1), row(2), row(3)])
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// Decode `packet` onto a picture filled with `fill`, with `pad` unused
    /// bytes after every row but the last. Returns the visible rows.
    fn run(
        packet: &[u8],
        width: usize,
        height: usize,
        depth: Depth,
        pad: usize,
        fill: u8,
    ) -> (Outcome, Vec<Vec<u8>>) {
        let bpp = if depth == Depth::Pal8 { 1 } else { 2 };
        let stride = width * bpp + pad;
        let mut buf = vec![fill; stride * (height - 1) + width * bpp];
        let outcome = {
            let mut frame = Frame::new(&mut buf, stride, width, height, depth).unwrap();
            decode(packet, &mut frame)
        };
        // The padding must be untouched.
        for y in 0..height - 1 {
            assert!(buf[y * stride + width * bpp..(y + 1) * stride].iter().all(|&b| b == fill));
        }
        let rows = (0..height).map(|y| buf[y * stride..y * stride + width * bpp].to_vec()).collect();
        (outcome, rows)
    }

    fn px16(row: &[u8]) -> Vec<u16> {
        row.chunks(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn size_check() {
        // 160x120 is 1200 blocks: at least 2 bytes.
        assert!(!packet_is_plausible(1, 160, 120));
        assert!(packet_is_plausible(2, 160, 120));
        assert!(packet_is_plausible(0, 80, 80));
    }

    #[test]
    fn pal_two_colours_bottom_row_first() {
        // Flags 0x0001: only the first pixel, bottom left, gets colour 0.
        let (outcome, rows) = run(&[0x01, 0x00, 7, 9], 4, 4, Depth::Pal8, 3, 0xEE);
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(rows, [[9, 9, 9, 9], [9, 9, 9, 9], [9, 9, 9, 9], [7, 9, 9, 9]]);
    }

    #[test]
    fn pal_eight_colours_by_quadrant() {
        // All flags set (0xFFFF needs a high byte >= 0x90): every pixel takes
        // the even colour of its 2x2 quadrant.
        let packet = [0xFF, 0xFF, 10, 11, 20, 21, 30, 31, 40, 41];
        let (outcome, rows) = run(&packet, 4, 4, Depth::Pal8, 0, 0);
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(rows, [[30, 30, 40, 40], [30, 30, 40, 40], [10, 10, 20, 20], [10, 10, 20, 20]]);
        // No flags set in the low byte, high byte 0x90: bit 12 and 15 set.
        let packet = [0x00, 0x90, 10, 11, 20, 21, 30, 31, 40, 41];
        let (_, rows) = run(&packet, 4, 4, Depth::Pal8, 0, 0);
        assert_eq!(rows, [[30, 31, 41, 40], [31, 31, 41, 41], [11, 11, 21, 21], [11, 11, 21, 21]]);
    }

    #[test]
    fn pal_one_colour_and_skips() {
        // 12x4: one-colour block, a skip of one block, one-colour block.
        let packet = [5, 0x80, 0x01, 0x84, 6, 0x8F];
        let (outcome, rows) = run(&packet, 12, 4, Depth::Pal8, 1, 0xAA);
        assert_eq!(outcome, Outcome::Finished);
        for row in rows {
            assert_eq!(row, [5, 5, 5, 5, 0xAA, 0xAA, 0xAA, 0xAA, 6, 6, 6, 6]);
        }
    }

    #[test]
    fn skip_count_of_zero_skips_the_rest() {
        // 8x8: one block, then a skip code with count 0. The C decoder
        // computes a count of -1 and never stops skipping; the bytes after
        // it are not read.
        let packet = [5, 0x80, 0x00, 0x84, 6, 0x80, 6, 0x80];
        let (outcome, rows) = run(&packet, 8, 8, Depth::Pal8, 0, 1);
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(rows[7], [5, 5, 5, 5, 1, 1, 1, 1]);
        assert_eq!(rows[3], [1; 8]);
    }

    #[test]
    fn skip_runs_cross_block_rows() {
        // 8x8: skip 3 blocks, then a one-colour block: top right.
        let packet = [0x03, 0x84, 9, 0x80];
        let (outcome, rows) = run(&packet, 8, 8, Depth::Pal8, 0, 1);
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(rows[0], [1, 1, 1, 1, 9, 9, 9, 9]);
        assert_eq!(rows[4], [1; 8]);
    }

    #[test]
    fn truncated_packets() {
        // Colours missing after the flags; nothing is written.
        let (outcome, rows) = run(&[0x01, 0x00, 7], 4, 4, Depth::Pal8, 0, 3);
        assert_eq!(outcome, Outcome::Truncated);
        assert_eq!(rows[3], [3; 4]);
        // An odd byte at the end, and an empty packet.
        assert_eq!(run(&[0x01], 4, 4, Depth::Pal8, 0, 3).0, Outcome::Truncated);
        assert_eq!(run(&[], 4, 4, Depth::Rgb555, 0, 3).0, Outcome::Truncated);
        // 16-bit eight-colour block with only eleven of its twelve bytes.
        let packet = [0xFF, 0x7F, 0x00, 0x80, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7];
        let (outcome, rows) = run(&packet, 4, 4, Depth::Rgb555, 0, 3);
        assert_eq!(outcome, Outcome::Truncated);
        assert_eq!(rows[0], [3; 8]);
    }

    #[test]
    fn edges_outside_whole_blocks_are_kept() {
        // 7x6 has one block: columns 4..7 and rows 4..6 are not coded.
        let (outcome, rows) = run(&[5, 0x80], 7, 6, Depth::Pal8, 2, 0xCC);
        assert_eq!(outcome, Outcome::Finished);
        for row in &rows[..4] {
            assert_eq!(row[..], [5, 5, 5, 5, 0xCC, 0xCC, 0xCC]);
        }
        assert_eq!(rows[4], [0xCC; 7]);
        assert_eq!(rows[5], [0xCC; 7]);
    }

    #[test]
    fn rgb_one_and_two_colours() {
        // One colour: the two code bytes are the pixel, top bit included.
        let (outcome, rows) = run(&[0x34, 0x92], 4, 4, Depth::Rgb555, 2, 0);
        assert_eq!(outcome, Outcome::Finished);
        for row in &rows {
            assert_eq!(px16(row), [0x9234; 4]);
        }
        // Two colours, flags 0x7FFE: the first and the last pixel take
        // colour 1.
        let packet = [0xFE, 0x7F, 0x11, 0x11, 0x22, 0xA2];
        let (_, rows) = run(&packet, 4, 4, Depth::Rgb555, 0, 0);
        assert_eq!(px16(&rows[3]), [0xA222, 0x1111, 0x1111, 0x1111]);
        assert_eq!(px16(&rows[0]), [0x1111, 0x1111, 0x1111, 0xA222]);
    }

    #[test]
    fn rgb_eight_colours_by_quadrant() {
        // The first colour's top bit selects eight colours.
        let mut packet = vec![0x00, 0x00];
        for c in [0x8001u16, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008] {
            packet.extend(c.to_le_bytes());
        }
        let (outcome, rows) = run(&packet, 4, 4, Depth::Rgb555, 4, 0);
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(px16(&rows[0]), [6, 6, 8, 8]);
        assert_eq!(px16(&rows[3]), [2, 2, 4, 4]);
        packet[0] = 0xFF;
        packet[1] = 0x7F;
        let (_, rows) = run(&packet, 4, 4, Depth::Rgb555, 4, 0);
        assert_eq!(px16(&rows[0]), [5, 5, 7, 8]);
        assert_eq!(px16(&rows[3]), [0x8001, 0x8001, 3, 3]);
    }

    #[test]
    fn frame_must_fit() {
        let mut buf = [0u8; 15];
        assert!(Frame::new(&mut buf, 4, 4, 4, Depth::Pal8).is_none());
        let mut buf = [0u8; 16];
        assert!(Frame::new(&mut buf, 4, 4, 4, Depth::Pal8).is_some());
        assert!(Frame::new(&mut buf, 3, 4, 4, Depth::Pal8).is_none());
        assert!(Frame::new(&mut buf, 4, 4, 4, Depth::Rgb555).is_none());
    }
}
