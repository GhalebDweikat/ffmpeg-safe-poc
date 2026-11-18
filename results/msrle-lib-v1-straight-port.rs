//! Safe Rust port of FFmpeg's Microsoft RLE video decoder: `msrle.c` and the
//! parts of `msrledec.c` it can reach (4-bit, 8-bit and 24-bit pictures).
//!
//! The port is bit-exact with the C decoder, including the things the C
//! decoder does that look like accidents:
//!
//! * In 8-bit and 24-bit pictures the C code checks runs against the frame's
//!   stride, not against the picture width, so a run may continue into the
//!   padding at the end of a row. The port makes the same decisions from the
//!   same stride, but is only handed the visible picture and drops the bytes
//!   that would land in the padding.
//! * A run that does not fit is skipped without consuming its pixel value,
//!   so the value is then read as the next code.
//! * Decode errors do not stop the picture from being output.

#![forbid(unsafe_code)]
#![no_std]

/// How decoding a packet ended. The picture is output in every case; the
/// caller only uses this to choose a log message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The stream ended with an end-of-picture code.
    Done,
    /// The packet was a raw, bottom-up picture and was copied.
    Uncompressed,
    /// 8/24-bit: the packet ran out without an end-of-picture code.
    NoEndOfPicture,
    /// 8/24-bit: an end-of-line code below the bottom row that is not
    /// followed by an end-of-picture marker.
    NextLineBeyondPicture,
    /// 8/24-bit: a skip code moved outside the picture.
    SkipBeyondPicture,
    /// 8/24-bit: a copy code asks for more bytes than the packet has.
    Overrun,
    /// 4-bit: the packet ran out.
    Pal4Overrun,
    /// 4-bit: a copy code does not fit in the row or in the packet.
    Pal4CopyOutOfBounds,
    /// 4-bit: a run code does not fit in the row.
    Pal4RunOutOfBounds,
    /// 4-bit: bytes left over after the last row.
    Pal4Leftover,
    /// The decoder has no RLE mode for this bit depth.
    UnknownDepth,
}

/// The visible picture: `height` rows of `row_len` bytes, `stride` bytes
/// apart. Row 0 is the top row.
pub struct Frame<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_len: usize,
    width: usize,
    height: usize,
}

/// Bytes in one row of a picture with `bits` bits per coded sample, as
/// FFmpeg lays it out (monochrome packed, palette indices one per byte).
pub fn row_len(width: usize, bits: u32) -> Option<usize> {
    match bits {
        1 => Some(width.div_ceil(8)),
        4 | 8 => Some(width),
        24 => width.checked_mul(3),
        _ => None,
    }
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
        let data = data.get_mut(..need)?;
        Some(Frame { data, stride, row_len, width, height })
    }

    /// Bytes `off .. off + len` of row `line`, cut down to the part that is
    /// inside the visible row.
    fn span(&mut self, line: usize, off: usize, len: usize) -> &mut [u8] {
        let end = (off + len).min(self.row_len);
        let off = off.min(end);
        let start = line * self.stride;
        &mut self.data[start + off..start + end]
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

    fn be16(&mut self) -> u16 {
        match self.data.split_first_chunk::<2>() {
            Some((b, rest)) => {
                self.data = rest;
                u16::from_be_bytes(*b)
            }
            None => {
                self.data = &[];
                0
            }
        }
    }

    fn skip(&mut self, n: usize) {
        self.data = &self.data[n.min(self.data.len())..];
    }

    /// The next `n` bytes; the caller has checked that they exist.
    fn take(&mut self, n: usize) -> &'a [u8] {
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        head
    }
}

/// Size of a raw picture as stored in AVI: rows padded to 32 bits.
fn raw_stride(width: usize, bits: u32) -> usize {
    (width * bits as usize).div_ceil(32) * 4
}

