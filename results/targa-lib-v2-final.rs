//! Safe Rust port of FFmpeg's Truevision Targa image decoder (`targa.c`).
//!
//! One packet is one picture, and the picture's size and pixel format come
//! from the packet. Decoding is therefore split in two: [`parse`] reads the
//! header, so that the caller can allocate a frame of the right size and
//! format, and [`decode`] fills that frame.
//!
//! The port is bit-exact with the C decoder, including the things the C
//! decoder does that look like accidents:
//!
//! * A literal packet that is cut short by the end of the data leaves the
//!   pixels it did not have untouched, and still counts them. If that
//!   completes the picture, the picture is output with whatever the frame
//!   buffer held before.
//! * The palette is written before the pixels are decoded, so a picture that
//!   then fails has still changed the palette of the frame buffer it was
//!   given (FFmpeg hands the same buffer to a later picture).
//! * 15-bit pictures are not mirrored when the right-to-left flag is set;
//!   16-bit pictures are.
//! * The header is read with FFmpeg's checked reader: a field that does not
//!   fit in the packet reads as zero and ends the packet.
//!
//! One thing is not copied: a run packet whose pixel value is cut short by
//! the end of the data. The C decoder then repeats bytes of an uninitialised
//! stack array; this port reads the missing bytes as zero.

#![forbid(unsafe_code)]
#![no_std]

const TGA_BW: u8 = 3;
const TGA_RLE: u8 = 8;
const TGA_RIGHTTOLEFT: u8 = 0x10;
const TGA_TOPTOBOTTOM: u8 = 0x20;
const TGA_INTERLEAVE2: u8 = 0x40;
const TGA_INTERLEAVE4: u8 = 0x80;

/// Pixel format of the decoded picture (FFmpeg's names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Gray8,
    Pal8,
    Rgb555Le,
    Bgr24,
    Bgra,
}

/// Why a header was refused. The order is the order of the C decoder's
/// checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// The packet is shorter than the header says the identifier and colour
    /// map are.
    NotEnoughData,
    /// Bits per pixel is not 8, 15, 16, 24 or 32.
    BitDepth,
    /// The colour map does not fit in 256 entries. The C decoder has set the
    /// pixel format by the time it notices.
    Palette { format: Format },
}

/// Why a picture was refused after its frame was allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Colour map entries are not 15, 16, 24 or 32 bits.
    PaletteEntrySize,
    /// The packet ends inside the colour map.
    PaletteData,
    /// An uncompressed picture is longer than the packet.
    ImageData,
    /// A compressed picture ends before the last row.
    RleOutOfData,
    /// The last packet of a compressed picture runs past the last row.
    RleOutOfBounds,
}

/// A parsed header. `width` and `height` may be zero; the caller checks
/// them (FFmpeg does it in `ff_set_dimensions`).
#[derive(Debug, Clone, Copy)]
pub struct Header<'a> {
    pub format: Format,
    pub width: usize,
    pub height: usize,
    compr: u8,
    flags: u8,
    bpp: u8,
    first_clr: usize,
    colors: usize,
    csize: u8,
    /// The packet after the header and the identifier.
    body: &'a [u8],
}

/// What [`parse`] found.
#[derive(Debug, Clone, Copy)]
pub struct Parsed<'a> {
    /// The header has no colour map but describes one; the description was
    /// ignored. (The C decoder logs a warning.)
    pub colormap_ignored: bool,
    pub header: Result<Header<'a>, HeaderError>,
}

