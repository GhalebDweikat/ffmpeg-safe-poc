//! Safe Rust port of FFmpeg's QuickTime Animation (RLE) video decoder,
//! `libavcodec/qtrle.c`: 1, 2, 4, 8, 16, 24 and 32 bits per pixel.
//!
//! The port is bit-exact with the C decoder, including the things the C
//! decoder does that depend on FFmpeg's frame layout:
//!
//! * The C code treats the picture as one long buffer of `stride * height`
//!   bytes and checks every write against the end of that buffer only. A
//!   run or a skip may therefore leave its row, cross the padding after it
//!   and continue on the next row. The port keeps the same position in the
//!   same buffer, makes the same decisions from the same stride, and drops
//!   the bytes that would land in the padding.
//! * Reads past the end of the packet give zero, in the groups the C code
//!   reads in (a 4-byte read with 3 bytes left gives four zero bytes).
//! * A packet that ends early, or a write that does not fit, stops decoding;
//!   the picture is output with whatever was decoded up to there.

#![forbid(unsafe_code)]
#![no_std]

/// What the packet header says is to be done with the packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Decode it onto the previous picture.
    Decode,
    /// Nothing changes: output the previous picture again.
    Duplicate,
    /// The packet is much shorter than it says it is; reject it.
    Invalid,
}

/// How decoding a packet ended. The picture is output in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// All lines were decoded, or the packet ran out.
    Done,
    /// A skip or a run left the picture; decoding stopped there.
    OutOfBounds,
    /// The header asked for no change; nothing was written.
    Duplicate,
    /// The decoder has no mode for this bit depth.
    UnknownDepth,
}

/// Bytes in one row of a picture with `bits` bits per coded sample, as
/// FFmpeg lays it out: palette indices one per byte, RGB555 in two bytes,
/// RGB24 in three, ARGB in four.
pub fn row_len(width: usize, bits: u32) -> Option<usize> {
    match bits {
        1 | 2 | 4 | 8 | 33 | 34 | 36 | 40 => Some(width),
        16 => width.checked_mul(2),
        24 => width.checked_mul(3),
        32 => width.checked_mul(4),
        _ => None,
    }
}

/// The visible picture: `height` rows of `row_len` bytes, `stride` bytes
/// apart. Row 0 is the top row.
pub struct Frame<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_len: usize,
    height: usize,
}

impl<'a> Frame<'a> {
    /// `data` must hold at least `stride * (height - 1) + row_len` bytes;
    /// anything after that is not touched.
    pub fn new(
        data: &'a mut [u8],
        stride: usize,
        width: usize,
        height: usize,
        bits: u32,
    ) -> Option<Self> {
        let row_len = row_len(width, bits)?;
        if width == 0 || height == 0 || stride < row_len {
            return None;
        }
        let need = stride.checked_mul(height - 1)?.checked_add(row_len)?;
        // The C decoder's `pixel_limit`, which must fit its `int`.
        let limit = stride.checked_mul(height)?;
        if limit > i32::MAX as usize {
            return None;
        }
        let data = data.get_mut(..need)?;
        Some(Frame { data, stride, row_len, height })
    }

    /// The C decoder's `pixel_limit`: the end of the last row including its
    /// padding.
    fn limit(&self) -> i64 {
        (self.stride * self.height) as i64
    }

    /// Store `bytes` at byte offset `pos` of the C decoder's buffer. Bytes
    /// that fall into the padding after a row are dropped. The caller has
    /// checked `0 <= pos` and `pos + bytes.len() <= limit`.
    fn put(&mut self, pos: i64, bytes: &[u8]) {
        let mut p = pos as usize;
        for &b in bytes {
            if p % self.stride < self.row_len {
                if let Some(d) = self.data.get_mut(p) {
                    *d = b;
                }
            }
            p += 1;
        }
    }
}

