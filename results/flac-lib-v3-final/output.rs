//! From decoded channels to output samples: ports of the
//! `flac_decorrelate_*` functions of flacdsp_template.c (and of the x86
//! versions in flacdsp.asm where they differ) and of `decorrelate_33bps`.
//!
//! The loops are written over zipped slices and fixed-size chunks, without
//! indexing, so that the compiler turns them into vector code (FFmpeg has
//! assembly for the interleaved formats).

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
#[inline(always)]
pub fn decorrelate_33bps(ch_mode: u8, ch0: &mut [i32], ch1: &mut [i32], side: &[i64]) {
    let both = ch0.iter_mut().zip(ch1.iter_mut()).zip(side);
    match ch_mode {
        1 => {
            for ((l, r), &s) in both {
                *r = i64::from(*l).wrapping_sub(s) as i32;
            }
        }
        2 => {
            for ((l, r), &s) in both {
                *l = i64::from(*r).wrapping_add(s) as i32;
            }
        }
        _ => {
            for ((l, r), &b) in both {
                let a = i64::from(*l).wrapping_sub(b >> 1);
                *l = a.wrapping_add(b) as i32;
                *r = a as i32;
            }
        }
    }
}

// Left and right from the two coded channels, before the shift.
#[inline(always)]
fn left_side(a: i32, b: i32) -> [i32; 2] {
    [a, a.wrapping_sub(b)]
}

#[inline(always)]
fn side_right(a: i32, b: i32) -> [i32; 2] {
    [a.wrapping_add(b), b]
}

#[inline(always)]
fn mid_side(a: i32, b: i32) -> [i32; 2] {
    let a = a.wrapping_sub(b >> 1);
    [a.wrapping_add(b), a]
}

#[inline(always)]
fn independent(a: i32, b: i32) -> [i32; 2] {
    [a, b]
}

/// A sample for 16-bit output: C shifts in 32 bits and keeps the low half.
#[inline(always)]
fn narrow_c(v: i32, shift: u32) -> i16 {
    ((v as u32) << shift) as i16
}

/// The same in the assembly: saturate to 16 bits, then shift within 16
/// bits (which is what the low half of a 32-bit shift holds).
#[inline(always)]
fn narrow_asm(v: i32, shift: u32) -> i16 {
    let s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
    ((s as u32) << shift) as i16
}

#[inline(always)]
fn wide(v: i32, shift: u32) -> i32 {
    ((v as u32) << shift) as i32
}

/// Two channels to interleaved output.
#[inline(always)]
fn pair<T: Copy>(
    out: &mut [T],
    a: &[i32],
    b: &[i32],
    mix: impl Fn(i32, i32) -> [i32; 2],
    conv: impl Fn(i32) -> T,
) {
    for ((o, &a), &b) in out.as_chunks_mut::<2>().0.iter_mut().zip(a).zip(b) {
        let [l, r] = mix(a, b);
        *o = [conv(l), conv(r)];
    }
}

/// Two channels to two planes.
#[inline(always)]
fn pair_planar<T: Copy>(
    out0: &mut [T],
    out1: &mut [T],
    a: &[i32],
    b: &[i32],
    mix: impl Fn(i32, i32) -> [i32; 2],
    conv: impl Fn(i32) -> T,
) {
    for (((o0, o1), &a), &b) in out0.iter_mut().zip(out1.iter_mut()).zip(a).zip(b) {
        let [l, r] = mix(a, b);
        *o0 = conv(l);
        *o1 = conv(r);
    }
}

/// One channel to one plane (or to mono output).
#[inline(always)]
fn plane<T: Copy>(out: &mut [T], a: &[i32], conv: impl Fn(i32) -> T) {
    for (o, &a) in out.iter_mut().zip(a) {
        *o = conv(a);
    }
}

