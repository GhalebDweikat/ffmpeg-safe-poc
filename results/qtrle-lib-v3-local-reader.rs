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
    fn put(&mut self, pos: i64, mut bytes: &[u8]) {
        let Ok(mut p) = usize::try_from(pos) else { return };
        while !bytes.is_empty() {
            // The part of `bytes` that belongs to this row and its padding,
            // and of that the part that is visible.
            let col = p % self.stride;
            let (head, rest) = bytes.split_at(bytes.len().min(self.stride - col));
            if col < self.row_len {
                let n = head.len().min(self.row_len - col);
                if let Some(dst) = self.data.get_mut(p..p + n) {
                    dst.copy_from_slice(&head[..n]);
                }
            }
            p += head.len();
            bytes = rest;
        }
    }

    /// The visible part of the row that starts at byte offset `row_ptr` of
    /// the C decoder's buffer; empty if there is no such row.
    fn row_at(&mut self, row_ptr: i64) -> &mut [u8] {
        usize::try_from(row_ptr)
            .ok()
            .and_then(|start| self.data.get_mut(start..start + self.row_len))
            .unwrap_or_default()
    }
}

/// Mirrors libavcodec's checked bytestream reader: reading past the end
/// yields zero and leaves the reader at the end.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {

    fn byte(&mut self) -> u8 {
        match self.data.split_first() {
            Some((&b, rest)) => {
                self.data = rest;
                b
            }
            None => 0,
        }
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

/// The C decoder's `CHECK_PIXEL_PTR(n)`, the other way round: whether a
/// write of `n` bytes at `pixel_ptr` (or, with `n` 0, the position itself)
/// is allowed.
#[inline(always)]
fn fits(pixel_ptr: i64, n: i64, limit: i64) -> bool {
    !(pixel_ptr + n > limit || pixel_ptr + n < 0)
}

/// The next byte of the stream, or zero once it has run out. The decoding
/// loops keep the stream in a local slice (which stays in registers) and
/// only make a `Reader` of it for the slow paths.
#[inline(always)]
fn next_byte(data: &mut &[u8]) -> u8 {
    match data.split_first() {
        Some((&b, rest)) => {
            *data = rest;
            b
        }
        None => 0,
    }
}

/// Copy `src` to `dst`, which has the same length. Up to 32 bytes are
/// copied with two fixed-size moves that may overlap, instead of a call to
/// `memcpy`.
#[inline(always)]
fn copy_bytes(dst: &mut [u8], src: &[u8]) {
    if dst.len() != src.len() {
        return;
    }
    if src.len() > 32 {
        dst.copy_from_slice(src);
        return;
    }
    macro_rules! ends {
        ($n:literal) => {
            if let (Some(a), Some(b)) = (src.first_chunk::<$n>(), src.last_chunk::<$n>()) {
                if let Some(head) = dst.first_chunk_mut::<$n>() {
                    *head = *a;
                }
                if let Some(tail) = dst.last_chunk_mut::<$n>() {
                    *tail = *b;
                }
                return;
            }
        };
    }
    ends!(16);
    ends!(8);
    ends!(4);
    ends!(2);
    if let ([d], [s]) = (dst, src) {
        *d = *s;
    }
}

/// One byte as eight pixels, most significant bit first.
#[inline(always)]
fn bits8(x: u8) -> [u8; 8] {
    // Copy the byte into all eight bytes of a word, keep a different bit in
    // each, and turn "bit set" into 1.
    let t = (x as u64).wrapping_mul(0x0101_0101_0101_0101) & 0x0102_0408_1020_4080;
    (((t + 0x7f7f_7f7f_7f7f_7f7f) >> 7) & 0x0101_0101_0101_0101).to_le_bytes()
}

/// One byte as four 2-bit pixels.
#[inline(always)]
fn unpack2(x: u8) -> [u8; 4] {
    [x >> 6, (x >> 4) & 0x03, (x >> 2) & 0x03, x & 0x03]
}

/// One byte as two 4-bit pixels.
#[inline(always)]
fn unpack4(x: u8) -> [u8; 2] {
    [x >> 4, x & 0x0f]
}

// ---------------------------------------------------------------------
// 1 bit per pixel
// ---------------------------------------------------------------------

/// A run that is not wholly inside the visible row, or whose two bytes are
/// not both in the packet: the C code, step by step. Returns false when the
/// run does not fit the picture.
#[cold]
fn run1_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
    let pi0 = bits8(r.byte());
    let pi1 = bits8(r.byte());
    if !fits(pixel_ptr, n * 2 * 8, f.limit()) {
        return false;
    }
    for _ in 0..n {
        f.put(pixel_ptr, &pi0);
        f.put(pixel_ptr + 8, &pi1);
        pixel_ptr += 16;
    }
    true
}