/// Mirrors libavcodec's checked bytestream reader: reading past the end
/// yields zero and leaves the reader at the end.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn left(&self) -> usize {
        self.data.len()
    }

    fn byte(&mut self) -> u8 {
        match self.data.split_first() {
            Some((&b, rest)) => {
                self.data = rest;
                b
            }
            None => 0,
        }
    }

    fn peek(&self) -> u8 {
        self.data.first().copied().unwrap_or(0)
    }

    fn skip(&mut self, n: usize) {
        self.data = &self.data[n.min(self.data.len())..];
    }

    /// The next `N` bytes. With fewer than `N` left: zeros, and the reader
    /// moves to the end (`bytestream2_get_ne16` and friends).
    fn array<const N: usize>(&mut self) -> [u8; N] {
        match self.data.split_first_chunk::<N>() {
            Some((a, rest)) => {
                self.data = rest;
                *a
            }
            None => {
                self.data = &[];
                [0; N]
            }
        }
    }

    fn be16(&mut self) -> u16 {
        u16::from_be_bytes(self.array())
    }

    fn be32(&mut self) -> u32 {
        u32::from_be_bytes(self.array())
    }

    /// Up to `n` bytes (`bytestream2_get_buffer`).
    fn take_up_to(&mut self, n: usize) -> &'a [u8] {
        let (head, rest) = self.data.split_at(n.min(self.data.len()));
        self.data = rest;
        head
    }
}

/// The header checks of `qtrle_decode_frame`, in its order. `height` is the
/// picture height; `discard_damaged_percentage` is the libavcodec option of
/// that name.
pub fn check_packet(packet: &[u8], height: usize, discard_damaged_percentage: i32) -> Verdict {
    if packet.len() < 8 {
        return Verdict::Duplicate;
    }
    let mut r = Reader { data: packet };
    let size = (r.be32() & 0x3FFF_FFFF) as i64;
    if size - packet.len() as i64 > size * discard_damaged_percentage as i64 / 100 {
        return Verdict::Invalid;
    }
    match region(packet, height) {
        Some(_) => Verdict::Decode,
        None => Verdict::Duplicate,
    }
}

/// First line and number of lines the packet changes, or `None` when it
/// changes nothing.
fn region(packet: &[u8], height: usize) -> Option<(usize, usize)> {
    if packet.len() < 8 {
        return None;
    }
    let mut r = Reader { data: &packet[4..] };
    let header = r.be16();
    if header & 0x0008 == 0 {
        return Some((0, height));
    }
    if packet.len() < 14 {
        return None;
    }
    let start_line = r.be16() as usize;
    r.skip(2);
    let lines = r.be16() as usize;
    // C: `if (height > s->avctx->height - start_line)`, in signed ints.
    if lines as i64 > height as i64 - start_line as i64 {
        return None;
    }
    Some((start_line, lines))
}

/// The C decoder's `CHECK_PIXEL_PTR(n)`.
macro_rules! check {
    ($pixel_ptr:expr, $n:expr, $limit:expr) => {
        if $pixel_ptr + $n > $limit || $pixel_ptr + $n < 0 {
            return Status::OutOfBounds;
        }
    };
}

/// One byte as eight pixels, most significant bit first.
fn bits8(x: u8) -> [u8; 8] {
    let mut px = [0u8; 8];
    for (i, p) in px.iter_mut().enumerate() {
        *p = (x >> (7 - i)) & 1;
    }
    px
}

