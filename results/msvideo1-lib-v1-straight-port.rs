//! Safe Rust port of FFmpeg's Microsoft Video 1 decoder (`msvideo1.c`).
//!
//! The picture is coded as 4x4 blocks, bottom block row first, and inside a
//! block the bottom pixel row first. A block is skipped (it keeps the
//! previous frame's pixels), filled with one colour, or painted with two or
//! eight colours chosen by sixteen flag bits. There are two variants: 8-bit
//! palette video and 16-bit RGB555.
//!
//! This is the straight port: it follows the C code line by line.

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
    stride: usize,
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
        Some(Frame { data, stride, width, height, depth })
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
        Depth::Pal8 => decode_8bit(packet, frame),
        Depth::Rgb555 => decode_16bit(packet, frame),
    }
}

fn decode_8bit(buf: &[u8], frame: &mut Frame<'_>) -> Outcome {
    let mut colors = [0u8; 8];
    let stride = frame.stride;
    let pixels = &mut *frame.data;

    let mut stream_ptr = 0usize;
    // The C code keeps this in an int and tests it for "not zero": a skip
    // code with a count of 0 makes it -1, and then every remaining block of
    // the frame is skipped.
    let mut skip_blocks = 0i32;
    let blocks_wide = frame.width / 4;
    let blocks_high = frame.height / 4;
    let mut total_blocks = blocks_wide * blocks_high;

    for block_y in (1..=blocks_high).rev() {
        let mut block_ptr = (block_y * 4 - 1) * stride;
        for _ in 0..blocks_wide {
            // check if this block should be skipped
            if skip_blocks != 0 {
                block_ptr += 4;
                skip_blocks = skip_blocks.wrapping_sub(1);
                total_blocks -= 1;
                continue;
            }

            // get the next two bytes in the encoded data stream
            if stream_ptr + 2 > buf.len() {
                return Outcome::Truncated;
            }
            let byte_a = buf[stream_ptr];
            let byte_b = buf[stream_ptr + 1];
            stream_ptr += 2;

            // check if the decode is finished
            if byte_a == 0 && byte_b == 0 && total_blocks == 0 {
                return Outcome::Truncated;
            } else if byte_b & 0xFC == 0x84 {
                // skip code, but don't count the current block
                skip_blocks = ((i32::from(byte_b) - 0x84) << 8) + i32::from(byte_a) - 1;
            } else if byte_b < 0x80 {
                // 2-color encoding
                let mut flags = u16::from(byte_b) << 8 | u16::from(byte_a);

                if stream_ptr + 2 > buf.len() {
                    return Outcome::Truncated;
                }
                colors[0] = buf[stream_ptr];
                colors[1] = buf[stream_ptr + 1];
                stream_ptr += 2;

                for pixel_y in 0..4 {
                    for pixel_x in 0..4 {
                        pixels[block_ptr - pixel_y * stride + pixel_x] =
                            colors[usize::from(flags & 1 ^ 1)];
                        flags >>= 1;
                    }
                }
            } else if byte_b >= 0x90 {
                // 8-color encoding
                let mut flags = u16::from(byte_b) << 8 | u16::from(byte_a);

                if stream_ptr + 8 > buf.len() {
                    return Outcome::Truncated;
                }
                colors.copy_from_slice(&buf[stream_ptr..stream_ptr + 8]);
                stream_ptr += 8;

                for pixel_y in 0..4 {
                    for pixel_x in 0..4 {
                        pixels[block_ptr - pixel_y * stride + pixel_x] = colors
                            [((pixel_y & 2) << 1) + (pixel_x & 2) + usize::from(flags & 1 ^ 1)];
                        flags >>= 1;
                    }
                }
            } else {
                // 1-color encoding
                colors[0] = byte_a;

                for pixel_y in 0..4 {
                    for pixel_x in 0..4 {
                        pixels[block_ptr - pixel_y * stride + pixel_x] = colors[0];
                    }
                }
            }

            block_ptr += 4;
            total_blocks -= 1;
        }
    }
    Outcome::Finished
}

fn rl16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

/// Store pixel number `at` of a 16-bit picture.
fn put16(pixels: &mut [u8], at: usize, color: u16) {
    pixels[2 * at..2 * at + 2].copy_from_slice(&color.to_ne_bytes());
}

fn decode_16bit(buf: &[u8], frame: &mut Frame<'_>) -> Outcome {
    let mut colors = [0u16; 8];
    // In pixels, as in the C code, which halves the stride in bytes.
    let stride = frame.stride / 2;
    let pixels = &mut *frame.data;

    let mut stream_ptr = 0usize;
    let mut skip_blocks = 0i32;
    let blocks_wide = frame.width / 4;
    let blocks_high = frame.height / 4;
    let mut total_blocks = blocks_wide * blocks_high;

    for block_y in (1..=blocks_high).rev() {
        let mut block_ptr = (block_y * 4 - 1) * stride;
        for _ in 0..blocks_wide {
            // check if this block should be skipped
            if skip_blocks != 0 {
                block_ptr += 4;
                skip_blocks = skip_blocks.wrapping_sub(1);
                total_blocks -= 1;
                continue;
            }

            // get the next two bytes in the encoded data stream
            if stream_ptr + 2 > buf.len() {
                return Outcome::Truncated;
            }
            let byte_a = buf[stream_ptr];
            let byte_b = buf[stream_ptr + 1];
            stream_ptr += 2;

            // check if the decode is finished
            if byte_a == 0 && byte_b == 0 && total_blocks == 0 {
                return Outcome::Truncated;
            } else if byte_b & 0xFC == 0x84 {
                // skip code, but don't count the current block
                skip_blocks = ((i32::from(byte_b) - 0x84) << 8) + i32::from(byte_a) - 1;
            } else if byte_b < 0x80 {
                // 2- or 8-color encoding modes
                let mut flags = u16::from(byte_b) << 8 | u16::from(byte_a);

                if stream_ptr + 4 > buf.len() {
                    return Outcome::Truncated;
                }
                colors[0] = rl16(buf, stream_ptr);
                colors[1] = rl16(buf, stream_ptr + 2);
                stream_ptr += 4;

                if colors[0] & 0x8000 != 0 {
                    // 8-color encoding
                    if stream_ptr + 12 > buf.len() {
                        return Outcome::Truncated;
                    }
                    for i in 2..8 {
                        colors[i] = rl16(buf, stream_ptr);
                        stream_ptr += 2;
                    }

                    for pixel_y in 0..4 {
                        for pixel_x in 0..4 {
                            put16(
                                pixels,
                                block_ptr - pixel_y * stride + pixel_x,
                                colors[((pixel_y & 2) << 1)
                                    + (pixel_x & 2)
                                    + usize::from(flags & 1 ^ 1)],
                            );
                            flags >>= 1;
                        }
                    }
                } else {
                    // 2-color encoding
                    for pixel_y in 0..4 {
                        for pixel_x in 0..4 {
                            put16(
                                pixels,
                                block_ptr - pixel_y * stride + pixel_x,
                                colors[usize::from(flags & 1 ^ 1)],
                            );
                            flags >>= 1;
                        }
                    }
                }
            } else {
                // otherwise, it's a 1-color block
                colors[0] = u16::from(byte_b) << 8 | u16::from(byte_a);

                for pixel_y in 0..4 {
                    for pixel_x in 0..4 {
                        put16(pixels, block_ptr - pixel_y * stride + pixel_x, colors[0]);
                    }
                }
            }

            block_ptr += 4;
            total_blocks -= 1;
        }
    }
    Outcome::Finished
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
