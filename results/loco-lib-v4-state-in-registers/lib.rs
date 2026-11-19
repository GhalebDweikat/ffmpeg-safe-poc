//! Safe Rust port of FFmpeg's LOCO lossless video decoder (`loco.c`), with
//! the parts of the bit reader (`get_bits.h`) and of the Golomb-Rice reader
//! (`golomb.h`, `get_ur_golomb_jpegls`) that it uses.
//!
//! The port is bit-exact with the C decoder: the same pictures, and an
//! error for exactly the packets the C decoder rejects. Three things in it
//! come from how the C code behaves rather than from the format:
//!
//! * The C bit reader may read a few bits past the end of the packet (it
//!   finds FFmpeg's zero padding there) and go on decoding, but its
//!   position then stays past the end, and `loco.c` rejects the packet
//!   when the plane is finished. So a packet is good only if every code
//!   lies wholly inside it, and that is what this port checks.
//! * A code value is computed in 32 bits and wraps; a value of all ones,
//!   which takes a megabyte of zero bits, is the C reader's error value.
//! * A chroma plane of an odd-sized picture is coded with the size rounded
//!   down, so its last column (or row) is never written. A plane coded
//!   with width 0 still gets the first pixel of every row.

#![forbid(unsafe_code)]
#![no_std]

#[cfg(test)]
mod reference;

/// The pixel format a stream decodes to; `loco.c` picks it from the mode
/// field of the extradata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Modes -1, 1, 2 (YUY2, UYVY): planar 4:2:2.
    Yuv422,
    /// Modes -4, 5 (YV12): planar 4:2:0, V coded before U.
    Yuv420,
    /// Modes -2, 3 (RGB): planar G, B, R, stored bottom-up.
    Gbr,
    /// Modes -3, 4 (RGBA): planar G, B, R, A, stored bottom-up.
    Gbra,
}

/// What the extradata says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub version: i32,
    pub mode: i32,
    pub lossy: i32,
    pub format: Format,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// Fewer than 12 bytes of extradata.
    ExtradataTooSmall,
    /// The lossy parameter is negative or above 65536.
    LossyTooLarge { version: i32, lossy: i32 },
    /// The mode is not one of the nine known ones.
    UnknownMode { version: i32, lossy: i32, mode: i32 },
}

fn rl32(d: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

impl Config {
    pub fn from_extradata(extradata: &[u8]) -> Result<Config, InitError> {
        if extradata.len() < 12 {
            return Err(InitError::ExtradataTooSmall);
        }
        let version = rl32(extradata, 0);
        // Version 1 has no lossy mode; every other version reads the field.
        let lossy = if version == 1 { 0 } else { rl32(extradata, 8) };
        if !(0..=65536).contains(&lossy) {
            return Err(InitError::LossyTooLarge { version, lossy });
        }
        let mode = rl32(extradata, 4);
        let format = match mode {
            -1 | 1 | 2 => Format::Yuv422,
            -2 | 3 => Format::Gbr,
            -4 | 5 => Format::Yuv420,
            -3 | 4 => Format::Gbra,
            _ => return Err(InitError::UnknownMode { version, lossy, mode }),
        };
        Ok(Config { version, mode, lossy, format })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The packet is damaged or too short. The C decoder reports every
    /// such case as "Input data too small".
    TooSmall,
    /// The planes handed over do not fit the picture.
    BadArgs,
}

/// The visible part of one plane: `rows` rows, `stride` bytes apart, the
/// last one cut off after `row_len` bytes. Row 0 is the top row.
pub struct Plane<'a> {
    data: &'a mut [u8],
    stride: usize,
    row_len: usize,
    rows: usize,
}

impl<'a> Plane<'a> {
    pub fn new(data: &'a mut [u8], stride: usize, row_len: usize, rows: usize) -> Option<Self> {
        if row_len == 0 || rows == 0 || stride < row_len {
            return None;
        }
        let need = stride.checked_mul(rows - 1)?.checked_add(row_len)?;
        let data = data.get_mut(..need)?;
        Some(Plane { data, stride, row_len, rows })
    }
}

/// Fill a bit window (see [`Bits`]) to at least 56 bits if it holds fewer
/// than 32. `None` if that takes the last 8 bytes of the buffer, which are
/// left to [`Bits::refill_tail`].
#[inline(always)]
fn refilled(acc: u64, n: u32, rest: &[u8]) -> Option<(u64, u32, &[u8])> {
    if n >= 32 {
        return Some((acc, n, rest));
    }
    let (word, _) = rest.split_first_chunk::<8>()?;
    // The bits that do not make a whole byte are ORed in as well; the next
    // refill puts the same bits in the same place.
    let acc = acc | u64::from_be_bytes(*word) >> n;
    let bytes = ((63 - n) >> 3) as usize;
    Some((acc, n + 8 * bytes as u32, &rest[bytes..]))
}

/// One Golomb-Rice code with parameter `k` (at most 9) from a bit window:
/// `z` zero bits, a one, `k` bits; its value is `(z << k) + bits`. `None`,
/// with nothing changed, if the window does not hold the whole code.
#[inline(always)]
fn window_code(acc: &mut u64, n: &mut u32, k: u32) -> Option<u32> {
    let z = acc.leading_zeros();
    let len = z + 1 + k;
    if len > *n {
        return None;
    }
    // The one bit and the k bits after it, as a number: (1 << k) + bits.
    let aligned = *acc << (z & 63);
    let top = (aligned >> (63 - k)) as u32;
    *acc = aligned << (k + 1);
    *n -= len;
    Some(top.wrapping_add(z.wrapping_sub(1) << k))
}

/// The bit reader: a 64-bit window on the stream, next bit on top. The
/// rarely used paths take and return it by value, so that the compiler can
/// keep it in registers on the usual one.
#[derive(Clone, Copy)]
struct Bits<'a> {
    /// The bytes not yet in the window.
    rest: &'a [u8],
    /// The top `n` bits are the next bits of the stream. Below them are
    /// bits of the bytes still in `rest`, or zeros.
    acc: u64,
    n: u32,
    /// How many of the `n` bits, at the low end, stand for bits past the
    /// end of the buffer. They are zero, like FFmpeg's packet padding.
    past_end: u32,
}