/// The same for `n` bytes of eight pixels each.
#[cold]
fn copy1_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
    if !fits(pixel_ptr, n * 8, f.limit()) {
        return false;
    }
    for _ in 0..n {
        let x = r.byte();
        f.put(pixel_ptr, &bits8(x));
        pixel_ptr += 8;
    }
    true
}

#[inline(never)]
fn decode_1bpp(mut data: &[u8], f: &mut Frame<'_>, start_line: usize, lines: usize) -> Status {
    let row_inc = f.stride as i64;
    let limit = f.limit();
    // A skip byte with the top bit set means "next line", also for the first
    // line; so start one row up and count one more line.
    let mut row_ptr = (f.stride * start_line) as i64 - row_inc;
    let mut lines = lines as i64 + 1;
    // The position is `row_ptr + x`; `row` is the visible part of the row
    // at `row_ptr`, empty when that is not a row of the picture.
    let mut x = 0i64;
    let mut row = f.row_at(row_ptr);

    while lines != 0 {
        let skip = next_byte(&mut data) as i64;
        let rle_code = next_byte(&mut data) as i8 as i64;
        if rle_code == 0 {
            break;
        }
        if skip & 0x80 != 0 {
            lines -= 1;
            row_ptr += row_inc;
            x = 2 * 8 * (skip & 0x7f);
            row = f.row_at(row_ptr);
        } else {
            x += 2 * 8 * skip;
        }
        if !fits(row_ptr + x, 0, limit) {
            return Status::OutOfBounds;
        }

        if rle_code == -1 {
            continue;
        }

        if rle_code < 0 {
            // Two bytes are 16 pixels, repeated.
            let n = -rle_code;
            let end = x + n * 16;
            if let (Some(dst), Some((&[a, b], rest))) =
                (row.get_mut(x as usize..end as usize), data.split_first_chunk::<2>())
            {
                data = rest;
                let lo = u64::from_le_bytes(bits8(a)) as u128;
                let hi = u64::from_le_bytes(bits8(b)) as u128;
                let pat = (lo | hi << 64).to_le_bytes();
                for c in dst.as_chunks_mut::<16>().0 {
                    *c = pat;
                }
            } else {
                let mut r = Reader { data };
                if !run1_slow(&mut r, f, row_ptr + x, n) {
                    return Status::OutOfBounds;
                }
                data = r.data;
                row = f.row_at(row_ptr);
            }
            x = end;
        } else {
            // Bytes of 8 pixels each, two per code unit.
            let n = rle_code * 2;
            let end = x + n * 8;
            if let (Some(dst), Some((src, rest))) =
                (row.get_mut(x as usize..end as usize), data.split_at_checked(n as usize))
            {
                data = rest;
                for (d, &s) in dst.as_chunks_mut::<8>().0.iter_mut().zip(src) {
                    *d = bits8(s);
                }
            } else {
                let mut r = Reader { data };
                if !copy1_slow(&mut r, f, row_ptr + x, n) {
                    return Status::OutOfBounds;
                }
                data = r.data;
                row = f.row_at(row_ptr);
            }
            x = end;
        }
    }
    Status::Done
}

// ---------------------------------------------------------------------
// 2 to 32 bits per pixel
// ---------------------------------------------------------------------

/// What differs between the 2-, 4-, 8-, 16-, 24- and 32-bit decoders. The
/// stream counts in units: one pixel at 16 bits and up, four bytes of
/// stream below that.
trait Mode {
    /// Bytes of picture per unit.
    const OUT: usize;
    /// Bytes of stream per unit.
    const IN: usize;

