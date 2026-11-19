//! C entry points for the safe TTA (True Audio) decoder.
//!
//! The decoder object holds the stream parameters and the per-channel
//! state, so it lives in a heap allocation that C holds as an opaque
//! pointer.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use tta_core::{frame_crc_ok, Decoder, Error, HeaderError, Samples};

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: turn the decoder with the SSE filter off (0) or back on, so
/// that the portable decoder can be exercised on a CPU that has AVX2.
#[no_mangle]
pub extern "C" fn tta_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

/// Decode with the fastest decoder this CPU can run.
fn decode(dec: &Decoder, pkt: &[u8], out: &mut Samples) -> Result<usize, Error> {
    #[cfg(target_arch = "x86_64")]
    if USE_SIMD.load(Ordering::Relaxed)
        && std::is_x86_feature_detected!("avx2")
        && std::is_x86_feature_detected!("bmi1")
        && std::is_x86_feature_detected!("bmi2")
    {
        // SAFETY: `decode_avx2` is safe code compiled for AVX2, BMI1 and
        // BMI2; the only requirement for calling it is that the CPU
        // supports them, which was just checked.
        return unsafe { dec.decode_avx2(pkt, out) };
    }
    dec.decode(pkt, out)
}

pub const TTA_RS_BAD_ARGS: i32 = -1;
pub const TTA_RS_INVALID_DATA: i32 = -2;

/// What the shim needs to know about the stream.
#[repr(C)]
pub struct TtaRsInfo {
    pub channels: i32,
    pub bits_per_raw_sample: i32,
    pub sample_rate: i32,
    /// Bytes per sample: 1, 2 or 3.
    pub bps: i32,
    /// Samples per channel in a full frame.
    pub frame_length: i32,
}

