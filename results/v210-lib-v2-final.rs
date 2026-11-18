//! Safe Rust port of FFmpeg's v210 decoder (`v210dec.c`): uncompressed
//! 10-bit 4:2:2 video, three samples packed into each little-endian 32-bit
//! word, unpacked to planar 16-bit samples.
//!
//! FFmpeg unpacks rows with hand-written SSSE3/AVX/AVX2/AVX-512 assembly.
//! This crate has no assembly and no `unsafe`. It has a portable decoder
//! and, on x86-64, a second one written with AVX2 intrinsics. The
//! intrinsics used here take and return values, not pointers, so Rust
//! lets safe code call them inside a function that is compiled for AVX2.
//! The one thing safe code cannot do is promise that the CPU has AVX2:
//! calling [`decode_avx2`] from ordinary code needs an `unsafe` block, and
//! that single call lives in the FFI crate, behind a CPU check.

#![forbid(unsafe_code)]
#![no_std]

/// `MKTAG('b', 'x', 'y', '2')`: rows are not padded at all.
pub const TAG_BXY2: u32 = u32::from_le_bytes(*b"bxy2");
/// `MKTAG('C', '2', '1', '0')`: may carry a 64-byte "INFO" header.
pub const TAG_C210: u32 = u32::from_le_bytes(*b"C210");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Unpadded rows with an odd width or an oversized picture.
    StridelessUnsupported,
    PacketTooSmall,
}

/// Where the rows are in a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Bytes from one row to the next; 0 if rows are not padded.
    pub stride: usize,
    /// Bytes to skip at the start of the packet.
    pub offset: usize,
    /// Set when the stride was guessed from the packet size because the
    /// rows have less padding than the format requires; the value is the
    /// padding unit in bytes. The caller warns about it.
    pub small_padding: Option<usize>,
}

/// Bytes per row when the width is padded to a multiple of `align` pixels.
fn v210_stride(width: usize, align: usize) -> usize {
    width.div_ceil(align) * align * 8 / 3
}

/// Work out the row layout the way the C decoder does.
///
/// `custom_stride` is the decoder option of that name: 0 for automatic,
/// negative for unpadded rows.
pub fn layout(
    packet: &[u8],
    width: usize,
    height: usize,
    codec_tag: u32,
    custom_stride: i32,
) -> Result<Layout, Error> {
    let size = packet.len();
    let mut small_padding = None;
    let mut stride;

    if custom_stride != 0 {
        stride = usize::try_from(custom_stride).unwrap_or(0);
    } else {
        stride = v210_stride(width, 48);
        if size < stride.saturating_mul(height) {
            let found = [24, 12, 6]
                .into_iter()
                .find(|&align| size == v210_stride(width, align) * height);
            match found {
                Some(align) => {
                    stride = v210_stride(width, align);
                    small_padding = Some(align * 8 / 3);
                }
                None if codec_tag == TAG_BXY2 => stride = 0,
                None => {}
            }
        }
    }

    let pixels = width.saturating_mul(height);
    if stride == 0 && (width & 1 != 0 || pixels > i32::MAX as usize / 6) {
        return Err(Error::StridelessUnsupported);
    }
    let need = if stride > 0 { stride.saturating_mul(height) } else { v210_stride(pixels, 6) };
    if size < need {
        return Err(Error::PacketTooSmall);
    }

    let mut offset = 0;
    if codec_tag == TAG_C210
        && size > 64
        && packet.starts_with(b"INFO")
        && size - 64 >= stride.saturating_mul(height)
    {
        offset = 64;
    }
    Ok(Layout { stride, offset, small_padding })
}

/// The three planes of the visible picture. Strides are in samples.
pub struct Planes<'a> {
    y: &'a mut [u16],
    u: &'a mut [u16],
    v: &'a mut [u16],
    y_stride: usize,
    c_stride: usize,
    width: usize,
    height: usize,
}

