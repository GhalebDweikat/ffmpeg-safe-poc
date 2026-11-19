//! C entry points for the safe FLAC decoder.
//!
//! The decoder keeps the stream parameters and the decoded channels from
//! one call to the next, so it lives in a heap allocation that C holds as
//! an opaque pointer. A packet is decoded in two calls, because FFmpeg
//! allocates the output buffer in between: `flac_rs_decode` parses the
//! packet and says what frame it holds, `flac_rs_output` writes the
//! samples.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use flac_core::{Decoder, Options, Outcome, Output, Quirks, Request, SampleFmt, Updates, MAX_CHANNELS};

pub const FLAC_RS_BAD_ARGS: i32 = -1;

/// `flac_rs_decode` results.
pub const FLAC_RS_FRAME: i32 = 0;
pub const FLAC_RS_SKIP: i32 = 1;
pub const FLAC_RS_INVALID: i32 = 2;

/// Option bits for `flac_rs_new` and `flac_rs_decode`.
pub const FLAC_RS_WANT32: u32 = 1;
pub const FLAC_RS_PLANAR: u32 = 2;
pub const FLAC_RS_CHECK_CRC: u32 = 4;
pub const FLAC_RS_BUGGY_LPC: u32 = 8;

/// Quirk bits for `flac_rs_output`.
pub const FLAC_RS_SATURATE_STEREO: u32 = 1;
pub const FLAC_RS_SATURATE_MULTI: u32 = 2;
pub const FLAC_RS_SATURATE_8: u32 = 4;

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: turn the AVX2 code paths off (0) or back on, so that the
/// portable decoder can be exercised on a CPU that has AVX2.
#[no_mangle]
pub extern "C" fn flac_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

/// Whether to take the AVX2 code paths: the CPU has what they were
/// compiled for, and the test hook has not turned them off.
fn simd() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        USE_SIMD.load(Ordering::Relaxed)
            && std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2")
            && std::is_x86_feature_detected!("lzcnt")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Codec context fields the decoder changed. A field keeps its "none"
/// value when it is to be left alone.
#[repr(C)]
pub struct FlacRsUpdates {
    /// Channel count to set the channel layout for; 0 for none.
    pub channels: i32,
    /// -1 for none.
    pub sample_rate: i32,
    /// -1 for none.
    pub bits_per_raw_sample: i32,
    /// 0 S16, 1 S32, 2 S16P, 3 S32P; -1 for none.
    pub sample_fmt: i32,
}

#[repr(C)]
pub struct FlacRsFrame {
    pub updates: FlacRsUpdates,
    /// Bytes of the packet used, for `FLAC_RS_SKIP` and `FLAC_RS_FRAME`.
    /// For a frame it can be one more than the packet has.
    pub consumed: i32,
    pub nb_samples: i32,
    pub channels: i32,
    pub crc_error: i32,
}

fn updates(u: Updates) -> FlacRsUpdates {
    FlacRsUpdates {
        channels: u.channels.unwrap_or(0),
        sample_rate: u.sample_rate.unwrap_or(-1),
        bits_per_raw_sample: u.bits_per_raw_sample.unwrap_or(-1),
        sample_fmt: match u.sample_fmt {
            None => -1,
            Some(SampleFmt::S16) => 0,
            Some(SampleFmt::S32) => 1,
            Some(SampleFmt::S16P) => 2,
            Some(SampleFmt::S32P) => 3,
        },
    }
}

fn request(options: u32) -> Request {
    Request { want32: options & FLAC_RS_WANT32 != 0, planar: options & FLAC_RS_PLANAR != 0 }
}