    /// Fill `dst`, a whole number of units, with the unit at the start of
    /// `src`, which holds at least `IN` bytes: the rest of the packet.
    fn run(dst: &mut [u8], src: &[u8]);
    /// Decode the units in `src` to `dst`; both hold the same number.
    fn copy(dst: &mut [u8], src: &[u8]);

    /// A run of `n` units that is not wholly inside the visible row, or
    /// whose unit is not wholly in the packet: the C code, step by step.
    /// Returns false when the run does not fit the picture.
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool;
    /// The same for a copy of `n` units.
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool;
}

/// Write `n` copies of `unit` from `pixel_ptr` on, if they fit the picture.
fn put_run(f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64, unit: &[u8]) -> bool {
    let len = unit.len() as i64;
    if !fits(pixel_ptr, n * len, f.limit()) {
        return false;
    }
    for _ in 0..n {
        f.put(pixel_ptr, unit);
        pixel_ptr += len;
    }
    true
}

struct Pal2;
struct Pal4;
struct Pal8;
struct Rgb555;
struct Rgb24;
struct Argb;

impl Mode for Pal2 {
    const OUT: usize = 16;
    const IN: usize = 4;

    #[inline(always)]
    fn run(dst: &mut [u8], src: &[u8]) {
        let Some(&[a, b, c, d]) = src.first_chunk::<4>() else { return };
        // Built as one 128-bit value, so that it is stored with one write.
        let w = |x: u8| u32::from_le_bytes(unpack2(x)) as u128;
        let pat = (w(a) | w(b) << 32 | w(c) << 64 | w(d) << 96).to_le_bytes();
        for c in dst.as_chunks_mut::<16>().0 {
            *c = pat;
        }
    }

    #[inline(always)]
    fn copy(dst: &mut [u8], src: &[u8]) {
        for (d, &s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src) {
            *d = unpack2(s);
        }
    }

    #[cold]
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        let mut pat = [0u8; 16];
        for p in pat.as_chunks_mut::<4>().0 {
            *p = unpack2(r.byte());
        }
        put_run(f, pixel_ptr, n, &pat)
    }

    #[cold]
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
        if !fits(pixel_ptr, n * 16, f.limit()) {
            return false;
        }
        for _ in 0..n * 4 {
            f.put(pixel_ptr, &unpack2(r.byte()));
            pixel_ptr += 4;
        }
        true
    }
}

impl Mode for Pal4 {
    const OUT: usize = 8;
    const IN: usize = 4;

    #[inline(always)]
    fn run(dst: &mut [u8], src: &[u8]) {
        let Some(&[a, b, c, d]) = src.first_chunk::<4>() else { return };
        let w = |x: u8| u16::from_le_bytes(unpack4(x)) as u64;
        let pat = (w(a) | w(b) << 16 | w(c) << 32 | w(d) << 48).to_le_bytes();
        for c in dst.as_chunks_mut::<8>().0 {
            *c = pat;
        }
    }

    #[inline(always)]
    fn copy(dst: &mut [u8], src: &[u8]) {
        for (d, &s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src) {
            *d = unpack4(s);
        }
    }

    #[cold]
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        let mut pat = [0u8; 8];
        for p in pat.as_chunks_mut::<2>().0 {
            *p = unpack4(r.byte());
        }
        put_run(f, pixel_ptr, n, &pat)
    }

    #[cold]
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
        if !fits(pixel_ptr, n * 8, f.limit()) {
            return false;
        }
        for _ in 0..n * 4 {
            f.put(pixel_ptr, &unpack4(r.byte()));
            pixel_ptr += 2;
        }
        true
    }
}

impl Mode for Pal8 {
    const OUT: usize = 4;
    const IN: usize = 4;

    #[inline(always)]
    fn run(dst: &mut [u8], src: &[u8]) {
        let Some(&px) = src.first_chunk::<4>() else { return };
        for d in dst.as_chunks_mut::<4>().0 {
            *d = px;
        }
    }

    #[inline(always)]
    fn copy(dst: &mut [u8], src: &[u8]) {
        copy_bytes(dst, src);
    }