fn decode_1bpp(r: &mut Reader<'_>, f: &mut Frame<'_>, row_ptr: i64, lines: i64) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();
    // A skip byte with the top bit set means "next line", also for the first
    // line; so start one row up and count one more line.
    let mut row_ptr = row_ptr - row_inc;
    let mut pixel_ptr = row_ptr;
    let mut lines = lines + 1;

    while lines != 0 {
        let skip = r.byte() as i64;
        let mut rle_code = r.byte() as i8 as i64;
        if rle_code == 0 {
            break;
        }
        if skip & 0x80 != 0 {
            lines -= 1;
            row_ptr += row_inc;
            pixel_ptr = row_ptr + 2 * 8 * (skip & 0x7f);
        } else {
            pixel_ptr += 2 * 8 * skip;
        }
        check!(pixel_ptr, 0, limit);

        if rle_code == -1 {
            continue;
        }

        if rle_code < 0 {
            // Two bytes are 16 pixels, repeated.
            rle_code = -rle_code;
            let pi0 = bits8(r.byte());
            let pi1 = bits8(r.byte());
            check!(pixel_ptr, rle_code * 2 * 8, limit);
            for _ in 0..rle_code {
                f.put(pixel_ptr, &pi0);
                f.put(pixel_ptr + 8, &pi1);
                pixel_ptr += 16;
            }
        } else {
            // Bytes of 8 pixels each, two per code unit.
            rle_code *= 2;
            check!(pixel_ptr, rle_code * 8, limit);
            for _ in 0..rle_code {
                let x = r.byte();
                f.put(pixel_ptr, &bits8(x));
                pixel_ptr += 8;
            }
        }
    }
    Status::Done
}

fn decode_2n4bpp(
    r: &mut Reader<'_>,
    f: &mut Frame<'_>,
    mut row_ptr: i64,
    lines: i64,
    bpp: u32,
) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();
    let num_pixels: usize = if bpp == 4 { 8 } else { 16 };
    let np = num_pixels as i64;

    for _ in 0..lines {
        let mut pixel_ptr = row_ptr + np * (r.byte() as i64 - 1);
        check!(pixel_ptr, 0, limit);

        loop {
            let rle_code = r.byte() as i8 as i64;
            if rle_code == -1 {
                break;
            }
            if r.left() < 1 {
                return Status::Done;
            }
            if rle_code == 0 {
                // Another skip code.
                pixel_ptr += np * (r.byte() as i64 - 1);
                check!(pixel_ptr, 0, limit);
            } else if rle_code < 0 {
                // Four bytes are 8 or 16 palette indices, repeated.
                let rle_code = -rle_code;
                let mut pi = [0u8; 16];
                for i in (0..num_pixels).rev() {
                    pi[num_pixels - 1 - i] =
                        (r.peek() >> ((i as u32 * bpp) & 0x07)) & ((1 << bpp) - 1);
                    if i & ((num_pixels >> 2) - 1) == 0 {
                        r.skip(1);
                    }
                }
                check!(pixel_ptr, rle_code * np, limit);
                for _ in 0..rle_code {
                    f.put(pixel_ptr, &pi[..num_pixels]);
                    pixel_ptr += np;
                }
            } else {
                // Packed pixels, four bytes per code unit.
                let rle_code = rle_code * 4;
                check!(pixel_ptr, rle_code * (np >> 2), limit);
                for _ in 0..rle_code {
                    let x = r.byte();
                    if bpp == 4 {
                        f.put(pixel_ptr, &[x >> 4, x & 0x0f]);
                        pixel_ptr += 2;
                    } else {
                        f.put(pixel_ptr, &[x >> 6, (x >> 4) & 0x03, (x >> 2) & 0x03, x & 0x03]);
                        pixel_ptr += 4;
                    }
                }
            }
        }
        row_ptr += row_inc;
    }
    Status::Done
}

