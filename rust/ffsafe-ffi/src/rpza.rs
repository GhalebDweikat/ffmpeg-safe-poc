//! C entry points for the safe RPZA decoder.
//!
//! Turns the (pointer, length) pairs handed over by the C shim into slices.
//! Everything after that runs in `rpza-core`, which forbids `unsafe`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use rpza_core::{decode, header, packet_is_plausible, Error, Frame};

pub const RPZA_RS_OK: i32 = 0;
pub const RPZA_RS_PACKET_TOO_SMALL: i32 = -1;
pub const RPZA_RS_FOUR_COLOR_TRUNCATED: i32 = -2;
pub const RPZA_RS_SIXTEEN_COLOR_TRUNCATED: i32 = -3;
pub const RPZA_RS_NO_BLOCKS_LEFT: i32 = -4;
pub const RPZA_RS_UNKNOWN_OPCODE: i32 = -5;
/// The caller passed a frame the decoder cannot use (bad size or stride).
pub const RPZA_RS_BAD_FRAME: i32 = -6;
/// The decoder panicked. This is a bug in the port, never a property of the
/// input; it is reported instead of unwinding into C.
pub const RPZA_RS_PANIC: i32 = crate::FFSAFE_PANIC;

#[repr(C)]
pub struct RpzaRsHeader {
    pub first_byte: u32,
    pub chunk_size: u32,
    pub payload_size: u32,
}


/// Read the header fields and run the early size check.
///
/// Returns `RPZA_RS_OK` or `RPZA_RS_PACKET_TOO_SMALL`. Touches no frame.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// and `out` must be null or valid for one write.
#[no_mangle]
pub unsafe extern "C" fn rpza_rs_header(
    pkt: *const u8,
    pkt_size: usize,
    width: i32,
    height: i32,
    out: *mut RpzaRsHeader,
) -> i32 {
    let pkt = crate::bytes(pkt, pkt_size);
    let result = catch_unwind(|| {
        let h = header(pkt);
        let plausible = match (usize::try_from(width), usize::try_from(height)) {
            (Ok(w), Ok(h)) => packet_is_plausible(pkt.len(), w, h),
            _ => false,
        };
        (h, plausible)
    });
    match result {
        Ok((h, plausible)) => {
            if !out.is_null() {
                out.write(RpzaRsHeader {
                    first_byte: u32::from(h.first_byte),
                    chunk_size: h.chunk_size,
                    payload_size: u32::try_from(h.payload_len).unwrap_or(u32::MAX),
                });
            }
            if plausible {
                RPZA_RS_OK
            } else {
                RPZA_RS_PACKET_TOO_SMALL
            }
        }
        Err(_) => RPZA_RS_PANIC,
    }
}

/// Decode one packet into the visible image at `frame`.
///
/// `frame_size` is the number of bytes the decoder may touch:
/// `stride * (height - 1) + width * 2`.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// `frame` must be valid for reads and writes of `frame_size` bytes and not
/// aliased for the duration of the call, and `opcode` must be null or valid
/// for one write.
#[no_mangle]
pub unsafe extern "C" fn rpza_rs_decode(
    pkt: *const u8,
    pkt_size: usize,
    frame: *mut u8,
    frame_size: usize,
    stride: usize,
    width: i32,
    height: i32,
    opcode: *mut u32,
) -> i32 {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return RPZA_RS_BAD_FRAME;
    };
    if frame.is_null() {
        return RPZA_RS_BAD_FRAME;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let pixels = slice::from_raw_parts_mut(frame, frame_size);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(mut frame) = Frame::new(pixels, stride, width, height) else {
            return Err(None);
        };
        decode(pkt, &mut frame).map_err(Some)
    }));

    match result {
        Ok(Ok(())) => RPZA_RS_OK,
        Ok(Err(None)) => RPZA_RS_BAD_FRAME,
        Ok(Err(Some(e))) => match e {
            Error::PacketTooSmall => RPZA_RS_PACKET_TOO_SMALL,
            Error::FourColorTruncated => RPZA_RS_FOUR_COLOR_TRUNCATED,
            Error::SixteenColorTruncated => RPZA_RS_SIXTEEN_COLOR_TRUNCATED,
            Error::NoBlocksLeft => RPZA_RS_NO_BLOCKS_LEFT,
            Error::UnknownOpcode(op) => {
                if !opcode.is_null() {
                    opcode.write(u32::from(op));
                }
                RPZA_RS_UNKNOWN_OPCODE
            }
        },
        Err(_) => RPZA_RS_PANIC,
    }
}
