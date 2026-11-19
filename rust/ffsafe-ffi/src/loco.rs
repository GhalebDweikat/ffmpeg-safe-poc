//! C entry points for the safe LOCO decoder.
//!
//! LOCO keeps nothing from one frame to the next: the two numbers the
//! extradata yields (pixel format and lossy parameter) are held by the C
//! shim and passed to every decode call.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use loco_core::{decode_frame, plane_count, plane_size, Config, Error, Format, InitError, Plane};

/// The packet is damaged or too short.
pub const LOCO_RS_TOO_SMALL: i32 = -1;
pub const LOCO_RS_BAD_ARGS: i32 = -2;

/// What the extradata says. `format` is 0 for planar 4:2:2, 1 for planar
/// 4:2:0, 2 for planar GBR, 3 for planar GBRA.
#[repr(C)]
pub struct LocoRsInfo {
    pub version: i32,
    pub mode: i32,
    pub lossy: i32,
    pub format: i32,
}

fn format_from(code: i32) -> Option<Format> {
    match code {
        0 => Some(Format::Yuv422),
        1 => Some(Format::Yuv420),
        2 => Some(Format::Gbr),
        3 => Some(Format::Gbra),
        _ => None,
    }
}

/// Read the extradata.
///
/// Returns 0 and fills `info`; or 1 if the extradata is too small, 2 if
/// the lossy parameter is out of range (`version` and `lossy` are filled
/// in), 3 if the mode is unknown (`version`, `lossy` and `mode` are filled
/// in); or a negative error.
///
/// # Safety
/// `extradata` must be valid for reads of `size` bytes (or `size` is 0);
/// `info` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn loco_rs_init(extradata: *const u8, size: usize, info: *mut LocoRsInfo) -> i32 {
    if info.is_null() {
        return LOCO_RS_BAD_ARGS;
    }
    let extradata = crate::bytes(extradata, size);
    let Ok(result) = catch_unwind(|| Config::from_extradata(extradata)) else {
        return crate::FFSAFE_PANIC;
    };
    let (status, version, mode, lossy, format) = match result {
        Ok(c) => {
            let format = match c.format {
                Format::Yuv422 => 0,
                Format::Yuv420 => 1,
                Format::Gbr => 2,
                Format::Gbra => 3,
            };
            (0, c.version, c.mode, c.lossy, format)
        }
        Err(InitError::ExtradataTooSmall) => (1, 0, 0, 0, -1),
        Err(InitError::LossyTooLarge { version, lossy }) => (2, version, 0, lossy, -1),
        Err(InitError::UnknownMode { version, lossy, mode }) => (3, version, mode, lossy, -1),
    };
    info.write(LocoRsInfo { version, mode, lossy, format });
    status
}

/// Decode one packet into the planes of a new frame.
///
/// `planes`, `plane_sizes` and `strides` each have one entry per plane of
/// the format (three, or four with alpha). `plane_sizes[i]` is the number
/// of bytes of plane `i` the decoder may touch: `strides[i] * (rows - 1)`
/// plus one visible row. Returns the number of packet bytes used, or
/// `LOCO_RS_TOO_SMALL` for a damaged packet, or another negative error.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0).
/// The three arrays must be valid for reads of as many entries as the
/// format has planes; each `planes[i]` must be valid for reads and writes
/// of `plane_sizes[i]` bytes, and the planes must not overlap each other
/// or be used by anything else during the call.
#[no_mangle]
pub unsafe extern "C" fn loco_rs_decode(
    format: i32,
    lossy: i32,
    width: i32,
    height: i32,
    pkt: *const u8,
    pkt_size: usize,
    planes: *const *mut u8,
    plane_sizes: *const usize,
    strides: *const usize,
) -> i32 {
    let (Some(format), Ok(width), Ok(height)) =
        (format_from(format), usize::try_from(width), usize::try_from(height))
    else {
        return LOCO_RS_BAD_ARGS;
    };
    if planes.is_null() || plane_sizes.is_null() || strides.is_null() {
        return LOCO_RS_BAD_ARGS;
    }
    let count = plane_count(format);
    let pointers = slice::from_raw_parts(planes, count);
    let sizes = slice::from_raw_parts(plane_sizes, count);
    let strides = slice::from_raw_parts(strides, count);
    if pointers.iter().any(|p| p.is_null()) {
        return LOCO_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let mut buffers: [&mut [u8]; 4] = [&mut [], &mut [], &mut [], &mut []];
    for i in 0..count {
        buffers[i] = slice::from_raw_parts_mut(pointers[i], sizes[i]);
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut frame: [Option<Plane<'_>>; 4] = [None, None, None, None];
        for (i, buffer) in buffers.into_iter().enumerate().take(count) {
            let (row_len, rows) = plane_size(format, i, width, height);
            frame[i] = Some(Plane::new(buffer, strides[i], row_len, rows)?);
        }
        let [a, b, c, d] = frame;
        let used = match (a, b, c, d) {
            (Some(a), Some(b), Some(c), Some(d)) => {
                decode_frame(format, lossy, width, height, pkt, &mut [a, b, c, d])
            }
            (Some(a), Some(b), Some(c), None) => decode_frame(format, lossy, width, height, pkt, &mut [a, b, c]),
            _ => return None,
        };
        Some(used)
    }));

    match result {
        Ok(Some(Ok(used))) => i32::try_from(used).unwrap_or(LOCO_RS_BAD_ARGS),
        Ok(Some(Err(Error::TooSmall))) => LOCO_RS_TOO_SMALL,
        Ok(Some(Err(Error::BadArgs))) | Ok(None) => LOCO_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}
