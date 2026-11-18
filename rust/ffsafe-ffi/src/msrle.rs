//! C entry points for the safe Microsoft RLE decoder.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use msrle_core::{decode, palette_from_extradata, Frame, Status};

pub const MSRLE_RS_BAD_FRAME: i32 = -1;

fn status_code(s: Status) -> i32 {
    match s {
        Status::Done => 0,
        Status::Uncompressed => 1,
        Status::NoEndOfPicture => 2,
        Status::NextLineBeyondPicture => 3,
        Status::SkipBeyondPicture => 4,
        Status::Overrun => 5,
        Status::Pal4Overrun => 6,
        Status::Pal4CopyOutOfBounds => 7,
        Status::Pal4RunOutOfBounds => 8,
        Status::Pal4Leftover => 9,
        Status::UnknownDepth => 10,
    }
}

/// Decode one packet onto the visible picture at `frame`.
///
/// `frame_size` is the number of bytes the decoder may touch:
/// `stride * (height - 1)` plus one visible row. Returns a status of 0 or
/// more (the picture is valid in every such case), `MSRLE_RS_BAD_FRAME`, or
/// `FFSAFE_PANIC`.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// and `frame` must be valid for reads and writes of `frame_size` bytes and
/// not aliased for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn msrle_rs_decode(
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
        return MSRLE_RS_BAD_FRAME;
    };
    if frame.is_null() {
        return MSRLE_RS_BAD_FRAME;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let pixels = slice::from_raw_parts_mut(frame, frame_size);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut frame = Frame::new(pixels, stride, width, height, bits)?;
        Some(decode(pkt, bits, &mut frame))
    }));

    match result {
        Ok(Some(status)) => status_code(status),
        Ok(None) => MSRLE_RS_BAD_FRAME,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Fill `pal` from the palette in the stream header.
///
/// # Safety
/// `extradata` must be valid for reads of `size` bytes (or `size` is 0),
/// and `pal` must be valid for reads and writes of 256 `u32`s.
#[no_mangle]
pub unsafe extern "C" fn msrle_rs_palette(extradata: *const u8, size: usize, pal: *mut u32) -> i32 {
    if pal.is_null() {
        return MSRLE_RS_BAD_FRAME;
    }
    let extradata = crate::bytes(extradata, size);
    let pal = &mut *pal.cast::<[u32; 256]>();
    match catch_unwind(AssertUnwindSafe(|| palette_from_extradata(extradata, pal))) {
        Ok(()) => 0,
        Err(_) => crate::FFSAFE_PANIC,
    }
}
