//! C entry points for the safe QuickTime Animation (RLE) video decoder.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use qtrle_core::{check_packet, decode, Frame, Status, Verdict};

pub const QTRLE_RS_BAD_FRAME: i32 = -1;

/// The header checks the C decoder makes before it asks for a frame.
///
/// Returns 0 (decode the packet), 1 (nothing changes: output the previous
/// picture again), 2 (reject the packet), `QTRLE_RS_BAD_FRAME`, or
/// `FFSAFE_PANIC`.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0).
#[no_mangle]
pub unsafe extern "C" fn qtrle_rs_check(
    pkt: *const u8,
    pkt_size: usize,
    height: i32,
    discard_damaged_percentage: i32,
) -> i32 {
    let Ok(height) = usize::try_from(height) else {
        return QTRLE_RS_BAD_FRAME;
    };
    let pkt = crate::bytes(pkt, pkt_size);
    match catch_unwind(|| check_packet(pkt, height, discard_damaged_percentage)) {
        Ok(Verdict::Decode) => 0,
        Ok(Verdict::Duplicate) => 1,
        Ok(Verdict::Invalid) => 2,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Decode one packet onto the visible picture at `frame`.
///
/// `frame_size` is the number of bytes the decoder may touch:
/// `stride * (height - 1)` plus one visible row. Returns a status of 0 or
/// more (the picture is valid in every such case), `QTRLE_RS_BAD_FRAME`, or
/// `FFSAFE_PANIC`.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// and `frame` must be valid for reads and writes of `frame_size` bytes and
/// not aliased for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn qtrle_rs_decode(
    pkt: *const u8,
    pkt_size: usize,
    frame: *mut u8,
    frame_size: usize,
    stride: usize,
    width: i32,
    height: i32,
    bits: i32,
) -> i32 {
    let (Ok(width), Ok(height), Ok(bits)) =
        (usize::try_from(width), usize::try_from(height), u32::try_from(bits))
    else {
        return QTRLE_RS_BAD_FRAME;
    };
    if frame.is_null() {
        return QTRLE_RS_BAD_FRAME;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let pixels = slice::from_raw_parts_mut(frame, frame_size);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut frame = Frame::new(pixels, stride, width, height, bits)?;
        Some(decode(pkt, bits, &mut frame))
    }));

    match result {
        Ok(Some(Status::Done)) => 0,
        Ok(Some(Status::OutOfBounds)) => 1,
        Ok(Some(Status::Duplicate)) => 2,
        Ok(Some(Status::UnknownDepth)) => 3,
        Ok(None) => QTRLE_RS_BAD_FRAME,
        Err(_) => crate::FFSAFE_PANIC,
    }
}
