//! C entry points for the safe Apple Intermediate Codec decoder.
//!
//! The decoder keeps its coefficient buffer from one slice to the next (as
//! the C decoder does, see `aic-core`), so it lives in a heap allocation
//! that C holds as an opaque pointer.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use aic_core::{Decoder, Error, Frame, Idct};

pub const AIC_RS_BAD_ARGS: i32 = -1;

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: use the portable code (0) or the SSE2 code again, so that
/// the portable decoder can be exercised on x86-64.
#[no_mangle]
pub extern "C" fn aic_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

/// Create a decoder. `x86_idct` says which of FFmpeg's IDCTs to reproduce:
/// non-zero for the x86 assembly one, 0 for the C one. Returns null on
/// failure.
#[no_mangle]
pub extern "C" fn aic_rs_new(width: i32, height: i32, x86_idct: i32) -> *mut Decoder {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return ptr::null_mut();
    };
    let idct = if x86_idct != 0 { Idct::X86 } else { Idct::C };
    catch_unwind(|| Box::into_raw(Box::new(Decoder::new(width, height, idct))))
        .unwrap_or(ptr::null_mut())
}

/// # Safety
/// `dec` must be null or a pointer from `aic_rs_new` that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn aic_rs_free(dec: *mut Decoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// What went wrong, for the log: the two numbers that FFmpeg's message
/// for the error shows.
#[repr(C)]
pub struct AicRsError {
    pub a: u32,
    pub b: u32,
}

fn status(e: Error, info: &mut AicRsError) -> i32 {
    let (code, a, b) = match e {
        Error::TooSmall => (1, 0, 0),
        Error::Version(v) => (2, u32::from(v), 0),
        Error::HeaderSize(s) => (3, u32::from(s), 0),
        Error::FrameSize(s) => (4, s, 0),
        Error::Dimensions(w, h) => (5, u32::from(w), u32::from(h)),
        Error::SliceSize(x, y) => (6, x as u32, y as u32),
        Error::Slice(x, y) => (7, x as u32, y as u32),
    };
    *info = AicRsError { a, b };
    code
}

/// The checks made before a frame is allocated.
///
/// Returns 0 to go on and decode, or 1 for a packet too small for the
/// slice table, 2 for a wrong version, 3 for a wrong header size, 4 for a
/// frame size larger than the packet, 5 for a picture size other than
/// `width` x `height`; or a negative error.
///
/// # Safety
/// `dec` must be a live pointer from `aic_rs_new`, not used by another
/// thread during the call; `pkt` must be valid for reads of `pkt_size`
/// bytes (or `pkt_size` is 0); `info` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn aic_rs_check(
    dec: *const Decoder,
    pkt: *const u8,
    pkt_size: usize,
    width: i32,
    height: i32,
    info: *mut AicRsError,
) -> i32 {
    let (Some(dec), Some(info), Ok(width), Ok(height)) =
        (dec.as_ref(), info.as_mut(), usize::try_from(width), usize::try_from(height))
    else {
        return AIC_RS_BAD_ARGS;
    };
    let pkt = crate::bytes(pkt, pkt_size);
    match catch_unwind(AssertUnwindSafe(|| dec.check(pkt, width, height))) {
        Ok(Ok(_)) => 0,
        Ok(Err(e)) => status(e, info),
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Decode a packet onto the visible picture.
///
/// Each plane's length is what the decoder may touch: `stride * (rows -
/// 1)` plus one visible row, where the chroma planes have half the width
/// and height, rounded up. Returns 0, or one of the codes of
/// `aic_rs_check`, or 6 for a bad slice size, or 7 for bad slice data (the
/// picture is then partly written and must not be output); or a negative
/// error.
///
/// # Safety
/// As for `aic_rs_check`; in addition `y`, `u` and `v` must be valid for
/// reads and writes of `y_len`, `u_len` and `v_len` bytes, must not
/// overlap, and must not be aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn aic_rs_decode(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    y: *mut u8,
    y_len: usize,
    y_stride: usize,
    u: *mut u8,
    u_len: usize,
    u_stride: usize,
    v: *mut u8,
    v_len: usize,
    v_stride: usize,
    width: i32,
    height: i32,
    info: *mut AicRsError,
) -> i32 {
    let (Some(dec), Some(info), Ok(width), Ok(height)) =
        (dec.as_mut(), info.as_mut(), usize::try_from(width), usize::try_from(height))
    else {
        return AIC_RS_BAD_ARGS;
    };
    if y.is_null() || u.is_null() || v.is_null() {
        return AIC_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let planes = [
        slice::from_raw_parts_mut(y, y_len),
        slice::from_raw_parts_mut(u, u_len),
        slice::from_raw_parts_mut(v, v_len),
    ];

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut frame = Frame::new(planes, [y_stride, u_stride, v_stride], width, height)?;
        #[cfg(target_arch = "x86_64")]
        if USE_SIMD.load(Ordering::Relaxed) && cfg!(target_feature = "sse2") {
            // SAFETY: `decode_sse2` is safe code compiled for SSE2; the
            // only requirement for calling it is that the CPU supports
            // SSE2, and this crate itself is compiled for SSE2 (checked
            // just above, at compile time), as all x86-64 code is.
            return Some(unsafe { dec.decode_sse2(pkt, &mut frame) });
        }
        Some(dec.decode(pkt, &mut frame))
    }));
    match result {
        Ok(Some(Ok(()))) => 0,
        Ok(Some(Err(e))) => status(e, info),
        Ok(None) => AIC_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use aic_core::{block_portable, Idct};

    /// The SSE2 block function and the portable one agree, for both kinds
    /// of IDCT, on small, large and extreme coefficients.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn sse2_matches_portable() {
        if !cfg!(target_feature = "sse2") {
            return;
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 40) as u32
        };
        for round in 0..300_000 {
            let mut coeffs = [0i16; 64];
            let fill = next() % 65;
            for _ in 0..fill {
                let v = match round % 4 {
                    0 => next() % 64,
                    1 => next() % 2048,
                    2 => next() % 65536,
                    _ => [0xffff, 0xfffe, 0x8000, 0x7fff, 1, 2][(next() % 6) as usize],
                };
                coeffs[(next() % 64) as usize] = v as i16;
            }
            let quant = if round % 3 == 0 { next() as u8 } else { (next() % 32) as u8 };
            for kind in [Idct::C, Idct::X86] {
                // SAFETY: this crate is compiled for SSE2 (checked above).
                let simd = unsafe { aic_core::block_sse2(&coeffs, quant, kind) };
                assert_eq!(simd, block_portable(&coeffs, quant, kind), "round {round} {kind:?} {coeffs:?}");
            }
        }
    }
}