impl<'a> Planes<'a> {
    /// Each plane must hold `stride * (height - 1)` samples plus one
    /// visible row: `width` for luma, `(width + 1) / 2` for chroma.
    pub fn new(
        y: &'a mut [u16],
        u: &'a mut [u16],
        v: &'a mut [u16],
        y_stride: usize,
        c_stride: usize,
        width: usize,
        height: usize,
    ) -> Option<Self> {
        let cw = width.div_ceil(2);
        if width == 0 || height == 0 || y_stride < width || c_stride < cw {
            return None;
        }
        let y_need = y_stride.checked_mul(height - 1)?.checked_add(width)?;
        let c_need = c_stride.checked_mul(height - 1)?.checked_add(cw)?;
        Some(Planes {
            y: y.get_mut(..y_need)?,
            u: u.get_mut(..c_need)?,
            v: v.get_mut(..c_need)?,
            y_stride,
            c_stride,
            width,
            height,
        })
    }
}

/// Little-endian word `i` of `src`; zero past the end.
fn word(src: &[u8], i: usize) -> u32 {
    match src.get(4 * i..).and_then(|s| s.first_chunk::<4>()) {
        Some(b) => u32::from_le_bytes(*b),
        None => 0,
    }
}

fn s0(w: u32) -> u16 {
    (w & 0x3ff) as u16
}
fn s1(w: u32) -> u16 {
    (w >> 10 & 0x3ff) as u16
}
fn s2(w: u32) -> u16 {
    (w >> 20 & 0x3ff) as u16
}

/// Unpack one row: `y.len()` pixels, `u.len() == v.len() == (y.len() + 1) / 2`.
fn decode_row(src: &[u8], y: &mut [u16], u: &mut [u16], v: &mut [u16]) {
    // Whole groups: 16 bytes hold 6 pixels.
    let groups = y.len() / 6;
    let (src6, _) = src.as_chunks::<16>();
    let (y6, y_tail) = y.as_chunks_mut::<6>();
    // Split the chroma where the luma splits: a tail of five pixels has
    // three chroma samples, which must not be taken for a whole group.
    let (u6, u_tail) = u.split_at_mut((3 * groups).min(u.len()));
    let (v6, v_tail) = v.split_at_mut((3 * groups).min(v.len()));
    let (u6, v6) = (u6.as_chunks_mut::<3>().0, v6.as_chunks_mut::<3>().0);
    for (((s, y), u), v) in src6.iter().zip(y6).zip(u6).zip(v6) {
        let w0 = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let w1 = u32::from_le_bytes([s[4], s[5], s[6], s[7]]);
        let w2 = u32::from_le_bytes([s[8], s[9], s[10], s[11]]);
        let w3 = u32::from_le_bytes([s[12], s[13], s[14], s[15]]);
        *y = [s1(w0), s0(w1), s2(w1), s1(w2), s0(w3), s2(w3)];
        *u = [s0(w0), s1(w1), s2(w2)];
        *v = [s2(w0), s0(w2), s1(w3)];
    }

    // The last one to five pixels.
    let rem = y_tail.len();
    if rem == 0 {
        return;
    }
    let tail = src.get(16 * groups..).unwrap_or(&[]);
    let (w0, w1, w2, w3) = (word(tail, 0), word(tail, 1), word(tail, 2), word(tail, 3));
    let ys = [s1(w0), s0(w1), s2(w1), s1(w2), s0(w3)];
    let us = [s0(w0), s1(w1), s2(w2)];
    let vs = [s2(w0), s0(w2), s1(w3)];
    y_tail.copy_from_slice(&ys[..rem]);
    // The chroma tails are (rem + 1) / 2 long when the row length is right.
    for (d, s) in u_tail.iter_mut().zip(us) {
        *d = s;
    }
    for (d, s) in v_tail.iter_mut().zip(vs) {
        *d = s;
    }
}

