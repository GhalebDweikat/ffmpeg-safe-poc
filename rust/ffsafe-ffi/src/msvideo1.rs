//! C entry points for the safe Microsoft Video 1 decoder.
//!
//! The decoder keeps nothing between frames except the picture itself,
//! which the C shim owns, so these are plain functions.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use msvideo1_core::{decode, packet_is_plausible, Depth, Frame, Outcome};

pub const MSVIDEO1_RS_BAD_ARGS: i32 = -1;

/// The check made before a frame is allocated. Returns 1 if a packet of
/// `pkt_size` bytes may be decoded, 0 if it is too small for the picture,
/// or a negative error.
#[no_mangle]
pub extern "C" fn msvideo1_rs_check(pkt_size: usize, width: i32, height: i32) -> i32 {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return MSVIDEO1_RS_BAD_ARGS;
    };
    match catch_unwind(|| packet_is_plausible(pkt_size, width, height)) {
        Ok(ok) => i32::from(ok),
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Decode a packet onto the visible picture at `frame`, which holds the
/// previous picture.
///
/// `frame_size` is the number of bytes the decoder may touch:
/// `stride * (height - 1)` plus one visible row. `palette` is non-zero for
/// 8-bit palette video, zero for 16-bit RGB555. Returns 0 if the whole
/// picture was decoded, 1 if the packet ran out first (the picture is still
/// to be output, but without a palette update), or a negative error.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0);
/// `frame` must be valid for reads and writes of `frame_size` bytes and not
/// aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn msvideo1_rs_decode(
    pkt: *const u8,
    pkt_size: usize,
    frame: *mut u8,
    frame_size: usize,
    stride: usize,
    width: i32,
    height: i32,
    palette: i32,
) -> i32 {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return MSVIDEO1_RS_BAD_ARGS;
    };
    if frame.is_null() {
        return MSVIDEO1_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let pixels = slice::from_raw_parts_mut(frame, frame_size);
    let depth = if palette != 0 { Depth::Pal8 } else { Depth::Rgb555 };

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut frame = Frame::new(pixels, stride, width, height, depth)?;
        Some(decode(pkt, &mut frame))
    }));

    match result {
        Ok(Some(Outcome::Finished)) => 0,
        Ok(Some(Outcome::Truncated)) => 1,
        Ok(None) => MSVIDEO1_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}