    #[cold]
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        let pi = [r.byte(), r.byte(), r.byte(), r.byte()];
        put_run(f, pixel_ptr, n, &pi)
    }

    #[cold]
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        if !fits(pixel_ptr, n * 4, f.limit()) {
            return false;
        }
        // A packet that ends early leaves the rest unwritten.
        let src = r.take_up_to(n as usize * 4);
        f.put(pixel_ptr, src);
        true
    }
}

impl Mode for Rgb555 {
    const OUT: usize = 2;
    const IN: usize = 2;

    #[inline(always)]
    fn run(dst: &mut [u8], src: &[u8]) {
        let Some(&[hi, lo]) = src.first_chunk::<2>() else { return };
        // Big-endian in the stream, native in the picture.
        let px = u16::from_be_bytes([hi, lo]).to_ne_bytes();
        for d in dst.as_chunks_mut::<2>().0 {
            *d = px;
        }
    }

    #[inline(always)]
    fn copy(dst: &mut [u8], src: &[u8]) {
        for (d, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src.as_chunks::<2>().0) {
            *d = u16::from_be_bytes(*s).to_ne_bytes();
        }
    }

    #[cold]
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        let rgb16 = r.be16().to_ne_bytes();
        put_run(f, pixel_ptr, n, &rgb16)
    }

    #[cold]
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
        if !fits(pixel_ptr, n * 2, f.limit()) {
            return false;
        }
        for _ in 0..n {
            let rgb16 = r.be16().to_ne_bytes();
            f.put(pixel_ptr, &rgb16);
            pixel_ptr += 2;
        }
        true
    }
}

impl Mode for Rgb24 {
    const OUT: usize = 3;
    const IN: usize = 3;

    #[inline(always)]
    fn run(dst: &mut [u8], src: &[u8]) {
        // The pixel is read with the byte after it as one 32-bit word when
        // the packet has one (it has, unless the run is its last code), and
        // written two pixels at a time as a 4-byte and a 2-byte store.
        let v = match (src.first_chunk::<4>(), src.first_chunk::<3>()) {
            (Some(four), _) => u32::from_le_bytes(*four) & 0x00ff_ffff,
            (None, Some(&[r, g, b])) => u32::from_le_bytes([r, g, b, 0]),
            _ => return,
        };
        let v = v as u64;
        let two = v | v << 24;
        let first = (two as u32).to_le_bytes();
        let second = ((two >> 32) as u16).to_le_bytes();
        let (pairs, rest) = dst.as_chunks_mut::<6>();
        for pair in pairs {
            let (head, tail) = pair.split_at_mut(4);
            head.copy_from_slice(&first);
            tail.copy_from_slice(&second);
        }
        if let Some(one) = rest.first_chunk_mut::<3>() {
            let (head, tail) = one.split_at_mut(2);
            head.copy_from_slice(&(v as u16).to_le_bytes());
            tail.copy_from_slice(&[(v >> 16) as u8]);
        }
    }

    #[inline(always)]
    fn copy(dst: &mut [u8], src: &[u8]) {
        copy_bytes(dst, src);
    }

    #[cold]
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        // Read as two bytes, then one.
        let rg: [u8; 2] = r.array();
        let b = r.byte();
        put_run(f, pixel_ptr, n, &[rg[0], rg[1], b])
    }

    #[cold]
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
        if !fits(pixel_ptr, n * 3, f.limit()) {
            return false;
        }
        // Two pixels at a time, read as four bytes and two.
        for _ in 0..n / 2 {
            let a: [u8; 4] = r.array();
            let b: [u8; 2] = r.array();
            f.put(pixel_ptr, &a);
            f.put(pixel_ptr + 4, &b);
            pixel_ptr += 6;
        }
        if n % 2 != 0 {
            let rg: [u8; 2] = r.array();
            let b = r.byte();
            f.put(pixel_ptr, &[rg[0], rg[1], b]);
        }
        true
    }
}

impl Mode for Argb {
    const OUT: usize = 4;
    const IN: usize = 4;

    #[inline(always)]
    fn run(dst: &mut [u8], src: &[u8]) {
        let Some(&px) = src.first_chunk::<4>() else { return };
        for d in dst.as_chunks_mut::<4>().0 {
            *d = px;
        }
    }

    #[inline(always)]
    fn copy(dst: &mut [u8], src: &[u8]) {
        copy_bytes(dst, src);
    }