fn decode_8bpp(r: &mut Reader<'_>, f: &mut Frame<'_>, mut row_ptr: i64, lines: i64) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();

    for _ in 0..lines {
        let mut pixel_ptr = row_ptr + 4 * (r.byte() as i64 - 1);
        check!(pixel_ptr, 0, limit);

        loop {
            let rle_code = r.byte() as i8 as i64;
            if rle_code == -1 {
                break;
            }
            if r.left() < 1 {
                return Status::Done;
            }
            if rle_code == 0 {
                pixel_ptr += 4 * (r.byte() as i64 - 1);
                check!(pixel_ptr, 0, limit);
            } else if rle_code < 0 {
                // Four palette indices, repeated.
                let rle_code = -rle_code;
                let pi = [r.byte(), r.byte(), r.byte(), r.byte()];
                check!(pixel_ptr, rle_code * 4, limit);
                for _ in 0..rle_code {
                    f.put(pixel_ptr, &pi);
                    pixel_ptr += 4;
                }
            } else {
                // Copy; a packet that ends early leaves the rest unwritten.
                let rle_code = rle_code * 4;
                check!(pixel_ptr, rle_code, limit);
                let src = r.take_up_to(rle_code as usize);
                f.put(pixel_ptr, src);
                pixel_ptr += rle_code;
            }
        }
        row_ptr += row_inc;
    }
    Status::Done
}

fn decode_16bpp(r: &mut Reader<'_>, f: &mut Frame<'_>, mut row_ptr: i64, lines: i64) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();

    for _ in 0..lines {
        let mut pixel_ptr = row_ptr + (r.byte() as i64 - 1) * 2;
        check!(pixel_ptr, 0, limit);

        loop {
            let rle_code = r.byte() as i8 as i64;
            if rle_code == -1 {
                break;
            }
            if r.left() < 1 {
                return Status::Done;
            }
            if rle_code == 0 {
                pixel_ptr += (r.byte() as i64 - 1) * 2;
                check!(pixel_ptr, 0, limit);
            } else if rle_code < 0 {
                let rle_code = -rle_code;
                // Big-endian in the stream, native in the picture.
                let rgb16 = r.be16().to_ne_bytes();
                check!(pixel_ptr, rle_code * 2, limit);
                for _ in 0..rle_code {
                    f.put(pixel_ptr, &rgb16);
                    pixel_ptr += 2;
                }
            } else {
                check!(pixel_ptr, rle_code * 2, limit);
                for _ in 0..rle_code {
                    let rgb16 = r.be16().to_ne_bytes();
                    f.put(pixel_ptr, &rgb16);
                    pixel_ptr += 2;
                }
            }
        }
        row_ptr += row_inc;
    }
    Status::Done
}

fn decode_24bpp(r: &mut Reader<'_>, f: &mut Frame<'_>, mut row_ptr: i64, lines: i64) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();

    for _ in 0..lines {
        let mut pixel_ptr = row_ptr + (r.byte() as i64 - 1) * 3;
        check!(pixel_ptr, 0, limit);

        loop {
            let rle_code = r.byte() as i8 as i64;
            if rle_code == -1 {
                break;
            }
            if r.left() < 1 {
                return Status::Done;
            }
            if rle_code == 0 {
                pixel_ptr += (r.byte() as i64 - 1) * 3;
                check!(pixel_ptr, 0, limit);
            } else if rle_code < 0 {
                let rle_code = -rle_code;
                // Read as two bytes, then one.
                let rg: [u8; 2] = r.array();
                let b = r.byte();
                check!(pixel_ptr, rle_code * 3, limit);
                for _ in 0..rle_code {
                    f.put(pixel_ptr, &[rg[0], rg[1], b]);
                    pixel_ptr += 3;
                }
            } else {
                check!(pixel_ptr, rle_code * 3, limit);
                // Two pixels at a time, read as four bytes and two.
                for _ in 0..rle_code / 2 {
                    let a: [u8; 4] = r.array();
                    let b: [u8; 2] = r.array();
                    f.put(pixel_ptr, &a);
                    f.put(pixel_ptr + 4, &b);
                    pixel_ptr += 6;
                }
                if rle_code % 2 != 0 {
                    let rg: [u8; 2] = r.array();
                    let b = r.byte();
                    f.put(pixel_ptr, &[rg[0], rg[1], b]);
                    pixel_ptr += 3;
                }
            }
        }
        row_ptr += row_inc;
    }
    Status::Done
}