impl<'a> Bits<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Bits { rest: buf, acc: 0, n: 0, past_end: 0 }
    }

    /// Fill the window to at least 56 bits if it holds fewer than 32.
    #[inline(always)]
    fn refill(&mut self) {
        match refilled(self.acc, self.n, self.rest) {
            Some((acc, n, rest)) => (self.acc, self.n, self.rest) = (acc, n, rest),
            None => *self = self.refill_tail(),
        }
    }

    /// The last bytes of the packet, and after them the padding.
    #[cold]
    #[inline(never)]
    fn refill_tail(mut self) -> Self {
        while self.n <= 56 {
            if let Some((&byte, rest)) = self.rest.split_first() {
                self.acc |= u64::from(byte) << (56 - self.n);
                self.rest = rest;
            } else {
                self.past_end = self.past_end.saturating_add(8);
            }
            self.n += 8;
        }
        self
    }

    /// One Golomb-Rice code with parameter `k` (at most 9), in 32 bits.
    /// This is `get_ur_golomb_jpegls(gb, k, INT_MAX, 0)`. `None` if the
    /// buffer ends before the one bit, or if the value is the C function's
    /// -1.
    #[inline(always)]
    fn code(&mut self, k: u32) -> Option<u32> {
        self.refill();
        if let Some(v) = window_code(&mut self.acc, &mut self.n, k) {
            return Some(v);
        }
        let (bits, v) = self.code_slow(k);
        *self = bits;
        v
    }

    /// A code that does not fit in what the window holds.
    #[cold]
    #[inline(never)]
    fn code_slow(mut self, k: u32) -> (Self, Option<u32>) {
        let mut zeros: u32 = 0;
        loop {
            let z = self.acc.leading_zeros();
            if z < self.n {
                zeros = zeros.wrapping_add(z);
                self.acc = self.acc << z << 1;
                self.n -= z + 1;
                break;
            }
            // Nothing but zeros in the window.
            zeros = zeros.wrapping_add(self.n);
            self.acc = 0;
            self.n = 0;
            if self.rest.is_empty() {
                return (self, None);
            }
            self.refill();
        }
        self.refill();
        let low = (self.acc >> 1 >> (63 - k)) as u32;
        self.acc <<= k;
        self.n -= k;
        let v = (zeros << k).wrapping_add(low);
        (self, if v == u32::MAX { None } else { Some(v) })
    }

    /// The number of bytes used of a buffer of `total` bytes, rounded up;
    /// `None` if bits past its end were used.
    fn used(&self, total: usize) -> Option<usize> {
        let unread = self.n.checked_sub(self.past_end)? as usize;
        let bits = (total - self.rest.len()) * 8 - unread;
        Some((bits + 7) >> 3)
    }
}