    #[cold]
    fn run_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, pixel_ptr: i64, n: i64) -> bool {
        let argb: [u8; 4] = r.array();
        put_run(f, pixel_ptr, n, &argb)
    }

    #[cold]
    fn copy_slow(r: &mut Reader<'_>, f: &mut Frame<'_>, mut pixel_ptr: i64, n: i64) -> bool {
        if !fits(pixel_ptr, n * 4, f.limit()) {
            return false;
        }
        // Two pixels at a time, read as eight bytes.
        for _ in 0..n / 2 {
            let a: [u8; 8] = r.array();
            f.put(pixel_ptr, &a);
            pixel_ptr += 8;
        }
        if n % 2 != 0 {
            let a: [u8; 4] = r.array();
            f.put(pixel_ptr, &a);
        }
        true
    }
}

#[inline(never)]
fn decode_lines<M: Mode>(
    mut data: &[u8],
    f: &mut Frame<'_>,
    start_line: usize,
    lines: usize,
) -> Status {
    let (stride, row_len) = (f.stride, f.row_len);
    let limit = f.limit();
    let out = M::OUT as i64;
    let mut row_start = start_line * stride;

    for _ in 0..lines {
        // The position is `row_ptr + x`; `row` is the visible part of this
        // line. A code whose pixels all land in `row` and whose bytes are
        // all in the packet is done right here; any other goes the slow way.
        let row_ptr = row_start as i64;
        let mut x = out * (next_byte(&mut data) as i64 - 1);
        if !fits(row_ptr + x, 0, limit) {
            return Status::OutOfBounds;
        }
        // `region` made sure that every line is a row of the picture.
        let Some(mut row) = f.data.get_mut(row_start..row_start + row_len) else {
            return Status::Done;
        };

        loop {
            let rle_code = next_byte(&mut data) as i8 as i64;
            if rle_code == -1 {
                break;
            }
            // The C code stops here if the code was the last byte of the
            // packet. A code that finds its bytes in the packet was not the
            // last byte, so only the other cases need to ask.
            if rle_code < 0 {
                // One unit, repeated.
                let n = -rle_code;
                let end = x + n * out;
                if let (Some(dst), Some((_, rest))) =
                    (row.get_mut(x as usize..end as usize), data.split_at_checked(M::IN))
                {
                    M::run(dst, data);
                    data = rest;
                } else {
                    if data.is_empty() {
                        return Status::Done;
                    }
                    let mut r = Reader { data };
                    if !M::run_slow(&mut r, f, row_ptr + x, n) {
                        return Status::OutOfBounds;
                    }
                    data = r.data;
                    row = f.row_at(row_ptr);
                }
                x = end;
            } else if rle_code > 0 {
                // Units from the stream.
                let n = rle_code;
                let end = x + n * out;
                if let (Some(dst), Some((src, rest))) = (
                    row.get_mut(x as usize..end as usize),
                    data.split_at_checked(n as usize * M::IN),
                ) {
                    data = rest;
                    M::copy(dst, src);
                } else {
                    if data.is_empty() {
                        return Status::Done;
                    }
                    let mut r = Reader { data };
                    if !M::copy_slow(&mut r, f, row_ptr + x, n) {
                        return Status::OutOfBounds;
                    }
                    data = r.data;
                    row = f.row_at(row_ptr);
                }
                x = end;
            } else {
                if data.is_empty() {
                    return Status::Done;
                }
                // Another skip code.
                x += out * (next_byte(&mut data) as i64 - 1);
                if !fits(row_ptr + x, 0, limit) {
                    return Status::OutOfBounds;
                }
            }
        }
        row_start += stride;
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
    let r = packet.get(body..).unwrap_or_default();
    match bits {
        1 | 33 => decode_1bpp(r, frame, start_line, lines),
        2 | 34 => decode_lines::<Pal2>(r, frame, start_line, lines),
        4 | 36 => decode_lines::<Pal4>(r, frame, start_line, lines),
        8 | 40 => decode_lines::<Pal8>(r, frame, start_line, lines),
        16 => decode_lines::<Rgb555>(r, frame, start_line, lines),
        24 => decode_lines::<Rgb24>(r, frame, start_line, lines),
        32 => decode_lines::<Argb>(r, frame, start_line, lines),
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