/// Mirrors libavcodec's checked bytestream reader: a read past the end
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

    fn le16(&mut self) -> u16 {
        match self.data.split_first_chunk::<2>() {
            Some((b, rest)) => {
                self.data = rest;
                u16::from_le_bytes(*b)
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

    /// The next `n` bytes, or as many as are left.
    fn take(&mut self, n: usize) -> &'a [u8] {
        let (head, rest) = self.data.split_at(n.min(self.data.len()));
        self.data = rest;
        head
    }
}

/// Read the 18-byte header and skip the identifier.
pub fn parse(packet: &[u8]) -> Parsed<'_> {
    let mut r = Reader { data: packet };

    let idlen = r.byte() as usize;
    let pal = r.byte();
    let compr = r.byte();
    let mut first_clr = r.le16() as usize;
    let mut colors = r.le16() as usize;
    let mut csize = r.byte();
    r.skip(4); // x and y origin
    let width = r.le16() as usize;
    let height = r.le16() as usize;
    let bpp = r.byte();
    let flags = r.byte();

    let mut colormap_ignored = false;
    if pal == 0 && (first_clr != 0 || colors != 0 || csize != 0) {
        // The specification says to ignore these values in this case.
        colormap_ignored = true;
        first_clr = 0;
        colors = 0;
        csize = 0;
    }

    let header = (|| {
        if r.left() < idlen + 2 * colors {
            return Err(HeaderError::NotEnoughData);
        }
        r.skip(idlen);

        let format = match bpp {
            8 if compr & !TGA_RLE == TGA_BW => Format::Gray8,
            8 => Format::Pal8,
            15 | 16 => Format::Rgb555Le,
            24 => Format::Bgr24,
            32 => Format::Bgra,
            _ => return Err(HeaderError::BitDepth),
        };

        if colors != 0 && colors + first_clr > 256 {
            return Err(HeaderError::Palette { format });
        }

        Ok(Header {
            format,
            width,
            height,
            compr,
            flags,
            bpp,
            first_clr,
            colors,
            csize,
            body: r.data,
        })
    })();

    Parsed { colormap_ignored, header }
}

impl Header<'_> {
    /// Bytes per pixel.
    pub fn depth(&self) -> usize {
        (self.bpp as usize + 1) >> 3
    }

    /// The picture type says there is no picture. The C decoder sets the
    /// dimensions and outputs nothing.
    pub fn no_data(&self) -> bool {
        self.compr & !TGA_RLE == 0
    }

    fn is_rle(&self) -> bool {
        self.compr & TGA_RLE != 0
    }

    fn raw_size(&self) -> u64 {
        (self.width * self.depth()) as u64 * self.height as u64
    }

    /// The check the C decoder makes before it allocates the frame: an
    /// uncompressed picture must fit in what follows the identifier (the
    /// colour map is not taken into account here; [`decode`] checks again).
    pub fn data_present(&self) -> bool {
        self.is_rle() || self.body.len() as u64 >= self.raw_size()
    }
}

/// The visible picture: `height` rows of `width * depth` bytes, `stride`
/// bytes apart, row 0 at the top; and the palette of a `Pal8` picture, 256
/// native-endian 32-bit entries.
pub struct Frame<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_len: usize,
    height: usize,
    pal: Option<&'a mut [u8; 1024]>,
}

impl<'a> Frame<'a> {
    /// A frame for the picture `hdr` describes. `data` must hold at least
    /// `stride * (height - 1) + width * depth` bytes; anything after that is
    /// not touched. `pal` is needed for `Pal8` and ignored otherwise.
    pub fn new(
        data: &'a mut [u8],
        stride: usize,
        pal: Option<&'a mut [u8; 1024]>,
        hdr: &Header<'_>,
    ) -> Option<Self> {
        let row_len = hdr.width * hdr.depth();
        if hdr.width == 0 || hdr.height == 0 || stride < row_len {
            return None;
        }
        let need = stride.checked_mul(hdr.height - 1)?.checked_add(row_len)?;
        let data = data.get_mut(..need)?;
        let pal = if hdr.format == Format::Pal8 { Some(pal?) } else { None };
        Some(Frame { data, stride, row_len, height: hdr.height, pal })
    }
}