/// Create a decoder from the stream's TTA1 header.
///
/// Returns 0 and writes `*dec` and `*info`, or a positive number for a
/// refused header (in the order the C decoder checks: 1 too short, 2 wrong
/// signature, 3 invalid format, 4 missing password, 5 invalid number of
/// channels, 6 invalid sample rate, 7 unsupported sample format, 8 sample
/// rate too large, 9 frame length too large), or a negative error.
///
/// # Safety
/// `extradata` must be valid for reads of `extradata_size` bytes (or the
/// size is 0); `password` must be null (no password) or valid for reads of
/// `password_len` bytes; `info` and `dec` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn tta_rs_new(
    extradata: *const u8,
    extradata_size: usize,
    password: *const u8,
    password_len: usize,
    info: *mut TtaRsInfo,
    dec: *mut *mut Decoder,
) -> i32 {
    if info.is_null() || dec.is_null() {
        return TTA_RS_BAD_ARGS;
    }
    let extradata = crate::bytes(extradata, extradata_size);
    let password = if password.is_null() {
        None
    } else {
        // A password may be the empty string, which `bytes` also handles.
        Some(crate::bytes(password, password_len))
    };
    match catch_unwind(|| Decoder::new(extradata, password).map(Box::new)) {
        Ok(Ok(d)) => {
            let h = d.header();
            info.write(TtaRsInfo {
                channels: i32::from(h.channels),
                bits_per_raw_sample: i32::from(h.bits_per_raw_sample),
                sample_rate: h.sample_rate,
                bps: i32::from(h.bps),
                frame_length: h.frame_length as i32,
            });
            dec.write(Box::into_raw(d));
            0
        }
        Ok(Err(e)) => match e {
            HeaderError::TooShort => 1,
            HeaderError::WrongSignature => 2,
            HeaderError::InvalidFormat => 3,
            HeaderError::MissingPassword => 4,
            HeaderError::InvalidChannels => 5,
            HeaderError::InvalidSampleRate => 6,
            HeaderError::UnsupportedSampleFormat => 7,
            HeaderError::SampleRateTooLarge => 8,
            HeaderError::FrameLengthTooLarge => 9,
        },
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// # Safety
/// `dec` must be null or a pointer from `tta_rs_new` that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn tta_rs_free(dec: *mut Decoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// Check the CRC-32 at the end of a packet of at least four bytes.
/// Returns 1 if it is right, 0 if not, or a negative error.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0).
#[no_mangle]
pub unsafe extern "C" fn tta_rs_crc_ok(pkt: *const u8, pkt_size: usize) -> i32 {
    let pkt = crate::bytes(pkt, pkt_size);
    match catch_unwind(|| frame_crc_ok(pkt)) {
        Ok(ok) => i32::from(ok),
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// # Safety
/// `ptr` must be valid for reads and writes of `bytes` bytes and not
/// aliased; it is checked for alignment here.
unsafe fn samples<'a, T>(ptr: *mut u8, bytes: usize) -> Option<&'a mut [T]> {
    if ptr.is_null() || !(ptr as usize).is_multiple_of(align_of::<T>()) {
        return None;
    }
    Some(slice::from_raw_parts_mut(ptr.cast::<T>(), bytes / size_of::<T>()))
}

/// Decode one packet into a frame's interleaved samples.
///
/// `out` is the frame's sample buffer and `out_bytes` the number of bytes
/// the decoder may touch: a full frame, `frame_length * channels` samples
/// of 1, 2 or 4 bytes. Returns the number of samples per channel that were
/// decoded (the frame length, or less for the stream's last frame), or
/// `TTA_RS_INVALID_DATA`, or another negative error.
///
/// # Safety
/// `dec` must be a live pointer from `tta_rs_new`, not used by another
/// thread during the call; `pkt` must be valid for reads of `pkt_size`
/// bytes (or `pkt_size` is 0); `out` must be valid for reads and writes of
/// `out_bytes` bytes and not aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn tta_rs_decode(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    out: *mut u8,
    out_bytes: usize,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return TTA_RS_BAD_ARGS;
    };
    let pkt = crate::bytes(pkt, pkt_size);
    let out = match dec.header().bps {
        1 => samples::<u8>(out, out_bytes).map(Samples::U8),
        2 => samples::<i16>(out, out_bytes).map(Samples::S16),
        _ => samples::<i32>(out, out_bytes).map(Samples::S32),
    };
    let Some(mut out) = out else {
        return TTA_RS_BAD_ARGS;
    };
    match catch_unwind(AssertUnwindSafe(|| decode(dec, pkt, &mut out))) {
        Ok(Ok(n)) => i32::try_from(n).unwrap_or(TTA_RS_BAD_ARGS),
        Ok(Err(Error::InvalidData)) => TTA_RS_INVALID_DATA,
        Ok(Err(Error::BadOutput)) => TTA_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use tta_core::{Decoder, Samples};

    /// The decoder with the SSE filter and the portable one agree, on
    /// random packets (any bit string is a TTA frame if it is long enough)
    /// for every sample size and channel count.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn simd_matches_portable() {
        if !(std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2"))
        {
            return;
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        let mut decoded = 0;
        for case in 0..600 {
            let channels = 1 + (case % 16) as u16;
            let bits = [8u16, 16, 24][case % 3];
            let format = if case % 5 == 0 { 2u16 } else { 1 };
            let mut h = [0u8; 22];
            h[..4].copy_from_slice(b"TTA1");
            h[4..6].copy_from_slice(&format.to_le_bytes());
            h[6..8].copy_from_slice(&channels.to_le_bytes());
            h[8..10].copy_from_slice(&bits.to_le_bytes());
            h[10..14].copy_from_slice(&(1 + next() % 200).to_le_bytes());
            h[14..18].copy_from_slice(&(next() % 500).to_le_bytes());
            let dec = Decoder::new(&h, Some(b"secret")).unwrap();
            let total = dec.header().frame_length as usize * usize::from(channels);
            // Zero bits end codes, so fewer ones mean shorter codes.
            let density = case % 4;
            let pkt: Vec<u8> = (0..next() % 8 + total as u32 * (2 + next() % 3))
                .map(|_| match density {
                    0 => next() as u8,
                    1 => (next() & next()) as u8,
                    2 => (next() & next() & next()) as u8,
                    _ => (next() | next()) as u8,
                })
                .collect();
            let (mut a8, mut b8) = (vec![0u8; total], vec![0u8; total]);
            let (mut a16, mut b16) = (vec![0i16; total], vec![0i16; total]);
            let (mut a32, mut b32) = (vec![0i32; total], vec![0i32; total]);
            let (mut a, mut b) = match bits {
                8 => (Samples::U8(&mut a8), Samples::U8(&mut b8)),
                16 => (Samples::S16(&mut a16), Samples::S16(&mut b16)),
                _ => (Samples::S32(&mut a32), Samples::S32(&mut b32)),
            };
            let ra = dec.decode(&pkt, &mut a);
            // SAFETY: the CPU features were detected above.
            let rb = unsafe { dec.decode_avx2(&pkt, &mut b) };
            assert_eq!(ra, rb, "case {case}");
            if let Ok(n) = ra {
                let n = n * usize::from(channels);
                assert_eq!((&a8[..n], &a16[..n], &a32[..n]), (&b8[..n], &b16[..n], &b32[..n]), "case {case}");
                decoded += 1;
            }
        }
        assert!(decoded > 100, "only {decoded} packets decoded");
    }

    /// Not a test: prints decode time per sample for both decoders.
    /// `cargo test --release -p ffsafe-ffi -- --ignored --nocapture timing`
    #[test]
    #[ignore]
    #[cfg(target_arch = "x86_64")]
    fn timing() {
        if !(std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2"))
        {
            return;
        }
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for (name, channels, density) in [("zeros 2ch", 2u16, 0), ("zeros 1ch", 1, 0), ("sparse 1ch", 1, 1), ("sparse 2ch", 2, 1)] {
            let mut h = [0u8; 22];
            h[..4].copy_from_slice(b"TTA1");
            h[4..6].copy_from_slice(&1u16.to_le_bytes());
            h[6..8].copy_from_slice(&channels.to_le_bytes());
            h[8..10].copy_from_slice(&16u16.to_le_bytes());
            h[10..14].copy_from_slice(&44100u32.to_le_bytes());
            let dec = Decoder::new(&h, None).unwrap();
            let total = dec.header().frame_length as usize * usize::from(channels);
            let pkt: Vec<u8> = (0..total * 4)
                .map(|_| match density {
                    0 => 0,
                    1 => (next() & next() & next()) as u8,
                    _ => next() as u8,
                })
                .collect();
            let mut o = vec![0i16; total];
            for simd in [false, true] {
                let mut best = f64::MAX;
                for _ in 0..30 {
                    let t = std::time::Instant::now();
                    let r = if simd {
                        // SAFETY: the CPU features were detected above.
                        unsafe { dec.decode_avx2(&pkt, &mut Samples::S16(&mut o)) }
                    } else {
                        dec.decode(&pkt, &mut Samples::S16(&mut o))
                    };
                    assert!(r.is_ok());
                    best = best.min(t.elapsed().as_secs_f64());
                }
                println!("{name:12} simd={simd:5} {:6.2} ns/sample", best * 1e9 / total as f64);
            }
        }
    }
}