/// `loco_get_rice_param`.
#[inline(always)]
fn rice_param(sum: i32, count: i32) -> u32 {
    let mut cnt = 0;
    let mut val = count;
    while sum > val && cnt < 9 {
        val <<= 1;
        cnt += 1;
    }
    cnt
}

/// `loco_update_rice_param`: the new sum and count. The sum is a C `int`
/// that a packet with a megabyte of zero bytes can overflow; it wraps
/// here, as it does in the compiled C.
#[inline(always)]
fn rice_update(sum: i32, count: i32, val: i32) -> (i32, i32) {
    let sum = sum.wrapping_add(val);
    if count == 15 {
        (sum >> 1, 8)
    } else {
        (sum, count + 1)
    }
}

/// `n` updates with a value of 0, for the pixels of a zero run. The count
/// goes up by one per pixel; each time it reaches 16 it drops to 8 and the
/// sum is halved.
#[inline(always)]
fn rice_update_zeros(sum: i32, count: i32, n: u32) -> (i32, i32) {
    let c = count as u32 + n;
    if c < 16 {
        (sum, c as i32)
    } else {
        let halvings = (c - 16) / 8 + 1;
        (sum >> halvings.min(31), (8 + (c - 16) % 8) as i32)
    }
}

/// `mid_pred`: the median of three.
#[inline(always)]
fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// A pixel: its prediction (`loco_predict`) from the pixel above (`a`),
/// the one to the left (`b`) and the one above that (`c`), plus its
/// residual.
#[inline(always)]
fn pixel(a: i32, b: i32, c: i32, residual: u8) -> i32 {
    i32::from((mid_pred(a, a + b - c, b) as u8).wrapping_add(residual))
}

/// Why [`Rice::fast_pixels`] stopped.
enum Stop {
    /// The row is done.
    End,
    /// The next pixel needs the general reader.
    Slow,
    /// The next pixel has a zero residual, and its code has been read; the
    /// length of the zero run that follows needs the general reader.
    SlowRun,
}

/// `RICEContext`.
struct Rice<'a> {
    bits: Bits<'a>,
    save: i32,
    run: i32,
    run2: i32,
    sum: i32,
    count: i32,
    lossy: u32,
}

impl<'a> Rice<'a> {
    fn new(buf: &'a [u8], lossy: u32) -> Self {
        Rice { bits: Bits::new(buf), save: 0, run: 0, run2: 0, sum: 8, count: 1, lossy }
    }

    /// The part of `loco_get_rice` that follows a zero residual when zero
    /// runs are on: read the length of the run.
    fn run_length(&mut self) -> Option<()> {
        self.run = self.bits.code(2)? as i32;
        if self.run > 1 {
            self.save = self.save.wrapping_add(self.run.wrapping_add(1));
        } else {
            self.save = self.save.wrapping_sub(3);
        }
        Some(())
    }

    /// `loco_get_rice`: the residual of the next pixel, of which only the
    /// low 8 bits matter. `None` is the C function's `INT_MIN`.
    fn next(&mut self) -> Option<u8> {
        if self.run > 0 {
            // we have zero run
            self.run -= 1;
            (self.sum, self.count) = rice_update(self.sum, self.count, 0);
            return Some(0);
        }
        let v = self.bits.code(rice_param(self.sum, self.count))?;
        (self.sum, self.count) = rice_update(self.sum, self.count, (v.wrapping_add(1) >> 1) as i32);
        if v == 0 {
            if self.save >= 0 {
                self.run_length()?;
            } else {
                self.run2 = self.run2.wrapping_add(1);
            }
            return Some(0);
        }
        let v = (v >> 1).wrapping_add(self.lossy) ^ 0u32.wrapping_sub(v & 1);
        if self.run2 > 0 {
            if self.run2 > 2 {
                self.save = self.save.wrapping_add(self.run2);
            } else {
                self.save = self.save.wrapping_sub(3);
            }
            self.run2 = 0;
        }
        if v == 0x8000_0000 {
            return None;
        }
        Some(v as u8)
    }