fn decode_32bpp(r: &mut Reader<'_>, f: &mut Frame<'_>, mut row_ptr: i64, lines: i64) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();

    for _ in 0..lines {
        let mut pixel_ptr = row_ptr + (r.byte() as i64 - 1) * 4;
        check!(pixel_ptr, 0, limit);

        loop {
            let rle_code = r.byte() as i8 as i64;
            if rle_code == -1 {
                break;
            }
            if r.left() < 1 {
                return Status::Done;
            }
            if rle_code == 0 {
                pixel_ptr += (r.byte() as i64 - 1) * 4;
                check!(pixel_ptr, 0, limit);
            } else if rle_code < 0 {
                let rle_code = -rle_code;
                let argb: [u8; 4] = r.array();
                check!(pixel_ptr, rle_code * 4, limit);
                for _ in 0..rle_code {
                    f.put(pixel_ptr, &argb);
                    pixel_ptr += 4;
                }
            } else {
                check!(pixel_ptr, rle_code * 4, limit);
                // Two pixels at a time, read as eight bytes.
                for _ in 0..rle_code / 2 {
                    let a: [u8; 8] = r.array();
                    f.put(pixel_ptr, &a);
                    pixel_ptr += 8;
                }
                if rle_code % 2 != 0 {
                    let a: [u8; 4] = r.array();
                    f.put(pixel_ptr, &a);
                    pixel_ptr += 4;
                }
            }
        }
        row_ptr += row_inc;
    }
    Status::Done
}

/// Decode one packet onto `frame`, which holds the previous picture.
///
/// `bits` is the bit depth the frame was created with. The caller has
/// already asked `check_packet` and got `Verdict::Decode`.
pub fn decode(packet: &[u8], bits: u32, frame: &mut Frame<'_>) -> Status {
    let Some((start_line, lines)) = region(packet, frame.height) else {
        return Status::Duplicate;
    };
    // The 4-byte size and the 2-byte header, and 8 more bytes when the
    // header gives a region.
    let body = if lines_header(packet) { 14 } else { 6 };
    let mut r = Reader { data: &packet[body..] };
    let row_ptr = (frame.stride * start_line) as i64;
    let lines = lines as i64;
    match bits {
        1 | 33 => decode_1bpp(&mut r, frame, row_ptr, lines),
        2 | 34 => decode_2n4bpp(&mut r, frame, row_ptr, lines, 2),
        4 | 36 => decode_2n4bpp(&mut r, frame, row_ptr, lines, 4),
        8 | 40 => decode_8bpp(&mut r, frame, row_ptr, lines),
        16 => decode_16bpp(&mut r, frame, row_ptr, lines),
        24 => decode_24bpp(&mut r, frame, row_ptr, lines),
        32 => decode_32bpp(&mut r, frame, row_ptr, lines),
        _ => Status::UnknownDepth,
    }
}