/// `N` independent channels to interleaved output.
#[inline(always)]
fn interleave<T: Copy, const N: usize>(out: &mut [T], channels: &[&[i32]], conv: impl Fn(i32) -> T) {
    let Ok(channels) = <&[&[i32]; N]>::try_from(channels) else {
        return;
    };
    let (frames, _) = out.as_chunks_mut::<N>();
    let n = frames.len();
    let channels = (*channels).map(|c| c.get(..n).unwrap_or(&[]));
    if channels.iter().any(|c| c.len() != n) {
        return;
    }
    for (i, frame) in frames.iter_mut().enumerate() {
        for c in 0..N {
            frame[c] = conv(channels[c][i]);
        }
    }
}

/// Independent channels, any number, to interleaved output.
#[inline(always)]
fn interleave_any<T: Copy>(out: &mut [T], channels: &[&[i32]], conv: impl Fn(i32) -> T + Copy) {
    match channels.len() {
        1 => plane(out, channels[0], conv),
        2 => pair(out, channels[0], channels[1], independent, conv),
        3 => interleave::<T, 3>(out, channels, conv),
        4 => interleave::<T, 4>(out, channels, conv),
        5 => interleave::<T, 5>(out, channels, conv),
        6 => interleave::<T, 6>(out, channels, conv),
        7 => interleave::<T, 7>(out, channels, conv),
        8 => interleave::<T, 8>(out, channels, conv),
        _ => {}
    }
}

/// Runs `$body` with `$mix` bound to the function for stereo mode `$mode`,
/// so that each mode gets its own loop.
macro_rules! per_stereo_mode {
    ($mode:expr, $mix:ident => $body:expr) => {
        match $mode {
            1 => {
                let $mix = left_side;
                $body
            }
            2 => {
                let $mix = side_right;
                $body
            }
            _ => {
                let $mix = mid_side;
                $body
            }
        }
    };
}

/// Write one frame. `channels` are the decoded channels, each one block
/// long; `ch_mode` is 0 or, for two channels, the stereo decorrelation.
/// Returns `None` if the output does not have the frame's shape.
#[inline(always)]
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
                if quirks.saturate_stereo {
                    per_stereo_mode!(ch_mode, mix => pair(out, a, b, mix, |v| narrow_asm(v, shift)));
                } else {
                    per_stereo_mode!(ch_mode, mix => pair(out, a, b, mix, |v| narrow_c(v, shift)));
                }
            } else {
                let saturate = match nch {
                    4 | 6 => quirks.saturate_multi,
                    8 => quirks.saturate_8,
                    _ => false,
                };
                if saturate {
                    interleave_any(out, channels, |v| narrow_asm(v, shift));
                } else {
                    interleave_any(out, channels, |v| narrow_c(v, shift));
                }
            }
        }
        Output::S32(out) => {
            if out.len() != n * nch {
                return None;
            }
            if ch_mode != 0 {
                let (a, b) = (channels[0], channels[1]);
                per_stereo_mode!(ch_mode, mix => pair(out, a, b, mix, |v| wide(v, shift)));
            } else {
                interleave_any(out, channels, |v| wide(v, shift));
            }
        }
        Output::S16P(out) => {
            if out.len() != nch || out.iter().any(|p| p.len() != n) {
                return None;
            }
            if ch_mode != 0 {
                let (a, b) = (channels[0], channels[1]);
                let (o0, o1) = out.split_at_mut(1);
                let (o0, o1) = (&mut *o0[0], &mut *o1[0]);
                per_stereo_mode!(ch_mode, mix => pair_planar(o0, o1, a, b, mix, |v| narrow_c(v, shift)));
            } else {
                for (o, c) in out.iter_mut().zip(channels) {
                    plane(o, c, |v| narrow_c(v, shift));
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
                let (o0, o1) = (&mut *o0[0], &mut *o1[0]);
                per_stereo_mode!(ch_mode, mix => pair_planar(o0, o1, a, b, mix, |v| wide(v, shift)));
            } else {
                for (o, c) in out.iter_mut().zip(channels) {
                    plane(o, c, |v| wide(v, shift));
                }
            }
        }
    }
    Some(())
}