    /// Decode pixels of a row for as long as it takes nothing unusual:
    /// zero runs, and codes that fit in the bit window (so they are short,
    /// and cannot be one of the two error values). `above` is the row
    /// decoded before, `left` the pixel to the left and the one above it.
    /// Returns the number of pixels done.
    ///
    /// This does the same as calling [`Rice::next`] for every pixel. It is
    /// a function of its own, with the state in local variables and no
    /// calls, so that the compiler keeps the state in registers.
    #[inline(never)]
    fn fast_pixels(&mut self, above: &[u8], cur: &mut [u8], left: &mut (i32, i32)) -> (usize, Stop) {
        let Bits { mut rest, mut acc, mut n, past_end } = self.bits;
        let (mut sum, mut count) = (self.sum, self.count);
        let (mut save, mut run, mut run2) = (self.save, self.run, self.run2);
        let lossy = self.lossy;
        let (mut b, mut c) = *left;
        let mut done = 0;
        let mut pixels = cur.iter_mut().zip(above);

        let stop = 'row: loop {
            if run > 0 {
                // A zero run: these pixels are their predictions.
                let mut m = 0u32;
                for (out, &a) in pixels.by_ref().take(run as usize) {
                    let a = i32::from(a);
                    b = pixel(a, b, c, 0);
                    *out = b as u8;
                    c = a;
                    m += 1;
                }
                done += m as usize;
                run -= m as i32;
                (sum, count) = rice_update_zeros(sum, count, m);
                if run > 0 {
                    // The row ended before the run did.
                    break Stop::End;
                }
            }
            // Coded pixels, up to the next zero run.
            loop {
                let Some((out, &a)) = pixels.next() else {
                    break 'row Stop::End;
                };
                // Stopping here leaves the pixel to the caller, which
                // counts the pixels done, not this iterator.
                match refilled(acc, n, rest) {
                    Some(window) => (acc, n, rest) = window,
                    None => break 'row Stop::Slow,
                }
                let Some(v) = window_code(&mut acc, &mut n, rice_param(sum, count)) else {
                    break 'row Stop::Slow;
                };
                (sum, count) = rice_update(sum, count, (v.wrapping_add(1) >> 1) as i32);
                let residual;
                let mut run_starts = false;
                if v == 0 {
                    residual = 0;
                    if save >= 0 {
                        match refilled(acc, n, rest) {
                            Some(window) => (acc, n, rest) = window,
                            None => break 'row Stop::SlowRun,
                        }
                        let Some(length) = window_code(&mut acc, &mut n, 2) else {
                            break 'row Stop::SlowRun;
                        };
                        run = length as i32;
                        save = if run > 1 { save.wrapping_add(run + 1) } else { save.wrapping_sub(3) };
                        run_starts = run > 0;
                    } else {
                        run2 = run2.wrapping_add(1);
                    }
                } else {
                    residual = ((v >> 1).wrapping_add(lossy) ^ 0u32.wrapping_sub(v & 1)) as u8;
                    if run2 > 0 {
                        save = if run2 > 2 { save.wrapping_add(run2) } else { save.wrapping_sub(3) };
                        run2 = 0;
                    }
                }
                let a = i32::from(a);
                b = pixel(a, b, c, residual);
                *out = b as u8;
                c = a;
                done += 1;
                if run_starts {
                    continue 'row;
                }
            }
        };

        self.bits = Bits { rest, acc, n, past_end };
        (self.sum, self.count) = (sum, count);
        (self.save, self.run, self.run2) = (save, run, run2);
        *left = (b, c);
        (done, stop)
    }
}

/// Decode one row. `above` is the row decoded before it, of the same
/// length; `left` is the pixel to the left of the first one and the pixel
/// above that.
fn decode_row(rc: &mut Rice<'_>, above: &[u8], cur: &mut [u8], left: &mut (i32, i32)) -> Option<()> {
    if above.len() != cur.len() {
        return None;
    }
    let mut x = 0;
    while x < cur.len() {
        let (done, stop) = rc.fast_pixels(above.get(x..)?, &mut cur[x..], left);
        x += done;
        let residual = match stop {
            Stop::End => continue,
            Stop::Slow => rc.next()?,
            Stop::SlowRun => {
                rc.run_length()?;
                0
            }
        };
        let a = i32::from(*above.get(x)?);
        left.0 = pixel(a, left.0, left.1, residual);
        *cur.get_mut(x)? = left.0 as u8;
        left.1 = a;
        x += 1;
    }
    Some(())
}

/// What the first row has above it: with the pixel above and the one
/// above-left equal, the prediction is the pixel to the left.
static NOTHING_ABOVE: [u8; 1024] = [0; 1024];