/// The rows of a frame in the order the packet stores them: from the top or
/// from the bottom, and with interleaving rows 0, n, 2n.. first, then 1,
/// n+1.., and so on.
struct Rows<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_len: usize,
    height: usize,
    top_down: bool,
    interleave: usize,
    /// The current row, counted in storage order. Always below `height`.
    y: usize,
}

impl<'a> Rows<'a> {
    fn new(frame: &'a mut Frame<'_>, flags: u8) -> Self {
        Rows {
            data: &mut *frame.data,
            stride: frame.stride,
            row_len: frame.row_len,
            height: frame.height,
            top_down: flags & TGA_TOPTOBOTTOM != 0,
            interleave: if flags & TGA_INTERLEAVE2 != 0 {
                2
            } else if flags & TGA_INTERLEAVE4 != 0 {
                4
            } else {
                1
            },
            y: 0,
        }
    }

    /// The current row.
    fn row(&mut self) -> &mut [u8] {
        let row = if self.top_down { self.y } else { self.height - 1 - self.y };
        &mut self.data[row * self.stride..][..self.row_len]
    }

    /// Move to the next row; `false` after the last one, as `advance_line`
    /// in the C decoder returns NULL.
    fn advance(&mut self) -> bool {
        let y = self.y + self.interleave;
        if y < self.height {
            self.y = y;
            return true;
        }
        let y = (y + 1) & (self.interleave - 1);
        if y != 0 && y < self.height {
            self.y = y;
            return true;
        }
        false
    }
}

/// Run-length coded pictures with `D` bytes per pixel.
fn decode_rle<const D: usize>(mut src: &[u8], rows: &mut Rows<'_>) -> Result<(), DecodeError> {
    // What is left of the current row. Never empty at the top of the loop.
    let mut cur = rows.row();

    loop {
        let Some((&code, rest)) = src.split_first() else {
            return Err(DecodeError::RleOutOfData);
        };
        src = rest;
        let mut count = (code & 0x7f) as usize + 1;

        if code & 0x80 == 0 {
            // `count` pixels follow. The usual case: they are all there and
            // end before the row does.
            if count * D < cur.len() {
                if let Some((pixels, rest)) = src.split_at_checked(count * D) {
                    let (dst, tail) = core::mem::take(&mut cur).split_at_mut(count * D);
                    dst.copy_from_slice(pixels);
                    cur = tail;
                    src = rest;
                    continue;
                }
            }
            loop {
                let n = count.min(cur.len() / D);
                let (dst, tail) = core::mem::take(&mut cur).split_at_mut(n * D);
                // If the data runs out, the rest of the pixels keep what
                // the frame held, and are counted all the same.
                let (pixels, rest) = src.split_at(src.len().min(n * D));
                dst[..pixels.len()].copy_from_slice(pixels);
                src = rest;
                cur = tail;
                count -= n;
                if cur.is_empty() {
                    if !rows.advance() {
                        // That was the last row.
                        return if count != 0 { Err(DecodeError::RleOutOfBounds) } else { Ok(()) };
                    }
                    cur = rows.row();
                }
                if count == 0 {
                    break;
                }
            }
        } else {
            // One pixel follows, to be repeated `count` times. (Where the
            // data ends inside it, the C decoder uses uninitialised bytes;
            // here the missing bytes are zero.)
            let pix: [u8; D] = match src.split_first_chunk::<D>() {
                Some((pix, rest)) => {
                    src = rest;
                    *pix
                }
                None => {
                    let mut pix = [0; D];
                    pix[..src.len()].copy_from_slice(src);
                    src = &[];
                    pix
                }
            };
            if count * D < cur.len() {
                let (dst, tail) = core::mem::take(&mut cur).split_at_mut(count * D);
                for px in dst.as_chunks_mut::<D>().0 {
                    *px = pix;
                }
                cur = tail;
                continue;
            }
            loop {
                let n = count.min(cur.len() / D);
                let (dst, tail) = core::mem::take(&mut cur).split_at_mut(n * D);
                for px in dst.as_chunks_mut::<D>().0 {
                    *px = pix;
                }
                cur = tail;
                count -= n;
                if cur.is_empty() {
                    if !rows.advance() {
                        // That was the last row.
                        return if count != 0 { Err(DecodeError::RleOutOfBounds) } else { Ok(()) };
                    }
                    cur = rows.row();
                }
                if count == 0 {
                    break;
                }
            }
        }
    }
}

