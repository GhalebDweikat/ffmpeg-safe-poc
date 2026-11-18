//! C entry points for the safe Cinepak decoder.
//!
//! Cinepak keeps its codebooks from one frame to the next, so the decoder
//! lives in a heap allocation that C holds as an opaque pointer.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;

use cinepak_core::{Decoder, Frame, Precheck};

pub const CINEPAK_RS_BAD_ARGS: i32 = -1;

/// Create a decoder. Returns null on failure.
#[no_mangle]
pub extern "C" fn cinepak_rs_new(width: i32, height: i32, bits: i32) -> *mut Decoder {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return ptr::null_mut();
    };
    catch_unwind(|| Box::into_raw(Box::new(Decoder::new(width, height, bits))))
        .unwrap_or(ptr::null_mut())
}

/// # Safety
/// `dec` must be null or a pointer from `cinepak_rs_new` that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn cinepak_rs_free(dec: *mut Decoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// The checks made before a frame is allocated.
///
/// Returns 0 to go on and decode, 1 for a packet that is too small, 2 for
/// an empty frame, 3 for an inconsistent header, 4 for an encoded size of
/// zero; or a negative error.
///
/// # Safety
/// `dec` must be a live pointer from `cinepak_rs_new`, not used by another
/// thread during the call; `pkt` must be valid for reads of `pkt_size`
/// bytes (or `pkt_size` is 0).
#[no_mangle]
pub unsafe extern "C" fn cinepak_rs_check(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    discard_damaged_percentage: i32,
    has_palette: i32,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return CINEPAK_RS_BAD_ARGS;
    };
    let pkt = crate::bytes(pkt, pkt_size);
    let result = catch_unwind(AssertUnwindSafe(|| {
        dec.precheck(pkt, discard_damaged_percentage, has_palette != 0)
    }));
    match result {
        Ok(Precheck::Decode) => 0,
        Ok(Precheck::TooSmall) => 1,
        Ok(Precheck::Empty) => 2,
        Ok(Precheck::Invalid) => 3,
        Ok(Precheck::ZeroEncodedSize) => 4,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Decode a packet that passed `cinepak_rs_check` onto the visible picture
/// at `frame`.
///
/// `frame_size` is the number of bytes the decoder may touch:
/// `stride * (height - 1)` plus one visible row. Writes 1 to `key` for a
/// key frame, else 0. Returns 0, or 1 if the data was damaged (the picture
/// is still to be output), or a negative error.
///
/// # Safety
/// As for `cinepak_rs_check`; in addition `frame` must be valid for reads
/// and writes of `frame_size` bytes and not aliased during the call, and
/// `key` must be null or valid for one write.
#[no_mangle]
pub unsafe extern "C" fn cinepak_rs_decode(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    frame: *mut u8,
    frame_size: usize,
    stride: usize,
    width: i32,
    height: i32,
    key: *mut i32,
) -> i32 {
    let (Some(dec), Ok(width), Ok(height)) =
        (dec.as_mut(), usize::try_from(width), usize::try_from(height))
    else {
        return CINEPAK_RS_BAD_ARGS;
    };
    if frame.is_null() {
        return CINEPAK_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let pixels = slice::from_raw_parts_mut(frame, frame_size);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let row_len = width.checked_mul(if dec.palette_video() { 1 } else { 3 })?;
        let mut frame = Frame::new(pixels, stride, row_len, height)?;
        Some(dec.decode(pkt, &mut frame))
    }));

    match result {
        Ok(Some((is_key, outcome))) => {
            if !key.is_null() {
                key.write(i32::from(is_key));
            }
            i32::from(outcome.is_err())
        }
        Ok(None) => CINEPAK_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}