/// Whether the packet header is followed by a first line and a line count.
/// Only called on packets `region` accepted, which are at least 8 bytes.
fn lines_header(packet: &[u8]) -> bool {
    packet.get(5).is_some_and(|b| b & 0x08 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: u8 = 0xee;

    fn run(body: &[u8], bits: u32, w: usize, h: usize, stride: usize) -> ([u8; 256], Status) {
        let mut pkt = [0u8; 128];
        let n = 6 + body.len();
        pkt[3] = n as u8;
        pkt[6..n].copy_from_slice(body);
        run_raw(&pkt[..n], bits, w, h, stride)
    }

    fn run_raw(pkt: &[u8], bits: u32, w: usize, h: usize, stride: usize) -> ([u8; 256], Status) {
        let mut buf = [E; 256];
        let need = stride * (h - 1) + row_len(w, bits).unwrap();
        let mut frame = Frame::new(&mut buf[..need], stride, w, h, bits).unwrap();
        let st = decode(pkt, bits, &mut frame);
        (buf, st)
    }

    #[test]
    fn header_checks() {
        // Short packets repeat the previous picture.
        assert_eq!(check_packet(&[0; 7], 4, 95), Verdict::Duplicate);
        // Declared size 1000, 8 bytes present: more than 95% missing.
        let mut p = [0u8; 8];
        p[2] = 0x03;
        p[3] = 0xe8;
        assert_eq!(check_packet(&p, 4, 95), Verdict::Invalid);
        assert_eq!(check_packet(&p, 4, 100), Verdict::Decode);
        // The top two bits of the size are flags.
        let p = [0xc0, 0, 0, 8, 0, 0, 0, 0];
        assert_eq!(check_packet(&p, 4, 95), Verdict::Decode);
        // Region header: needs 14 bytes, and must fit the picture.
        let mut p = [0u8; 14];
        p[5] = 8;
        assert_eq!(check_packet(&p[..13], 4, 95), Verdict::Duplicate);
        p[7] = 1; // start line 1
        p[11] = 3; // 3 lines
        assert_eq!(check_packet(&p, 4, 95), Verdict::Decode);
        p[11] = 4;
        assert_eq!(check_packet(&p, 4, 95), Verdict::Duplicate);
        p[7] = 5;
        p[11] = 0;
        assert_eq!(check_packet(&p, 4, 95), Verdict::Duplicate);
    }

    #[test]
    fn eight_bit_run_copy_skip() {
        // Row 0: run of 2 x [1 2 3 4]. Row 1: skip one group, copy one group.
        let body = [1, 0xfe, 1, 2, 3, 4, 0xff, 2, 1, 5, 6, 7, 8, 0xff];
        let (buf, st) = run(&body, 8, 8, 2, 16);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[0..9], &[1, 2, 3, 4, 1, 2, 3, 4, E]);
        assert_eq!(&buf[16..24], &[E, E, E, E, 5, 6, 7, 8]);
    }

    #[test]
    fn region_header() {
        // Start at line 1, one line; 8-bit.
        let pkt = [0, 0, 0, 21, 0, 8, 0, 1, 0, 0, 0, 1, 0, 0, 1, 1, 9, 9, 9, 9, 0xff];
        let (buf, st) = run_raw(&pkt, 8, 4, 3, 8);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[0..4], &[E; 4]);
        assert_eq!(&buf[8..12], &[9; 4]);
        assert_eq!(&buf[16..20], &[E; 4]);
    }

    #[test]
    fn run_crosses_padding_into_next_row() {
        // Width 4, stride 8: a run of 3 groups covers row 0, its padding
        // and the first group of row 1.
        let body = [1, 0xfd, 1, 2, 3, 4, 0xff];
        let (buf, st) = run(&body, 8, 4, 2, 8);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[0..12], &[1, 2, 3, 4, E, E, E, E, 1, 2, 3, 4]);
    }

    #[test]
    fn run_past_the_picture_stops_decoding() {
        // 4 groups do not fit in 2 rows of stride 8 when starting at 4.
        let body = [2, 0xfc, 1, 2, 3, 4, 0xff];
        let (buf, st) = run(&body, 8, 8, 2, 8);
        assert_eq!(st, Status::OutOfBounds);
        assert_eq!(&buf[0..16], &[E; 16]);
        // A skip byte of 0 moves to before the picture.
        let (_, st) = run(&[0, 0xff], 8, 8, 2, 8);
        assert_eq!(st, Status::OutOfBounds);
    }

    #[test]
    fn one_bit() {
        // New line, run of 1 x (0xa5, 0x0f); then on the same line a copy
        // of 2 bytes after skipping nothing.
        let body = [0x80, 0xff, 0x80, 0xfe, 0xa5, 0x0f, 0x00, 0x01, 0xff, 0x01, 0, 0];
        // First pair (0x80, -1): new line, nothing else. Second pair
        // (0x80, -2): next line, run of 2.
        let (buf, st) = run(&body, 1, 64, 2, 64);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[0..2], &[E, E]);
        let row = &buf[64..];
        assert_eq!(&row[0..16], &[1, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 1, 1, 1, 1]);
        assert_eq!(&row[16..32], &row[0..16]);
        assert_eq!(&row[32..48], &[1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(row[48], E);
    }

    #[test]
    fn two_and_four_bit() {
        let body = [1, 0xff, 1, 0xfe, 0x12, 0x34, 0x56, 0x78, 0xff];
        // First line is empty (skip 1, end). Second: run of 2.
        let (buf, st) = run(&body, 4, 16, 2, 16);
        assert_eq!(st, Status::Done);
        assert_eq!(buf[0], E);
        assert_eq!(&buf[16..32], &[1, 2, 3, 4, 5, 6, 7, 8, 1, 2, 3, 4, 5, 6, 7, 8]);

        let body = [1, 1, 0x1b, 0xe4, 0x00, 0xff, 0xff];
        let (buf, st) = run(&body, 2, 16, 1, 16);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[0..16], &[0, 1, 2, 3, 3, 2, 1, 0, 0, 0, 0, 0, 3, 3, 3, 3]);

        let body = [1, 0xff, 1, 0xff, 1, 0xfe, 0x1b, 0xe4, 0x00, 0xff, 0xff];
        let (buf, st) = run(&body, 2, 32, 3, 32);
        assert_eq!(st, Status::Done);
        let row = &buf[64..];
        assert_eq!(&row[0..16], &[0, 1, 2, 3, 3, 2, 1, 0, 0, 0, 0, 0, 3, 3, 3, 3]);
        assert_eq!(&row[16..32], &row[0..16]);
    }

    #[test]
    fn sixteen_bit_is_byte_swapped() {
        let body = [1, 0xfe, 0x12, 0x34, 1, 0x56, 0x78, 0xff];
        let (buf, st) = run(&body, 16, 3, 1, 8);
        assert_eq!(st, Status::Done);
        let a = 0x1234u16.to_ne_bytes();
        let b = 0x5678u16.to_ne_bytes();
        assert_eq!(&buf[0..7], &[a[0], a[1], a[0], a[1], b[0], b[1], E]);
    }

    #[test]
    fn twenty_four_bit_short_packet_reads_zeros_in_groups() {
        // Copy of 3 pixels with 8 of 9 bytes present: the first pair reads
        // 4 + 2 bytes, the odd pixel reads 2 (present) and 1 (missing).
        let body = [1, 3, 1, 2, 3, 4, 5, 6, 7, 8];
        let (buf, st) = run(&body, 24, 3, 1, 16);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[0..10], &[1, 2, 3, 4, 5, 6, 7, 8, 0, E]);
        // With 5 of 6 bytes, the 2-byte read fails as a whole.
        let body = [1, 2, 1, 2, 3, 4, 5];
        let (buf, _) = run(&body, 24, 3, 1, 16);
        assert_eq!(&buf[0..7], &[1, 2, 3, 4, 0, 0, E]);
        // Run.
        let body = [2, 0xfe, 7, 8, 9, 0xff];
        let (buf, _) = run(&body, 24, 3, 1, 16);
        assert_eq!(&buf[0..10], &[E, E, E, 7, 8, 9, 7, 8, 9, E]);
    }

    #[test]
    fn thirty_two_bit() {
        let body = [1, 0xfe, 1, 2, 3, 4, 3, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 0xff];
        let (buf, st) = run(&body, 32, 5, 1, 32);
        assert_eq!(st, Status::Done);
        assert_eq!(
            &buf[0..21],
            &[1, 2, 3, 4, 1, 2, 3, 4, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, E]
        );
    }
}