/// Mirror every row of a picture with `D` bytes per pixel.
fn mirror<const D: usize>(frame: &mut Frame<'_>) {
    let row_len = frame.row_len;
    for row in frame.data.chunks_mut(frame.stride) {
        if let Some(row) = row.get_mut(..row_len) {
            row.as_chunks_mut::<D>().0.reverse();
        }
    }
}

fn read_palette(src: &[u8], entry: usize, dst: &mut [u8]) {
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(entry)) {
        let v = match *s {
            [b, g, r, a] => u32::from_le_bytes([b, g, r, a]),
            [b, g, r] => u32::from_le_bytes([b, g, r, 0xff]),
            [lo, hi] => {
                // RGB555, with the top bits repeated at the bottom.
                let v = u32::from(u16::from_le_bytes([lo, hi]));
                let v = (v & 0x7c00) << 9 | (v & 0x03e0) << 6 | (v & 0x001f) << 3;
                0xff00_0000 | v | (v & 0xe0e0e0) >> 5
            }
            _ => 0,
        };
        d.copy_from_slice(&v.to_ne_bytes());
    }
}

/// Decode the picture `hdr` describes into `frame`. On an error the frame
/// may have been written in part, as in the C decoder.
pub fn decode(hdr: &Header<'_>, frame: &mut Frame<'_>) -> Result<(), DecodeError> {
    let mut r = Reader { data: hdr.body };

    if hdr.colors != 0 {
        let entry = match hdr.csize {
            32 => 4,
            24 => 3,
            15 | 16 => 2,
            _ => return Err(DecodeError::PaletteEntrySize),
        };
        let pal_size = hdr.colors * entry;
        match frame.pal.as_deref_mut() {
            // Should not occur, but skip the palette anyway.
            None => r.skip(pal_size),
            Some(pal) => {
                if r.left() < pal_size {
                    return Err(DecodeError::PaletteData);
                }
                let dst = &mut pal[4 * hdr.first_clr..][..4 * hdr.colors];
                read_palette(r.take(pal_size), entry, dst);
            }
        }
    }

    let mut rows = Rows::new(frame, hdr.flags);
    if hdr.is_rle() {
        match hdr.format {
            Format::Gray8 | Format::Pal8 => decode_rle::<1>(r.data, &mut rows)?,
            Format::Rgb555Le => decode_rle::<2>(r.data, &mut rows)?,
            Format::Bgr24 => decode_rle::<3>(r.data, &mut rows)?,
            Format::Bgra => decode_rle::<4>(r.data, &mut rows)?,
        }
    } else {
        if (r.left() as u64) < hdr.raw_size() {
            return Err(DecodeError::ImageData);
        }
        let row_len = rows.row_len;
        loop {
            let src = r.take(row_len);
            rows.row()[..src.len()].copy_from_slice(src);
            if !rows.advance() {
                break;
            }
        }
    }

    // Right-to-left pictures are mirrored. The C decoder's switch has no
    // case for 15 bits, so those stay as they are.
    if hdr.flags & TGA_RIGHTTOLEFT != 0 && hdr.bpp != 15 {
        match hdr.format {
            Format::Gray8 | Format::Pal8 => mirror::<1>(frame),
            Format::Rgb555Le => mirror::<2>(frame),
            Format::Bgr24 => mirror::<3>(frame),
            Format::Bgra => mirror::<4>(frame),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 18-byte header.
    fn header(compr: u8, w: u16, h: u16, bpp: u8, flags: u8) -> [u8; 18] {
        let mut p = [0u8; 18];
        p[2] = compr;
        p[12..14].copy_from_slice(&w.to_le_bytes());
        p[14..16].copy_from_slice(&h.to_le_bytes());
        p[16] = bpp;
        p[17] = flags;
        p
    }

    fn packet(hdr: [u8; 18], body: &[u8]) -> ([u8; 256], usize) {
        let mut p = [0u8; 256];
        p[..18].copy_from_slice(&hdr);
        p[18..18 + body.len()].copy_from_slice(body);
        (p, 18 + body.len())
    }

    /// Decode into a buffer filled with 0xee, rows `stride` apart.
    fn run(pkt: &[u8], stride: usize) -> ([u8; 256], [u8; 1024], Result<(), DecodeError>) {
        let mut buf = [0xeeu8; 256];
        let mut pal = [0x11u8; 1024];
        let hdr = parse(pkt).header.unwrap();
        assert!(hdr.data_present());
        let need = stride * (hdr.height - 1) + hdr.width * hdr.depth();
        let mut frame = Frame::new(&mut buf[..need], stride, Some(&mut pal), &hdr).unwrap();
        let res = decode(&hdr, &mut frame);
        (buf, pal, res)
    }

    #[test]
    fn raw_bottom_up_and_top_down() {
        let (p, n) = packet(header(2, 2, 2, 24, 0), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let (buf, _, res) = run(&p[..n], 8);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..8], &[7, 8, 9, 10, 11, 12, 0xee, 0xee]);
        assert_eq!(&buf[8..14], &[1, 2, 3, 4, 5, 6]);

        let (p, n) = packet(header(2, 2, 2, 24, 0x20), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let (buf, _, res) = run(&p[..n], 8);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..6], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(&buf[8..14], &[7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn rle_run_and_literal_cross_rows() {
        // 3x2 gray, top-down: a run of 4 (crossing into row 1), then 2 literal.
        let (p, n) = packet(header(11, 3, 2, 8, 0x20), &[0x83, 7, 0x01, 8, 9]);
        let (buf, _, res) = run(&p[..n], 4);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..7], &[7, 7, 7, 0xee, 7, 8, 9]);
    }

    #[test]
    fn rle_errors() {
        // Ends before the picture is complete.
        let (p, n) = packet(header(11, 3, 2, 8, 0x20), &[0x83, 7]);
        let (buf, _, res) = run(&p[..n], 4);
        assert_eq!(res, Err(DecodeError::RleOutOfData));
        assert_eq!(&buf[..7], &[7, 7, 7, 0xee, 7, 0xee, 0xee]);
        // The last run is too long.
        let (p, n) = packet(header(11, 3, 2, 8, 0x20), &[0x86, 7]);
        assert_eq!(run(&p[..n], 4).2, Err(DecodeError::RleOutOfBounds));
    }

    #[test]
    fn short_literal_keeps_old_pixels() {
        // 4x1, 16 bits: 4 literal pixels announced, 3 bytes present.
        let (p, n) = packet(header(10, 4, 1, 16, 0x20), &[0x03, 1, 2, 3]);
        let (buf, _, res) = run(&p[..n], 8);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..8], &[1, 2, 3, 0xee, 0xee, 0xee, 0xee, 0xee]);
    }

    #[test]
    fn short_run_value_reads_as_zero() {
        let (p, n) = packet(header(10, 2, 1, 24, 0x20), &[0x81, 5]);
        let (buf, _, res) = run(&p[..n], 8);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..7], &[5, 0, 0, 5, 0, 0, 0xee]);
    }

    #[test]
    fn interleave_order() {
        // 1x5 gray, top-down, 2-way: stored rows are 0, 2, 4, 1, 3.
        let (p, n) = packet(header(3, 1, 5, 8, 0x60), &[0, 2, 4, 1, 3]);
        let (buf, _, res) = run(&p[..n], 1);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..5], &[0, 1, 2, 3, 4]);
        // 4-way: 0, 4, 1, 5, 2, 3. Bottom-up counts from the last row.
        let (p, n) = packet(header(3, 1, 6, 8, 0x80), &[0, 4, 1, 5, 2, 3]);
        let (buf, _, res) = run(&p[..n], 1);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..6], &[5, 4, 3, 2, 1, 0]);
        // 4-way with fewer than four rows.
        let (p, n) = packet(header(3, 1, 3, 8, 0xa0), &[0, 1, 2]);
        let (buf, _, res) = run(&p[..n], 1);
        assert_eq!(res, Ok(()));
        assert_eq!(&buf[..3], &[0, 1, 2]);
    }

    #[test]
    fn mirror_all_depths_but_15() {
        let (p, n) = packet(header(2, 3, 1, 24, 0x30), &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(&run(&p[..n], 16).0[..9], &[7, 8, 9, 4, 5, 6, 1, 2, 3]);
        let (p, n) = packet(header(2, 2, 1, 32, 0x30), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&run(&p[..n], 16).0[..8], &[5, 6, 7, 8, 1, 2, 3, 4]);
        let (p, n) = packet(header(2, 2, 1, 16, 0x30), &[1, 2, 3, 4]);
        assert_eq!(&run(&p[..n], 16).0[..4], &[3, 4, 1, 2]);
        let (p, n) = packet(header(2, 2, 1, 15, 0x30), &[1, 2, 3, 4]);
        assert_eq!(&run(&p[..n], 16).0[..4], &[1, 2, 3, 4]);
        let (p, n) = packet(header(3, 4, 1, 8, 0x30), &[1, 2, 3, 4]);
        assert_eq!(&run(&p[..n], 16).0[..4], &[4, 3, 2, 1]);
    }

    #[test]
    fn palettes() {
        // Two 24-bit entries starting at index 3.
        let mut h = header(1, 1, 1, 8, 0x20);
        h[1] = 1;
        h[3] = 3;
        h[5] = 2;
        h[7] = 24;
        let (p, n) = packet(h, &[1, 2, 3, 4, 5, 6, 9]);
        let (buf, pal, res) = run(&p[..n], 1);
        assert_eq!(res, Ok(()));
        assert_eq!(buf[0], 9);
        assert_eq!(&pal[8..12], &[0x11; 4]);
        assert_eq!(pal[12..16], 0xff03_0201u32.to_ne_bytes());
        assert_eq!(pal[16..20], 0xff06_0504u32.to_ne_bytes());
        assert_eq!(&pal[20..24], &[0x11; 4]);

        // One 16-bit entry: 0x7fff is white, 0x7c00 is red.
        h[3] = 0;
        h[7] = 16;
        let (p, n) = packet(h, &[0xff, 0x7f, 0x00, 0x7c, 9]);
        let (_, pal, res) = run(&p[..n], 1);
        assert_eq!(res, Ok(()));
        assert_eq!(pal[0..4], 0xffff_ffffu32.to_ne_bytes());
        assert_eq!(pal[4..8], 0xffff_0000u32.to_ne_bytes());

        // 32-bit entries are stored as they are.
        h[5] = 1;
        h[7] = 32;
        let (p, n) = packet(h, &[1, 2, 3, 4, 9]);
        assert_eq!(run(&p[..n], 1).1[0..4], 0x0403_0201u32.to_ne_bytes());

        // Unknown entry size, and a palette that is cut short.
        h[7] = 8;
        let (p, n) = packet(h, &[1, 2, 3, 4, 9]);
        assert_eq!(run(&p[..n], 1).2, Err(DecodeError::PaletteEntrySize));
        h[7] = 32;
        h[5] = 2;
        let (p, n) = packet(h, &[1, 2, 3, 4, 9]);
        assert_eq!(run(&p[..n], 1).2, Err(DecodeError::PaletteData));
    }

    #[test]
    fn header_checks() {
        // A colour map description without a colour map is ignored.
        let mut h = header(2, 1, 1, 24, 0);
        h[5] = 200;
        h[6] = 200;
        let parsed = parse(&h);
        assert!(parsed.colormap_ignored);
        let hdr = parsed.header.unwrap();
        assert_eq!((hdr.format, hdr.width, hdr.height), (Format::Bgr24, 1, 1));
        assert!(!hdr.data_present());

        // With a colour map, 2 bytes per colour must be there, whatever the
        // entry size.
        h[1] = 1;
        h[5] = 3;
        h[6] = 0;
        let (p, n) = packet(h, &[0; 5]);
        assert_eq!(parse(&p[..n]).header.unwrap_err(), HeaderError::NotEnoughData);
        assert!(parse(&p[..n + 1]).header.is_ok());

        // More than 256 entries: refused after the format is known.
        h[3] = 254;
        let (p, n) = packet(h, &[0; 6]);
        assert_eq!(
            parse(&p[..n]).header.unwrap_err(),
            HeaderError::Palette { format: Format::Bgr24 }
        );

        let h = header(2, 1, 1, 17, 0);
        assert_eq!(parse(&h).header.unwrap_err(), HeaderError::BitDepth);

        // 8 bits: gray only for type 3 (and 11).
        assert_eq!(parse(&header(3, 1, 1, 8, 0)).header.unwrap().format, Format::Gray8);
        assert_eq!(parse(&header(11, 1, 1, 8, 0)).header.unwrap().format, Format::Gray8);
        assert_eq!(parse(&header(2, 1, 1, 8, 0)).header.unwrap().format, Format::Pal8);
        assert!(parse(&header(8, 1, 1, 8, 0)).header.unwrap().no_data());

        // The identifier is skipped.
        let mut h = header(3, 1, 1, 8, 0x20);
        h[0] = 2;
        let (p, n) = packet(h, &[50, 51, 52]);
        assert_eq!(run(&p[..n], 1).0[0], 52);
    }

    #[test]
    fn truncated_header_reads_as_zero() {
        // 17 bytes: the flags byte is missing and reads as 0 (bottom-up).
        let h = header(3, 1, 1, 8, 0x20);
        let hdr = parse(&h[..17]).header.unwrap();
        assert_eq!((hdr.width, hdr.height, hdr.flags), (1, 1, 0));
        // 15 bytes: the height does not fit, so it is 0 and so is the depth.
        assert_eq!(parse(&h[..15]).header.unwrap_err(), HeaderError::BitDepth);
        // An odd byte before a 16-bit field is dropped with the field.
        let mut h = header(3, 1, 1, 8, 0);
        h[1] = 1;
        h[3] = 0xff;
        assert_eq!(parse(&h[..4]).header.unwrap_err(), HeaderError::BitDepth);
    }

    #[test]
    fn frame_must_match_header() {
        let h = header(2, 4, 3, 32, 0);
        let hdr = parse(&h).header.unwrap();
        let mut buf = [0u8; 256];
        assert!(Frame::new(&mut buf[..16 * 2 + 16], 16, None, &hdr).is_some());
        assert!(Frame::new(&mut buf[..16 * 2 + 15], 16, None, &hdr).is_none());
        assert!(Frame::new(&mut buf, 15, None, &hdr).is_none());
        let h = header(1, 4, 3, 8, 0);
        let hdr = parse(&h).header.unwrap();
        assert!(Frame::new(&mut buf, 4, None, &hdr).is_none());
        let h = header(2, 0, 3, 24, 0);
        let hdr = parse(&h).header.unwrap();
        assert!(Frame::new(&mut buf, 4, None, &hdr).is_none());
    }
}