/// Create a decoder. `has_extradata` says whether the context has
/// extradata at all (a null pointer in C); the stream parameters found in
/// it are written to `upd`.
///
/// Returns null on failure, with 1 in `err` for invalid extradata and 2
/// for anything else.
///
/// # Safety
/// `extradata` must be valid for reads of `extradata_size` bytes (or the
/// size is 0); `upd` and `err` must each be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn flac_rs_new(
    extradata: *const u8,
    extradata_size: usize,
    has_extradata: i32,
    options: u32,
    upd: *mut FlacRsUpdates,
    err: *mut i32,
) -> *mut Decoder {
    if upd.is_null() || err.is_null() {
        return ptr::null_mut();
    }
    let extradata = (has_extradata != 0).then(|| crate::bytes(extradata, extradata_size));
    match catch_unwind(|| Decoder::new(extradata, request(options))) {
        Ok(Ok((dec, u))) => {
            upd.write(updates(u));
            err.write(0);
            Box::into_raw(Box::new(dec))
        }
        Ok(Err(_)) => {
            err.write(1);
            ptr::null_mut()
        }
        Err(_) => {
            err.write(2);
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `dec` must be null or a pointer from `flac_rs_new` that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn flac_rs_free(dec: *mut Decoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// Parse a packet. `out.updates` is to be applied to the codec context
/// whatever the result is.
///
/// Returns `FLAC_RS_FRAME` when a frame was decoded and waits to be
/// written by `flac_rs_output`, `FLAC_RS_SKIP` when the packet (or
/// `out.consumed` bytes of it) held no frame, `FLAC_RS_INVALID` for
/// invalid data, or a negative error.
///
/// # Safety
/// `dec` must be a live pointer from `flac_rs_new`, not used by another
/// thread during the call; `pkt` must be valid for reads of `pkt_size`
/// bytes (or `pkt_size` is 0); `out` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn flac_rs_decode(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    options: u32,
    out: *mut FlacRsFrame,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return FLAC_RS_BAD_ARGS;
    };
    if out.is_null() {
        return FLAC_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let opts = Options {
        request: request(options),
        check_crc: options & FLAC_RS_CHECK_CRC != 0,
        buggy_lpc: options & FLAC_RS_BUGGY_LPC != 0,
    };
    let use_simd = simd();
    let result = catch_unwind(AssertUnwindSafe(|| {
        #[cfg(target_arch = "x86_64")]
        if use_simd {
            // SAFETY: `decode_avx2` is safe code compiled for AVX2, BMI1,
            // BMI2 and LZCNT; the only requirement for calling it is that
            // the CPU has them, which `simd()` checked.
            return unsafe { dec.decode_avx2(pkt, &opts) };
        }
        let _ = use_simd;
        dec.decode(pkt, &opts)
    }));
    let Ok((outcome, u)) = result else {
        return crate::FFSAFE_PANIC;
    };
    let mut frame = FlacRsFrame {
        updates: updates(u),
        consumed: 0,
        nb_samples: 0,
        channels: 0,
        crc_error: 0,
    };
    let ret = match outcome {
        Outcome::Skip(n) => {
            frame.consumed = n as i32;
            FLAC_RS_SKIP
        }
        Outcome::Frame(f) => {
            frame.consumed = f.bytes_read as i32;
            frame.nb_samples = f.samples as i32;
            frame.channels = f.channels as i32;
            frame.crc_error = i32::from(f.crc_error);
            FLAC_RS_FRAME
        }
        Outcome::Invalid => FLAC_RS_INVALID,
    };
    out.write(frame);
    ret
}

/// # Safety
/// `ptr` must be valid for reads and writes of `bytes` bytes and not
/// aliased; it is checked for alignment here.
unsafe fn samples<'a, T>(ptr: *mut u8, bytes: usize) -> Option<&'a mut [T]> {
    if ptr.is_null() || !(ptr as usize).is_multiple_of(align_of::<T>()) || bytes % size_of::<T>() != 0 {
        return None;
    }
    Some(slice::from_raw_parts_mut(ptr.cast::<T>(), bytes / size_of::<T>()))
}

/// # Safety
/// `planes` must be valid for reads of `nb_planes` pointers, each valid
/// for reads and writes of `plane_bytes` bytes; the planes must not
/// overlap (checked here) or be aliased during the call.
unsafe fn planar<'a, T>(
    planes: *const *mut u8,
    nb_planes: usize,
    plane_bytes: usize,
) -> Option<[&'a mut [T]; MAX_CHANNELS]> {
    let ptrs = slice::from_raw_parts(planes, nb_planes);
    for (i, &p) in ptrs.iter().enumerate() {
        for &q in &ptrs[..i] {
            if (p as usize).abs_diff(q as usize) < plane_bytes {
                return None;
            }
        }
    }
    let mut out: [&mut [T]; MAX_CHANNELS] = Default::default();
    for (o, &p) in out.iter_mut().zip(ptrs) {
        *o = samples(p, plane_bytes)?;
    }
    Some(out)
}

/// Write the frame that the last `flac_rs_decode` returned.
///
/// `fmt` is the context's sample format (0 S16, 1 S32, 2 S16P, 3 S32P).
/// `planes` are the frame's data planes: one for the interleaved formats,
/// one per channel for the planar ones. Each has `plane_bytes` bytes, the
/// samples of the frame and nothing more. `quirks` says where FFmpeg's
/// x86 assembly is in use (see `flac_core::Quirks`).
///
/// # Safety
/// `dec` as for `flac_rs_decode`; `planes` must be valid for reads of
/// `nb_planes` pointers, each of which must be valid for reads and writes
/// of `plane_bytes` bytes, not overlapping another plane and not aliased
/// during the call.
#[no_mangle]
pub unsafe extern "C" fn flac_rs_output(
    dec: *mut Decoder,
    planes: *const *mut u8,
    nb_planes: i32,
    plane_bytes: usize,
    fmt: i32,
    quirks: u32,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return FLAC_RS_BAD_ARGS;
    };
    let Ok(nb_planes) = usize::try_from(nb_planes) else {
        return FLAC_RS_BAD_ARGS;
    };
    if planes.is_null() || nb_planes == 0 || nb_planes > MAX_CHANNELS {
        return FLAC_RS_BAD_ARGS;
    }
    let quirks = Quirks {
        saturate_stereo: quirks & FLAC_RS_SATURATE_STEREO != 0,
        saturate_multi: quirks & FLAC_RS_SATURATE_MULTI != 0,
        saturate_8: quirks & FLAC_RS_SATURATE_8 != 0,
    };
    let use_simd = simd();
    let mut write = |out: Output| {
        catch_unwind(AssertUnwindSafe(|| {
            #[cfg(target_arch = "x86_64")]
            if use_simd {
                // SAFETY: as in `flac_rs_decode`.
                return unsafe { dec.output_avx2(out, quirks) };
            }
            let _ = use_simd;
            dec.output(out, quirks)
        }))
    };
    let result = match fmt {
        0 => match (nb_planes, samples::<i16>(planes.read(), plane_bytes)) {
            (1, Some(s)) => write(Output::S16(s)),
            _ => return FLAC_RS_BAD_ARGS,
        },
        1 => match (nb_planes, samples::<i32>(planes.read(), plane_bytes)) {
            (1, Some(s)) => write(Output::S32(s)),
            _ => return FLAC_RS_BAD_ARGS,
        },
        2 => match planar::<i16>(planes, nb_planes, plane_bytes) {
            Some(mut p) => write(Output::S16P(&mut p[..nb_planes])),
            None => return FLAC_RS_BAD_ARGS,
        },
        3 => match planar::<i32>(planes, nb_planes, plane_bytes) {
            Some(mut p) => write(Output::S32P(&mut p[..nb_planes])),
            None => return FLAC_RS_BAD_ARGS,
        },
        _ => return FLAC_RS_BAD_ARGS,
    };
    match result {
        Ok(Some(())) => 0,
        Ok(None) => FLAC_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use flac_core::{Decoder, Options, Outcome, Output, Quirks, Request};

    struct Bits(Vec<u8>, usize);

    impl Bits {
        fn put(&mut self, n: u32, v: u64) {
            for i in (0..n).rev() {
                if self.1 % 8 == 0 {
                    self.0.push(0);
                }
                *self.0.last_mut().unwrap() |= (((v >> i) & 1) as u8) << (7 - self.1 % 8);
                self.1 += 1;
            }
        }
    }

    /// A stereo frame of 256 samples: two LPC subframes of the given
    /// order with random warm-up samples, coefficients and residual bits.
    /// The header checksum is left for the caller to find.
    fn frame(seed: &mut u64, bps_code: u64, bps: u32, assignment: u64, order: u64) -> Vec<u8> {
        let mut rnd = || {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *seed >> 33
        };
        let mut w = Bits(Vec::new(), 0);
        w.put(16, 0xfff8);
        w.put(8, 0x79); // block size in 16 bits, 44100 Hz
        w.put(4, assignment);
        w.put(3, bps_code);
        w.put(1, 0);
        w.put(8, 0);
        w.put(16, 255);
        w.put(8, 0); // header checksum
        for ch in 0..2 {
            let side = matches!((assignment, ch), (8, 1) | (9, 0) | (10, 1));
            w.put(1, 0);
            w.put(6, 32 + order - 1);
            w.put(1, 0);
            for _ in 0..order {
                w.put(bps + u32::from(side), rnd() >> 4);
            }
            w.put(4, 11);
            w.put(5, 10);
            for _ in 0..order {
                w.put(12, rnd());
            }
            w.put(2, 1);
            w.put(4, 2);
            for part in 0..4 {
                let k = (rnd() % 14) as u32;
                w.put(5, u64::from(k));
                for _ in 0..if part == 0 { 64 - order } else { 64 } {
                    w.put((rnd() % 5) as u32, 0);
                    w.put(1, 1);
                    w.put(k, rnd());
                }
            }
        }
        w.put(32, 0); // padding to a byte and the frame checksum
        w.0
    }

    /// The decoder built for AVX2 and the portable one give the same
    /// samples, for every predictor order, stereo mode and output format.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx2_matches_portable() {
        if !super::simd() {
            return;
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut frames = 0;
        for case in 0..400u64 {
            let (bps_code, bps) = [(4, 16), (6, 24), (7, 32), (1, 8)][(case % 4) as usize];
            let assignment = [1, 8, 9, 10][(case / 4 % 4) as usize];
            let mut pkt = frame(&mut seed, bps_code, bps, assignment, 1 + case % 32);
            let opts = Options {
                request: Request { want32: case % 3 == 0, planar: case % 5 == 0 },
                check_crc: true,
                buggy_lpc: case % 7 == 0,
            };
            for crc in 0..=255 {
                pkt[7] = crc;
                let mut a = Decoder::new(None, opts.request).unwrap().0;
                let mut b = Decoder::new(None, opts.request).unwrap().0;
                let ra = a.decode(&pkt, &opts);
                // SAFETY: `simd()` found the CPU features `decode_avx2` and
                // `output_avx2` are compiled for.
                let rb = unsafe { b.decode_avx2(&pkt, &opts) };
                assert_eq!(ra, rb, "case {case}");
                let Outcome::Frame(f) = ra.0 else {
                    continue;
                };
                frames += 1;
                let quirks = Quirks { saturate_stereo: case % 2 == 0, ..Default::default() };
                let n = f.samples;
                let wide = bps > 16 || opts.request.want32;
                if opts.request.planar && wide {
                    let (mut a0, mut a1, mut b0, mut b1) = (vec![0i32; n], vec![0; n], vec![0; n], vec![0; n]);
                    a.output(Output::S32P(&mut [&mut a0, &mut a1]), quirks).unwrap();
                    unsafe { b.output_avx2(Output::S32P(&mut [&mut b0, &mut b1]), quirks) }.unwrap();
                    assert_eq!((a0, a1), (b0, b1), "case {case}");
                } else if opts.request.planar {
                    let (mut a0, mut a1, mut b0, mut b1) = (vec![0i16; n], vec![0; n], vec![0; n], vec![0; n]);
                    a.output(Output::S16P(&mut [&mut a0, &mut a1]), quirks).unwrap();
                    unsafe { b.output_avx2(Output::S16P(&mut [&mut b0, &mut b1]), quirks) }.unwrap();
                    assert_eq!((a0, a1), (b0, b1), "case {case}");
                } else if wide {
                    let (mut oa, mut ob) = (vec![0i32; 2 * n], vec![0i32; 2 * n]);
                    a.output(Output::S32(&mut oa), quirks).unwrap();
                    unsafe { b.output_avx2(Output::S32(&mut ob), quirks) }.unwrap();
                    assert_eq!(oa, ob, "case {case}");
                } else {
                    let (mut oa, mut ob) = (vec![0i16; 2 * n], vec![0i16; 2 * n]);
                    a.output(Output::S16(&mut oa), quirks).unwrap();
                    unsafe { b.output_avx2(Output::S16(&mut ob), quirks) }.unwrap();
                    assert_eq!(oa, ob, "case {case}");
                }
            }
        }
        assert_eq!(frames, 400);
    }
}