fn copy_raw(packet: &[u8], bits: u32, frame: &mut Frame<'_>) {
    let istride = raw_stride(frame.width, bits);
    let (width, height, row_len) = (frame.width, frame.height, frame.row_len);
    for y in 0..height {
        // The stored picture is bottom-up.
        let src = &packet[(height - 1 - y) * istride..][..istride];
        let dst = frame.span(y, 0, row_len);
        if bits == 4 {
            for (x, d) in dst.iter_mut().enumerate().take(width) {
                let b = src[x >> 1];
                *d = if x & 1 == 0 { b >> 4 } else { b & 0x0f };
            }
        } else {
            dst.copy_from_slice(&src[..row_len]);
        }
    }
}

fn decode_pal4(r: &mut Reader<'_>, frame: &mut Frame<'_>) -> Status {
    let width = frame.width;
    let mut line = frame.height as isize - 1;
    let mut x = 0usize;

    while line >= 0 && x <= width {
        if r.left() == 0 {
            return Status::Pal4Overrun;
        }
        let code = r.byte();
        if code != 0 {
            // A run alternating between the two nibbles of the next byte.
            let n = code as usize;
            if x + n > width + 1 {
                return Status::Pal4RunOutOfBounds;
            }
            let b = r.byte();
            let pair = [b >> 4, b & 0x0f];
            let dst = frame.span(line as usize, x, n);
            for (i, d) in dst.iter_mut().enumerate() {
                *d = pair[i & 1];
            }
            x += dst.len();
            continue;
        }
        match r.byte() {
            0 => {
                line -= 1;
                x = 0;
            }
            1 => return Status::Done,
            2 => {
                x += r.byte() as usize;
                line -= r.byte() as isize;
            }
            b => {
                // Copy `b` pixels, two per byte, padded to a 16-bit boundary.
                let odd = (b & 1) as usize;
                let n = (b as usize + 1) / 2;
                if x + 2 * n - odd > width || r.left() < n {
                    return Status::Pal4CopyOutOfBounds;
                }
                let src = r.take(n);
                let dst = frame.span(line as usize, x, 2 * n - odd);
                for (i, d) in dst.iter_mut().enumerate() {
                    let s = src[i >> 1];
                    *d = if i & 1 == 0 { s >> 4 } else { s & 0x0f };
                }
                x += 2 * n - odd;
                if n & 1 != 0 {
                    r.skip(1);
                }
            }
        }
    }

    if r.left() != 0 {
        return Status::Pal4Leftover;
    }
    Status::Done
}

/// 8-bit (`bpp` 1) and 24-bit (`bpp` 3) pictures.
fn decode_bytes(r: &mut Reader<'_>, frame: &mut Frame<'_>, bpp: usize) -> Status {
    // The C decoder bounds a row by the stride, not by the picture width.
    let row_cap = frame.stride;
    let px_cap = frame.stride / bpp;
    let mut line = frame.height - 1;
    let mut pos = 0usize;

    while r.left() > 0 {
        let p1 = r.byte() as usize;
        if p1 != 0 {
            // Run of one pixel value.
            if (pos + p1) * bpp > row_cap {
                continue;
            }
            let pix = [r.byte(), if bpp == 3 { r.byte() } else { 0 }, if bpp == 3 { r.byte() } else { 0 }];
            let dst = frame.span(line, pos * bpp, p1 * bpp);
            if bpp == 1 {
                dst.fill(pix[0]);
            } else {
                for px in dst.chunks_mut(3) {
                    for (d, s) in px.iter_mut().zip(pix) {
                        *d = s;
                    }
                }
            }
            pos += p1;
            continue;
        }
        let p2 = r.byte() as usize;
        match p2 {
            0 => {
                // End of line.
                let Some(next) = line.checked_sub(1) else {
                    return if r.be16() == 1 {
                        Status::Done
                    } else {
                        Status::NextLineBeyondPicture
                    };
                };
                line = next;
                pos = 0;
            }
            1 => return Status::Done,
            2 => {
                let dx = r.byte() as usize;
                let dy = r.byte() as usize;
                pos += dx;
                match line.checked_sub(dy) {
                    Some(next) if pos < px_cap => line = next,
                    _ => return Status::SkipBeyondPicture,
                }
            }
            _ => {
                // Copy `p2` pixels from the stream.
                if (pos + p2) * bpp > row_cap {
                    r.skip(2 * bpp);
                    continue;
                }
                if r.left() < p2 * bpp {
                    return Status::Overrun;
                }
                let src = r.take(p2 * bpp);
                let dst = frame.span(line, pos * bpp, p2 * bpp);
                let n = dst.len();
                dst.copy_from_slice(&src[..n]);
                pos += p2;
                // 8-bit copies are padded to 16 bits; runs are not.
                if bpp == 1 && p2 & 1 != 0 {
                    r.skip(1);
                }
            }
        }
    }

    Status::NoEndOfPicture
}