/// Twelve pixels from 32 bytes with AVX2.
///
/// Every 10-bit sample lies inside two neighbouring bytes, 0, 2 or 4 bits
/// up. A byte shuffle moves each sample's two bytes into a 16-bit lane, a
/// multiply by 64, 16 or 4 pushes the sample to the top of the lane (and
/// its neighbours' bits out), and a shift right by 6 brings it down again.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn unpack12(s: &[u8; 32], y: &mut [u16; 12], u: &mut [u16; 6], v: &mut [u16; 6]) {
    use core::arch::x86_64::*;

    let q = |i: usize| i64::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3], s[i + 4], s[i + 5], s[i + 6], s[i + 7]]);
    let x = _mm256_set_epi64x(q(24), q(16), q(8), q(0));

    // Per 16-byte half: y0..y5 in lanes 0..5.
    let luma_bytes = _mm256_setr_epi8(
        1, 2, 4, 5, 6, 7, 9, 10, 12, 13, 14, 15, -1, -1, -1, -1,
        1, 2, 4, 5, 6, 7, 9, 10, 12, 13, 14, 15, -1, -1, -1, -1,
    );
    let luma_mul = _mm256_setr_epi16(16, 64, 4, 16, 64, 4, 0, 0, 16, 64, 4, 16, 64, 4, 0, 0);
    let luma = _mm256_srli_epi16(_mm256_mullo_epi16(_mm256_shuffle_epi8(x, luma_bytes), luma_mul), 6);
    // Close the gap between the two halves: twelve samples in a row.
    let luma = _mm256_permutevar8x32_epi32(luma, _mm256_setr_epi32(0, 1, 2, 4, 5, 6, 3, 7));

    // Per 16-byte half: u0..u2 in lanes 0..2, v0..v2 in lanes 4..6.
    let chroma_bytes = _mm256_setr_epi8(
        0, 1, 5, 6, 10, 11, -1, -1, 2, 3, 8, 9, 13, 14, -1, -1,
        0, 1, 5, 6, 10, 11, -1, -1, 2, 3, 8, 9, 13, 14, -1, -1,
    );
    let chroma_mul = _mm256_setr_epi16(64, 16, 4, 0, 4, 64, 16, 0, 64, 16, 4, 0, 4, 64, 16, 0);
    let chroma = _mm256_srli_epi16(_mm256_mullo_epi16(_mm256_shuffle_epi8(x, chroma_bytes), chroma_mul), 6);

    let four = |q: u64| [q as u16, (q >> 16) as u16, (q >> 32) as u16, (q >> 48) as u16];
    let luma = [
        _mm256_extract_epi64::<0>(luma) as u64,
        _mm256_extract_epi64::<1>(luma) as u64,
        _mm256_extract_epi64::<2>(luma) as u64,
    ];
    for (dst, q) in y.as_chunks_mut::<4>().0.iter_mut().zip(luma) {
        *dst = four(q);
    }
    // Three samples from each half, joined into six.
    let six = |a: u64, b: u64| {
        let lo = four(a & 0xffff_ffff_ffff | b << 48);
        [lo[0], lo[1], lo[2], lo[3], (b >> 16) as u16, (b >> 32) as u16]
    };
    *u = six(_mm256_extract_epi64::<0>(chroma) as u64, _mm256_extract_epi64::<2>(chroma) as u64);
    *v = six(_mm256_extract_epi64::<1>(chroma) as u64, _mm256_extract_epi64::<3>(chroma) as u64);
}

/// [`decode_row`], twelve pixels at a time where the row allows.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn decode_row_avx2(src: &[u8], y: &mut [u16], u: &mut [u16], v: &mut [u16]) {
    let n = (y.len() / 12).min(src.len() / 32);
    let (y12, y_rest) = y.split_at_mut(12 * n);
    let (u6, u_rest) = u.split_at_mut((6 * n).min(u.len()));
    let (v6, v_rest) = v.split_at_mut((6 * n).min(v.len()));
    let blocks = src
        .as_chunks::<32>()
        .0
        .iter()
        .zip(y12.as_chunks_mut::<12>().0)
        .zip(u6.as_chunks_mut::<6>().0)
        .zip(v6.as_chunks_mut::<6>().0);
    for (((s, y), u), v) in blocks {
        unpack12(s, y, u, v);
    }
    decode_row(src.get(32 * n..).unwrap_or(&[]), y_rest, u_rest, v_rest);
}

/// Decode a packet into `planes` using a layout from [`layout`].
pub fn decode(packet: &[u8], layout: &Layout, planes: &mut Planes<'_>) {
    decode_rows(packet, layout, planes, decode_row);
}

/// [`decode`] for CPUs with AVX2. Same output.
///
/// Safe code can only call this from another function compiled for AVX2;
/// anyone else has to check the CPU and use `unsafe`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub fn decode_avx2(packet: &[u8], layout: &Layout, planes: &mut Planes<'_>) {
    decode_rows(packet, layout, planes, |s, y, u, v| decode_row_avx2(s, y, u, v));
}