/// `loco_decode_plane`. `width` and `height` are the coded size, which for
/// chroma is the picture size halved and rounded down, and may be 0; like
/// the C loop, this writes the first pixel of the first row, and the first
/// pixel of every further row, whatever the width is. `bottom_up` stands
/// for the C code's negative stride. Returns the number of bytes used.
fn decode_plane(
    plane: &mut Plane<'_>,
    width: usize,
    height: usize,
    bottom_up: bool,
    buf: &[u8],
    lossy: u32,
) -> Option<usize> {
    // init_get_bits8 refuses buffers too large for a 31-bit bit count.
    if buf.is_empty() || buf.len().checked_mul(8)? >= i32::MAX as usize - 64 * 8 {
        return None;
    }
    let (w, h) = (width.max(1), height.max(1));
    if w > plane.row_len || h > plane.rows {
        return None;
    }
    let stride = plane.stride;
    let rows = plane.rows;
    let mut rc = Rice::new(buf, lossy);

    // The first row: every pixel is predicted by the one to its left, the
    // first one by 128.
    {
        let at = if bottom_up { (rows - 1) * stride } else { 0 };
        let mut left = (128, 0);
        for part in plane.data[at..at + w].chunks_mut(NOTHING_ABOVE.len()) {
            decode_row(&mut rc, &NOTHING_ABOVE[..part.len()], part, &mut left)?;
        }
    }

    for j in 1..h {
        // The row decoded before this one, and this one.
        let (above, cur) = if bottom_up {
            let m = rows - 1 - j;
            let (head, tail) = plane.data.split_at_mut((m + 1) * stride);
            (&tail[..w], &mut head[m * stride..][..w])
        } else {
            let (head, tail) = plane.data.split_at_mut(j * stride);
            (&head[(j - 1) * stride..][..w], &mut tail[..w])
        };
        // Taking the pixel above for both the pixel to the left and the
        // one above-left makes the prediction for the first pixel the
        // pixel above it, as in the C code's left column.
        let first = i32::from(above[0]);
        decode_row(&mut rc, above, cur, &mut (first, first))?;
    }

    rc.bits.used(buf.len())
}

/// `rotate_faulty_loco`: undoes a shear that the LOCO encoder puts into RGB
/// pictures of odd width. Row `y`, counted from the bottom, is moved left
/// by `y` pixels and filled up from the start of the row after it.
fn rotate_faulty_loco(plane: &mut Plane<'_>, width: usize, height: usize) {
    let stride = plane.stride;
    let rows = plane.rows;
    if width > plane.row_len || height > rows {
        return;
    }
    for y in 1..height {
        if width >= y {
            // Row y of the bottom-up picture is memory row rows - 1 - y;
            // the row after it is the memory row before.
            let cur = (rows - 1 - y) * stride;
            plane.data.copy_within(cur + y..cur + width, cur);
            if y + 1 < height {
                let next = (rows - 2 - y) * stride;
                plane.data.copy_within(next..next + y, cur + width - y);
            }
        }
    }
}

/// For each coded plane of a format, in stream order: its index in the
/// frame and the right shifts of its width and height.
fn layout(format: Format) -> &'static [(usize, u32, u32)] {
    match format {
        Format::Yuv422 => &[(0, 0, 0), (1, 1, 0), (2, 1, 0)],
        Format::Yuv420 => &[(0, 0, 0), (2, 1, 1), (1, 1, 1)],
        Format::Gbr => &[(1, 0, 0), (0, 0, 0), (2, 0, 0)],
        Format::Gbra => &[(1, 0, 0), (0, 0, 0), (2, 0, 0), (3, 0, 0)],
    }
}

/// The number of planes a frame of this format has.
pub fn plane_count(format: Format) -> usize {
    layout(format).len()
}

/// The visible size (bytes per row, rows) of plane `index` of a picture.
pub fn plane_size(format: Format, index: usize, width: usize, height: usize) -> (usize, usize) {
    let chroma = index == 1 || index == 2;
    match format {
        Format::Yuv422 if chroma => (width.div_ceil(2), height),
        Format::Yuv420 if chroma => (width.div_ceil(2), height.div_ceil(2)),
        _ => (width, height),
    }
}

