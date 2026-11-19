//! C entry points for the safe Truevision Targa image decoder.
//!
//! A Targa packet carries its own picture size and pixel format, so there
//! are two calls: `targa_rs_header` tells the shim what frame to allocate,
//! and `targa_rs_decode` fills it. The second call parses the header again
//! and checks the frame it is given against it; it does not rely on the
//! shim having passed a frame of the right size.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use targa_core::{decode, parse, DecodeError, Format, Frame, HeaderError};

/// Results of `targa_rs_header`, in the order of the C decoder's checks.
pub const TARGA_RS_HDR_DECODE: i32 = 0;
/// The picture type is "no data": set the dimensions, output nothing.
pub const TARGA_RS_HDR_NO_DATA: i32 = 1;
/// Uncompressed picture, larger than the packet: refuse after setting the
/// dimensions.
pub const TARGA_RS_HDR_IMAGE_SHORT: i32 = 2;
/// Refuse; `info` holds nothing.
pub const TARGA_RS_HDR_SHORT: i32 = -1;
pub const TARGA_RS_HDR_DEPTH: i32 = -2;
/// Refuse, after setting the pixel format; `info.format` is valid.
pub const TARGA_RS_HDR_PALETTE: i32 = -3;

/// The frame passed to `targa_rs_decode` does not fit the packet.
pub const TARGA_RS_BAD_FRAME: i32 = -1;

#[repr(C)]
pub struct TargaRsInfo {
    /// 0 gray8, 1 pal8, 2 rgb555le, 3 bgr24, 4 bgra.
    pub format: i32,
    pub width: i32,
    pub height: i32,
    /// The header describes a colour map it does not have (a warning).
    pub colormap_ignored: i32,
}

fn format_code(f: Format) -> i32 {
    match f {
        Format::Gray8 => 0,
        Format::Pal8 => 1,
        Format::Rgb555Le => 2,
        Format::Bgr24 => 3,
        Format::Bgra => 4,
    }
}

/// Parse the header of one packet.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// and `info` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn targa_rs_header(
    pkt: *const u8,
    pkt_size: usize,
    info: *mut TargaRsInfo,
) -> i32 {
    if info.is_null() {
        return crate::FFSAFE_PANIC;
    }
    let pkt = crate::bytes(pkt, pkt_size);

    let result = catch_unwind(|| {
        let parsed = parse(pkt);
        let mut out = TargaRsInfo {
            format: -1,
            width: 0,
            height: 0,
            colormap_ignored: i32::from(parsed.colormap_ignored),
        };
        let status = match parsed.header {
            Err(HeaderError::NotEnoughData) => TARGA_RS_HDR_SHORT,
            Err(HeaderError::BitDepth) => TARGA_RS_HDR_DEPTH,
            Err(HeaderError::Palette { format }) => {
                out.format = format_code(format);
                TARGA_RS_HDR_PALETTE
            }
            Ok(hdr) => {
                out.format = format_code(hdr.format);
                // Both are 16-bit fields.
                out.width = hdr.width as i32;
                out.height = hdr.height as i32;
                if hdr.no_data() {
                    TARGA_RS_HDR_NO_DATA
                } else if !hdr.data_present() {
                    TARGA_RS_HDR_IMAGE_SHORT
                } else {
                    TARGA_RS_HDR_DECODE
                }
            }
        };
        (status, out)
    });

    match result {
        Ok((status, out)) => {
            info.write(out);
            status
        }
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Decode one packet into the visible picture at `frame`.
///
/// `frame_size` is the number of bytes the decoder may touch: `stride *
/// (height - 1)` plus one visible row. `pal` is the palette of a pal8 frame
/// (256 native-endian 32-bit entries), or null for the other formats.
///
/// Returns 0 when the picture is complete; 1 to 5 when the packet is
/// refused (bad colour map entry size, colour map cut short, uncompressed
/// picture cut short, compressed picture cut short, compressed picture too
/// long), in which case the frame may have been written in part;
/// `TARGA_RS_BAD_FRAME`; or `FFSAFE_PANIC`.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0);
/// `frame` must be valid for reads and writes of `frame_size` bytes, `pal`
/// must be null or valid for reads and writes of 1024 bytes, and neither
/// may be aliased for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn targa_rs_decode(
    pkt: *const u8,
    pkt_size: usize,
    frame: *mut u8,
    frame_size: usize,
    stride: usize,
    pal: *mut u8,
) -> i32 {
    if frame.is_null() {
        return TARGA_RS_BAD_FRAME;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let pixels = slice::from_raw_parts_mut(frame, frame_size);
    let pal = if pal.is_null() { None } else { Some(&mut *pal.cast::<[u8; 1024]>()) };

    let result = catch_unwind(AssertUnwindSafe(|| {
        let hdr = parse(pkt).header.ok()?;
        let mut frame = Frame::new(pixels, stride, pal, &hdr)?;
        Some(decode(&hdr, &mut frame))
    }));

    match result {
        Ok(Some(Ok(()))) => 0,
        Ok(Some(Err(e))) => match e {
            DecodeError::PaletteEntrySize => 1,
            DecodeError::PaletteData => 2,
            DecodeError::ImageData => 3,
            DecodeError::RleOutOfData => 4,
            DecodeError::RleOutOfBounds => 5,
        },
        Ok(None) => TARGA_RS_BAD_FRAME,
        Err(_) => crate::FFSAFE_PANIC,
    }
}