#[inline(always)]
fn decode_rows(
    packet: &[u8],
    layout: &Layout,
    planes: &mut Planes<'_>,
    row_fn: impl Fn(&[u8], &mut [u16], &mut [u16], &mut [u16]),
) {
    let (width, height) = (planes.width, planes.height);
    let cw = width.div_ceil(2);
    let src = packet.get(layout.offset..).unwrap_or(&[]);

    if layout.stride > 0 {
        for row in 0..height {
            let s = src.get(row * layout.stride..).unwrap_or(&[]);
            row_fn(
                s,
                &mut planes.y[row * planes.y_stride..][..width],
                &mut planes.u[row * planes.c_stride..][..cw],
                &mut planes.v[row * planes.c_stride..][..cw],
            );
        }
        return;
    }

    // Unpadded rows: the picture is one long row, so a 6-pixel group can
    // straddle two rows. Rare; unpack sample by sample.
    for row in 0..height {
        let first = row * width;
        for (x, d) in planes.y[row * planes.y_stride..][..width].iter_mut().enumerate() {
            let p = first + x;
            let g = 4 * (p / 6);
            *d = match p % 6 {
                0 => s1(word(src, g)),
                1 => s0(word(src, g + 1)),
                2 => s2(word(src, g + 1)),
                3 => s1(word(src, g + 2)),
                4 => s0(word(src, g + 3)),
                _ => s2(word(src, g + 3)),
            };
        }
        // `width` is even here, so every row starts on a chroma sample.
        let first = first / 2;
        for x in 0..cw {
            let c = first + x;
            let g = 4 * (c / 3);
            let (u, v) = match c % 3 {
                0 => (s0(word(src, g)), s2(word(src, g))),
                1 => (s1(word(src, g + 1)), s0(word(src, g + 2))),
                _ => (s2(word(src, g + 2)), s1(word(src, g + 3))),
            };
            planes.u[row * planes.c_stride + x] = u;
            planes.v[row * planes.c_stride + x] = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(a: u16, b: u16, c: u16) -> [u8; 4] {
        (u32::from(a) | u32::from(b) << 10 | u32::from(c) << 20).to_le_bytes()
    }

    #[test]
    fn one_group() {
        let mut src = [0u8; 16];
        src[0..4].copy_from_slice(&pack(100, 1, 200));
        src[4..8].copy_from_slice(&pack(2, 101, 3));
        src[8..12].copy_from_slice(&pack(201, 4, 102));
        src[12..16].copy_from_slice(&pack(5, 202, 6));
        let (mut y, mut u, mut v) = ([0u16; 6], [0u16; 3], [0u16; 3]);
        decode_row(&src, &mut y, &mut u, &mut v);
        assert_eq!(y, [1, 2, 3, 4, 5, 6]);
        assert_eq!(u, [100, 101, 102]);
        assert_eq!(v, [200, 201, 202]);
    }

    #[test]
    fn tail_of_five() {
        let mut src = [0u8; 16];
        src[0..4].copy_from_slice(&pack(100, 1, 200));
        src[4..8].copy_from_slice(&pack(2, 101, 3));
        src[8..12].copy_from_slice(&pack(201, 4, 102));
        src[12..16].copy_from_slice(&pack(5, 202, 6));
        let (mut y, mut u, mut v) = ([0u16; 5], [0u16; 3], [0u16; 3]);
        decode_row(&src, &mut y, &mut u, &mut v);
        assert_eq!(y, [1, 2, 3, 4, 5]);
        assert_eq!(u, [100, 101, 102]);
        assert_eq!(v, [200, 201, 202]);
    }

    #[test]
    fn strides() {
        assert_eq!(v210_stride(1280, 48), 3456);
        assert_eq!(v210_stride(1, 48), 128);
        let pkt = [0u8; 128];
        assert_eq!(layout(&pkt, 1, 1, 0, 0), Ok(Layout { stride: 128, offset: 0, small_padding: None }));
        assert_eq!(layout(&pkt[..16], 1, 1, 0, 0), Ok(Layout { stride: 16, offset: 0, small_padding: Some(16) }));
        assert_eq!(layout(&pkt[..15], 1, 1, 0, 0), Err(Error::PacketTooSmall));
    }
}