/// `decode_frame`: decode one packet into the planes of a new frame.
/// `planes` are the frame's planes in frame order. Returns the number of
/// packet bytes used.
pub fn decode_frame(
    format: Format,
    lossy: i32,
    width: usize,
    height: usize,
    packet: &[u8],
    planes: &mut [Plane<'_>],
) -> Result<usize, Error> {
    let coded = layout(format);
    if planes.len() < coded.len() || width == 0 || height == 0 {
        return Err(Error::BadArgs);
    }
    let lossy = u32::try_from(lossy).map_err(|_| Error::BadArgs)?;
    for (index, plane) in planes.iter().enumerate().take(coded.len()) {
        if (plane.row_len, plane.rows) != plane_size(format, index, width, height) {
            return Err(Error::BadArgs);
        }
    }
    let bottom_up = matches!(format, Format::Gbr | Format::Gbra);

    let mut buf = packet;
    let mut decoded = 0;
    for (n, &(index, wshift, hshift)) in coded.iter().enumerate() {
        if n > 0 {
            // ADVANCE_BY_DECODED: a plane must leave at least one byte
            // for the next one.
            if decoded >= buf.len() {
                return Err(Error::TooSmall);
            }
            buf = &buf[decoded..];
        }
        decoded = decode_plane(&mut planes[index], width >> wshift, height >> hshift, bottom_up, buf, lossy)
            .ok_or(Error::TooSmall)?;
    }
    if format == Format::Gbr && width & 1 != 0 {
        for plane in planes.iter_mut().take(3) {
            rotate_faulty_loco(plane, width, height);
        }
    }
    Ok(packet.len() - (buf.len() - decoded))
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// Writes Golomb-Rice codes.
    struct Writer {
        bytes: Vec<u8>,
        nbits: usize,
    }

    impl Writer {
        fn new() -> Self {
            Writer { bytes: vec![], nbits: 0 }
        }
        fn bit(&mut self, bit: u32) {
            if self.nbits % 8 == 0 {
                self.bytes.push(0);
            }
            if bit != 0 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.nbits % 8);
            }
            self.nbits += 1;
        }
        fn code(&mut self, v: u32, k: u32) {
            for _ in 0..v >> k {
                self.bit(0);
            }
            self.bit(1);
            for i in (0..k).rev() {
                self.bit(v >> i & 1);
            }
        }
    }

    /// Encode residuals (none of them 0, so no run codes are needed) for
    /// one plane, lossless.
    fn encode_nonzero(residuals: &[i32]) -> Vec<u8> {
        let mut w = Writer::new();
        let (mut sum, mut count) = (8i32, 1i32);
        for &s in residuals {
            assert!(s != 0);
            let v = if s > 0 { 2 * s as u32 } else { (-2 * s - 1) as u32 };
            let mut k = 0;
            while sum > count << k && k < 9 {
                k += 1;
            }
            w.code(v, k);
            sum += ((v + 1) >> 1) as i32;
            count += 1;
            if count == 16 {
                sum >>= 1;
                count >>= 1;
            }
        }
        w.bytes
    }

    fn extradata(version: i32, mode: i32, lossy: i32) -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..4].copy_from_slice(&version.to_le_bytes());
        e[4..8].copy_from_slice(&mode.to_le_bytes());
        e[8..12].copy_from_slice(&lossy.to_le_bytes());
        e
    }

    #[test]
    fn extradata_is_parsed_like_decode_init() {
        assert_eq!(Config::from_extradata(&[0; 11]), Err(InitError::ExtradataTooSmall));
        let c = Config::from_extradata(&extradata(1, 3, 77)).unwrap();
        assert_eq!((c.lossy, c.format), (0, Format::Gbr));
        let c = Config::from_extradata(&extradata(2, -4, 5)).unwrap();
        assert_eq!((c.lossy, c.format), (5, Format::Yuv420));
        let c = Config::from_extradata(&extradata(9, 2, 65536)).unwrap();
        assert_eq!((c.lossy, c.format), (65536, Format::Yuv422));
        assert!(matches!(
            Config::from_extradata(&extradata(2, 1, 65537)),
            Err(InitError::LossyTooLarge { .. })
        ));
        assert!(matches!(Config::from_extradata(&extradata(2, 1, -1)), Err(InitError::LossyTooLarge { .. })));
        assert!(matches!(Config::from_extradata(&extradata(1, 0, 0)), Err(InitError::UnknownMode { .. })));
        assert!(matches!(Config::from_extradata(&extradata(1, 6, 0)), Err(InitError::UnknownMode { .. })));
    }

    #[test]
    fn long_codes() {
        // z zero bits, a one, then k bits, for z on both sides of what the
        // window holds; then a short code.
        for z in [0u32, 1, 22, 23, 24, 25, 30, 46, 47, 55, 56, 63, 64, 65, 120, 1000] {
            for k in 0..10u32 {
                let mut w = Writer::new();
                let v = (z << k) | (0x155 & ((1 << k) - 1));
                w.code(3, 1);
                w.code(v, k);
                w.code(5, k);
                w.bit(1);
                let mut bits = Bits::new(&w.bytes);
                assert_eq!(bits.code(1), Some(3));
                assert_eq!(bits.code(k), Some(v), "z = {z}, k = {k}");
                assert_eq!(bits.code(k), Some(5));
                // The straight port reads the same.
                let mut gb = reference::GetBits::new(&w.bytes).unwrap();
                assert_eq!(gb.ur_golomb_jpegls(1), 3);
                assert_eq!(gb.ur_golomb_jpegls(k), v);
                assert_eq!(gb.ur_golomb_jpegls(k), 5);
                assert_eq!(bits.used(w.bytes.len()), Some(((gb.index + 7) >> 3) as usize));
            }
        }
    }

    #[test]
    fn codes_at_the_end_of_the_buffer() {
        // Nothing but zeros: no code.
        assert_eq!(Bits::new(&[0, 0]).code(0), None);
        // A one in the last bit and k bits read from the padding: a value,
        // but the buffer is overrun.
        let mut bits = Bits::new(&[0x01]);
        assert_eq!(bits.code(9), Some(7 << 9));
        assert_eq!(bits.used(1), None);
        // The same code with its 9 bits inside the buffer.
        let mut bits = Bits::new(&[0x01, 0xff, 0x80]);
        assert_eq!(bits.code(9), Some((7 << 9) + 0x1ff));
        assert_eq!(bits.used(3), Some(3));
        let mut bits = Bits::new(&[0x01, 0xff, 0x80, 0x00]);
        assert_eq!(bits.code(9), Some((7 << 9) + 0x1ff));
        assert_eq!(bits.used(4), Some(3));
    }

    #[test]
    fn zero_runs_update_the_state_like_single_pixels() {
        for start in 1..16 {
            for sum in [-1000, -1, 0, 1, 7, 8, 100, 65535, i32::MAX] {
                for n in 1..200u32 {
                    let mut one = (sum, start);
                    for _ in 0..n {
                        one = rice_update(one.0, one.1, 0);
                    }
                    assert_eq!(one, rice_update_zeros(sum, start, n));
                }
            }
        }
    }

    /// A 3x2 4:2:2 picture whose planes hold hand-made residuals.
    #[test]
    fn small_yuv422_frame() {
        // Luma 3x2: residuals in decoding order.
        let luma = encode_nonzero(&[10, 1, -1, 2, 3, -5]);
        // Chroma 1x2 each (width 3 / 2).
        let u = encode_nonzero(&[-28, 4]);
        let v = encode_nonzero(&[100, -100]);
        let mut pkt = vec![];
        pkt.extend(&luma);
        pkt.extend(&u);
        pkt.extend(&v);

        let (mut y, mut cb, mut cr) = ([0u8; 16 + 3], [7u8; 8 + 2], [7u8; 8 + 2]);
        let mut planes = [
            Plane::new(&mut y, 16, 3, 2).unwrap(),
            Plane::new(&mut cb, 8, 2, 2).unwrap(),
            Plane::new(&mut cr, 8, 2, 2).unwrap(),
        ];
        let used = decode_frame(Format::Yuv422, 0, 3, 2, &pkt, &mut planes).unwrap();
        assert_eq!(used, pkt.len());
        // Top row: 128 + 10, then left + residual.
        assert_eq!(&y[0..3], &[138, 139, 138]);
        // Second row: above + 2; then median(a, a + b - c, b) + residual.
        // x = 1: a = 139, b = 140, c = 138: median(139, 141, 140) = 140, + 3.
        // x = 2: a = 138, b = 143, c = 139: median(138, 142, 143) = 142, - 5.
        assert_eq!(&y[16..19], &[140, 143, 137]);
        // Chroma: one coded column; the second visible column is untouched.
        assert_eq!((cb[0], cb[1], cb[8], cb[9]), (100, 7, 104, 7));
        assert_eq!((cr[0], cr[8]), (228, 128));

        // One byte short: the last plane's last code is cut off.
        let mut planes = [
            Plane::new(&mut y, 16, 3, 2).unwrap(),
            Plane::new(&mut cb, 8, 2, 2).unwrap(),
            Plane::new(&mut cr, 8, 2, 2).unwrap(),
        ];
        assert_eq!(
            decode_frame(Format::Yuv422, 0, 3, 2, &pkt[..pkt.len() - 1], &mut planes),
            Err(Error::TooSmall)
        );
    }

    /// Zero residuals: the first zero is followed by a run length.
    #[test]
    fn zero_run() {
        // k = 3 at the start: v = 0 is "1000"; then the run code with k = 2:
        // run = 5 is one zero, a one, "01". The run covers the 5 remaining
        // pixels of a 3x2 plane. Total 8 bits.
        let pkt = [0b1000_0101u8, 0x80, 0x80, 0xff];
        let mut g = [0u8; 16 + 3];
        let mut b = [0u8; 16 + 3];
        let mut r = [0u8; 16 + 3];
        let mut planes = [
            Plane::new(&mut g, 16, 3, 2).unwrap(),
            Plane::new(&mut b, 16, 3, 2).unwrap(),
            Plane::new(&mut r, 16, 3, 2).unwrap(),
        ];
        // The third plane gets one byte, which is not enough.
        assert_eq!(decode_frame(Format::Gbr, 0, 3, 2, &pkt, &mut planes), Err(Error::TooSmall));
        // The first coded plane is B (plane 1): all 128.
        assert_eq!(&b[0..3], &[128; 3]);
        assert_eq!(&b[16..19], &[128; 3]);
    }

    /// RGB pictures of odd width are sheared back.
    #[test]
    fn odd_width_rgb_is_rotated() {
        // 3x3, rows bottom-up in the stream. Memory rows (top first):
        let mut d = [0u8; 2 * 8 + 3];
        d[0..3].copy_from_slice(&[7, 8, 9]); // stream row 2
        d[8..11].copy_from_slice(&[4, 5, 6]); // stream row 1
        d[16..19].copy_from_slice(&[1, 2, 3]); // stream row 0
        let mut p = Plane::new(&mut d, 8, 3, 3).unwrap();
        rotate_faulty_loco(&mut p, 3, 3);
        // Row 1: moved left by one, filled with the start of row 2.
        // Row 2: moved left by two; nothing follows it.
        assert_eq!(&d[16..19], &[1, 2, 3]);
        assert_eq!(&d[8..11], &[5, 6, 7]);
        assert_eq!(&d[0..3], &[9, 8, 9]);
    }

    #[test]
    fn planes_must_fit() {
        let mut y = [0u8; 4];
        let mut planes = [Plane::new(&mut y, 2, 2, 2).unwrap()];
        assert_eq!(decode_frame(Format::Yuv422, 0, 2, 2, &[0x80], &mut planes), Err(Error::BadArgs));
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self, n: u32) -> u32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) % u64::from(n)) as u32
        }
    }

    /// Random buffers, decoded as one plane by this decoder and by the
    /// straight port: both accept the same ones, use the same number of
    /// bytes and give the same picture.
    #[test]
    fn agrees_with_the_straight_port() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut accepted, mut rejected) = (0, 0);
        for case in 0..60000 {
            let w = rng.next(12) as usize;
            let h = rng.next(8) as usize;
            let (row_len, rows) = (w.max(1) + rng.next(2) as usize, h.max(1) + rng.next(2) as usize);
            let stride = row_len + rng.next(5) as usize;
            let bottom_up = rng.next(2) == 1;
            let lossy = if rng.next(3) == 0 { rng.next(300) } else { 0 };
            // Bytes with few bits set give short codes and zero runs;
            // long stretches of zero bytes give the long codes.
            let style = rng.next(5);
            let len = 1 + rng.next(if style == 4 { 400 } else { 3 * (w * h) as u32 + 4 }) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| match style {
                    0 => rng.next(256) as u8,
                    1 => (rng.next(256) & rng.next(256)) as u8,
                    2 => (rng.next(256) | rng.next(256)) as u8,
                    3 => [0x80, 0x88, 0xa5, 0xff, 0x40, 0x00][rng.next(6) as usize],
                    _ => {
                        if rng.next(40) == 0 {
                            rng.next(256) as u8
                        } else {
                            0
                        }
                    }
                })
                .collect();

            let size = stride * (rows - 1) + row_len;
            let mut mine = vec![0x55u8; size];
            let mut theirs = vec![0x55u8; size];
            let got = decode_plane(
                &mut Plane::new(&mut mine, stride, row_len, rows).unwrap(),
                w,
                h,
                bottom_up,
                &buf,
                lossy,
            );
            // The straight port reports an overrun as one byte too many.
            let want = reference::decode_plane(&mut theirs, stride, rows, w, h, bottom_up, &buf, lossy)
                .filter(|&used| used <= buf.len());
            assert_eq!(got, want, "case {case}: {w}x{h} bottom_up={bottom_up} buf={buf:?}");
            if got.is_some() {
                assert_eq!(mine, theirs, "case {case}: {w}x{h} bottom_up={bottom_up} buf={buf:?}");
                accepted += 1;
            } else {
                rejected += 1;
            }
        }
        // The test is worth something only if both outcomes are common.
        assert!(accepted > 5000 && rejected > 5000, "{accepted} accepted, {rejected} rejected");
    }
}
