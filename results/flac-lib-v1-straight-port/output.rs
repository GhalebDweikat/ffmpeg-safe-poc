//! From decoded channels to output samples: ports of the
//! `flac_decorrelate_*` functions of flacdsp_template.c (and of the x86
//! versions in flacdsp.asm where they differ) and of `decorrelate_33bps`.

/// Where FFmpeg's x86 assembly gives different samples than its C code.
///
/// For interleaved 16-bit output the assembly narrows each sample with a
/// saturating pack (`packssdw`) and shifts afterwards, where the C code
/// shifts and truncates. A valid stream never has a sample outside 16
/// bits, a damaged one can. Which of the two FFmpeg runs depends on the
/// CPU and on how it was built, so the caller says.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Quirks {
    /// Left/side, side/right and mid/side frames (FFmpeg: SSE2).
    pub saturate_stereo: bool,
    /// Independent frames of 4 or 6 channels (FFmpeg: SSSE3).
    pub saturate_multi: bool,
    /// Independent frames of 8 channels (FFmpeg: SSSE3 or AVX, 64-bit only).
    pub saturate_8: bool,
}

/// The output samples of one frame, in one of the decoder's four formats.
/// Interleaved buffers hold `samples * channels` values; planar ones a
/// slice of `samples` values per channel.
pub enum Output<'a, 'b> {
    S16(&'a mut [i16]),
    S32(&'a mut [i32]),
    S16P(&'a mut [&'b mut [i16]]),
    S32P(&'a mut [&'b mut [i32]]),
}

/// `decorrelate_33bps`: a 32-bit stereo frame with a side channel. The
/// side channel has 33 bits and is in `side`; this puts left and right
/// into the two 32-bit channels.
pub fn decorrelate_33bps(ch_mode: u8, ch0: &mut [i32], ch1: &mut [i32], side: &[i64]) {
    let n = side.len();
    let (ch0, ch1) = (&mut ch0[..n], &mut ch1[..n]);
    match ch_mode {
        1 => {
            for i in 0..n {
                ch1[i] = i64::from(ch0[i]).wrapping_sub(side[i]) as i32;
            }
        }
        2 => {
            for i in 0..n {
                ch0[i] = i64::from(ch1[i]).wrapping_add(side[i]) as i32;
            }
        }
        _ => {
            for i in 0..n {
                let b = side[i];
                let a = i64::from(ch0[i]).wrapping_sub(b >> 1);
                ch0[i] = a.wrapping_add(b) as i32;
                ch1[i] = a as i32;
            }
        }
    }
}

/// Left and right of sample `i` of a stereo frame, before the shift.
#[inline(always)]
fn stereo(ch_mode: u8, a: i32, b: i32) -> (i32, i32) {
    match ch_mode {
        1 => (a, a.wrapping_sub(b)),
        2 => (a.wrapping_add(b), b),
        _ => {
            let a = a.wrapping_sub(b >> 1);
            (a.wrapping_add(b), a)
        }
    }
}

/// A sample for 16-bit output: C shifts in 32 bits and keeps the low half.
#[inline(always)]
fn narrow_c(v: i32, shift: u32) -> i16 {
    ((v as u32) << shift) as i16
}

/// The same in the assembly: saturate to 16 bits, then shift.
#[inline(always)]
fn narrow_asm(v: i32, shift: u32) -> i16 {
    let s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as u16;
    (if shift < 16 { s << shift } else { 0 }) as i16
}

#[inline(always)]
fn wide(v: i32, shift: u32) -> i32 {
    ((v as u32) << shift) as i32
}

/// Write one frame. `channels` are the decoded channels, each one block
/// long; `ch_mode` is 0 or, for two channels, the stereo decorrelation.
/// Returns `None` if the output does not have the frame's shape.
pub fn write(
    channels: &[&[i32]],
    ch_mode: u8,
    shift: u32,
    quirks: Quirks,
    out: Output,
) -> Option<()> {
    let nch = channels.len();
    let n = channels.first()?.len();
    if channels.iter().any(|c| c.len() != n) || (ch_mode != 0 && nch != 2) || shift > 31 {
        return None;
    }
    match out {
        Output::S16(out) => {
            if out.len() != n * nch {
                return None;
            }
            if ch_mode != 0 {
                let (a, b) = (channels[0], channels[1]);
                for (i, o) in out.chunks_exact_mut(2).enumerate() {
                    let (l, r) = stereo(ch_mode, a[i], b[i]);
                    if quirks.saturate_stereo {
                        o[0] = narrow_asm(l, shift);
                        o[1] = narrow_asm(r, shift);
                    } else {
                        o[0] = narrow_c(l, shift);
                        o[1] = narrow_c(r, shift);
                    }
                }
            } else {
                let saturate = match nch {
                    4 | 6 => quirks.saturate_multi,
                    8 => quirks.saturate_8,
                    _ => false,
                };
                for (i, o) in out.chunks_exact_mut(nch).enumerate() {
                    for (o, c) in o.iter_mut().zip(channels) {
                        *o = if saturate { narrow_asm(c[i], shift) } else { narrow_c(c[i], shift) };
                    }
                }
            }
        }
        Output::S32(out) => {
            if out.len() != n * nch {
                return None;
            }
            if ch_mode != 0 {
                let (a, b) = (channels[0], channels[1]);
                for (i, o) in out.chunks_exact_mut(2).enumerate() {
                    let (l, r) = stereo(ch_mode, a[i], b[i]);
                    o[0] = wide(l, shift);
                    o[1] = wide(r, shift);
                }
            } else {
                for (i, o) in out.chunks_exact_mut(nch).enumerate() {
                    for (o, c) in o.iter_mut().zip(channels) {
                        *o = wide(c[i], shift);
                    }
                }
            }
        }
        Output::S16P(out) => {
            if out.len() != nch || out.iter().any(|p| p.len() != n) {
                return None;
            }
            if ch_mode != 0 {
                let (a, b) = (channels[0], channels[1]);
                let (o0, o1) = out.split_at_mut(1);
                for i in 0..n {
                    let (l, r) = stereo(ch_mode, a[i], b[i]);
                    o0[0][i] = narrow_c(l, shift);
                    o1[0][i] = narrow_c(r, shift);
                }
            } else {
                for (o, c) in out.iter_mut().zip(channels) {
                    for (o, &c) in o.iter_mut().zip(c.iter()) {
                        *o = narrow_c(c, shift);
                    }
                }
            }
        }
        Output::S32P(out) => {
            if out.len() != nch || out.iter().any(|p| p.len() != n) {
                return None;
            }
            if ch_mode != 0 {
                let (a, b) = (channels[0], channels[1]);
                let (o0, o1) = out.split_at_mut(1);
                for i in 0..n {
                    let (l, r) = stereo(ch_mode, a[i], b[i]);
                    o0[0][i] = wide(l, shift);
                    o1[0][i] = wide(r, shift);
                }
            } else {
                for (o, c) in out.iter_mut().zip(channels) {
                    for (o, &c) in o.iter_mut().zip(c.iter()) {
                        *o = wide(c, shift);
                    }
                }
            }
        }
    }
    Some(())
}