/// Decode one packet onto `frame`, which holds the previous picture.
///
/// `bits` is the bit depth the frame was created with.
pub fn decode(packet: &[u8], bits: u32, frame: &mut Frame<'_>) -> Status {
    // Same test as the C decoder: a packet exactly the size of a raw
    // picture is taken to be one.
    if frame.height.checked_mul(raw_stride(frame.width, bits)) == Some(packet.len()) {
        copy_raw(packet, bits, frame);
        return Status::Uncompressed;
    }
    let mut r = Reader { data: packet };
    match bits {
        4 => decode_pal4(&mut r, frame),
        8 => decode_bytes(&mut r, frame, 1),
        24 => decode_bytes(&mut r, frame, 3),
        _ => Status::UnknownDepth,
    }
}

/// The palette stored in the stream header: little-endian BGRx entries,
/// made opaque. Entries the header does not cover are left alone.
pub fn palette_from_extradata(extradata: &[u8], pal: &mut [u32; 256]) {
    if extradata.len() < 4 {
        return;
    }
    for (entry, src) in pal.iter_mut().zip(extradata.chunks_exact(4)) {
        *entry = 0xff00_0000 | u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(packet: &[u8], bits: u32, w: usize, h: usize, stride: usize) -> ([u8; 256], Status) {
        let mut buf = [0xeeu8; 256];
        let need = stride * (h - 1) + row_len(w, bits).unwrap();
        let mut frame = Frame::new(&mut buf[..need], stride, w, h, bits).unwrap();
        let st = decode(packet, bits, &mut frame);
        (buf, st)
    }

    #[test]
    fn eight_bit_run_and_copy() {
        // bottom row: run of 3 x 7, then end of line; top row: copy 3 bytes.
        let pkt = [3, 7, 0, 0, 0, 3, 1, 2, 3, 0, 0, 1];
        let (buf, st) = run(&pkt, 8, 4, 2, 8);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[8..12], &[7, 7, 7, 0xee]);
        assert_eq!(&buf[0..4], &[1, 2, 3, 0xee]);
    }

    #[test]
    fn run_into_padding_is_dropped_but_counted() {
        // Width 4, stride 8: a run of 6 is accepted and clipped to 4.
        // (Two rows, so the packet is not the size of a raw picture.)
        let (buf, st) = run(&[6, 9, 0, 1], 8, 4, 2, 8);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[8..12], &[9; 4]);
        assert_eq!(buf[4], 0xee);
    }

    #[test]
    fn four_bit_run() {
        let (buf, st) = run(&[5, 0x12, 0, 1], 4, 6, 2, 8);
        assert_eq!(st, Status::Done);
        assert_eq!(&buf[8..14], &[1, 2, 1, 2, 1, 0xee]);
    }

    #[test]
    fn raw_picture_is_flipped() {
        // 2x2, 8 bits: raw rows are 4 bytes each, bottom row first.
        let pkt = [1, 2, 0, 0, 3, 4, 0, 0];
        let (buf, st) = run(&pkt, 8, 2, 2, 4);
        assert_eq!(st, Status::Uncompressed);
        assert_eq!(&buf[0..2], &[3, 4]);
        assert_eq!(&buf[4..6], &[1, 2]);
    }

    #[test]
    fn palette() {
        let mut pal = [0u32; 256];
        palette_from_extradata(&[1, 2, 3, 4, 5, 6, 7, 8, 9], &mut pal);
        assert_eq!(pal[0], 0xff03_0201);
        assert_eq!(pal[1], 0xff07_0605);
        assert_eq!(pal[2], 0);
    }
}
