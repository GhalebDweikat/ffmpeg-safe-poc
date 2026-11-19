//! C entry points for the safe MagicYUV decoder.
//!
//! The decoder object holds the slice positions and Huffman tables of the
//! packet being decoded (and keeps their allocations from one packet to the
//! next); C holds it as an opaque pointer. A packet goes through three
//! calls, with the C side acting on the result in between, in the order
//! the C decoder does things: `header` (then the picture format and size
//! are set), `tables` (then the frame is allocated), `decode`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use magicyuv_core::{BadPlanes, Decoder, Error, Plane};

pub const MAGICYUV_RS_BAD_ARGS: i32 = -1;

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: turn the AVX2 predictors off (0) or back on, so that the
/// portable ones can be exercised on a CPU that has AVX2.
#[no_mangle]
pub extern "C" fn magicyuv_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

fn use_avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        USE_SIMD.load(Ordering::Relaxed) && std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

fn decode8(dec: &Decoder, pkt: &[u8], planes: &mut [Plane<'_, u8>]) -> Result<bool, BadPlanes> {
    #[cfg(target_arch = "x86_64")]
    if use_avx2() {
        // SAFETY: `decode8_avx2` is safe code compiled for AVX2; the only
        // requirement for calling it is that the CPU supports AVX2, which
        // was just checked.
        return unsafe { dec.decode8_avx2(pkt, planes) };
    }
    dec.decode8(pkt, planes)
}

fn decode16(dec: &Decoder, pkt: &[u8], planes: &mut [Plane<'_, u16>]) -> Result<bool, BadPlanes> {
    #[cfg(target_arch = "x86_64")]
    if use_avx2() {
        // SAFETY: as in `decode8`.
        return unsafe { dec.decode16_avx2(pkt, planes) };
    }
    dec.decode16(pkt, planes)
}

/// What `magicyuv_rs_header` found, or the number that goes with an error.
#[repr(C)]
pub struct MagicyuvRsHeader {
    /// The format code from the packet.
    pub format: i32,
    pub color_matrix: i32,
    pub flags: i32,
    pub width: i32,
    pub height: i32,
    /// For errors that are logged with a value.
    pub detail: i64,
}

/// Create a decoder. Returns null on failure.
#[no_mangle]
pub extern "C" fn magicyuv_rs_new() -> *mut Decoder {
    catch_unwind(|| Box::into_raw(Box::new(Decoder::new()))).unwrap_or(ptr::null_mut())
}

/// # Safety
/// `dec` must be null or a pointer from `magicyuv_rs_new` that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn magicyuv_rs_free(dec: *mut Decoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// The C side's number for an error, and the value it is logged with.
fn error_code(e: Error) -> (i32, i64) {
    match e {
        Error::Invalid => (1, 0),
        Error::HeaderSize(n) => (2, n as i64),
        Error::Version(v) => (3, v as i64),
        Error::Format(f) => (4, f as i64),
        Error::SliceWidth(w) => (5, w as i64),
        Error::SliceHeight(h) => (6, h as i64),
        Error::SliceHeightAlign(h) => (7, h as i64),
        Error::SliceCount(n) => (8, n as i64),
        Error::ImpossibleSliceHeight => (9, 0),
        Error::ImpossibleHeight => (10, 0),
        Error::HuffmanCodes => (11, 0),
        Error::HuffmanBuild => (12, 0),
        Error::HuffmanShort => (13, 0),
    }
}

/// The checks made before the picture format and size are set.
///
/// Returns 0 and fills `out`, or a positive error number (see
/// `error_code`; `out.detail` is then set), or a negative error.
///
/// # Safety
/// `dec` must be a live pointer from `magicyuv_rs_new`, not used by another
/// thread during the call; `pkt` must be valid for reads of `pkt_size`
/// bytes (or `pkt_size` is 0); `out` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn magicyuv_rs_header(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    out: *mut MagicyuvRsHeader,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return MAGICYUV_RS_BAD_ARGS;
    };
    if out.is_null() {
        return MAGICYUV_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let mut h = MagicyuvRsHeader { format: 0, color_matrix: 0, flags: 0, width: 0, height: 0, detail: 0 };
    let ret = match catch_unwind(AssertUnwindSafe(|| dec.header(pkt))) {
        Ok(Ok(header)) => {
            h.format = header.format.code as i32;
            h.color_matrix = header.color_matrix as i32;
            h.flags = header.flags as i32;
            h.width = header.width;
            h.height = header.height;
            0
        }
        Ok(Err(e)) => {
            let (code, detail) = error_code(e);
            h.detail = detail;
            code
        }
        Err(_) => crate::FFSAFE_PANIC,
    };
    out.write(h);
    ret
}

/// The rest of the checks, the slice positions and the Huffman tables.
/// `coded_width` and `coded_height` are the picture size as set from the
/// header.
///
/// Returns 0, or a positive error number (`*detail` is then set), or a
/// negative error.
///
/// # Safety
/// As for `magicyuv_rs_header`; `detail` must be null or valid for one
/// write.
#[no_mangle]
pub unsafe extern "C" fn magicyuv_rs_tables(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    coded_width: i32,
    coded_height: i32,
    detail: *mut i64,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return MAGICYUV_RS_BAD_ARGS;
    };
    let pkt = crate::bytes(pkt, pkt_size);
    match catch_unwind(AssertUnwindSafe(|| dec.tables(pkt, coded_width, coded_height))) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            let (code, value) = error_code(e);
            if !detail.is_null() {
                detail.write(value);
            }
            code
        }
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Decode a packet that passed `magicyuv_rs_tables` onto the visible
/// planes of a frame.
///
/// `data`, `len` and `linesize` have `nb_planes` entries, in the order the
/// planes are coded. `len[i]` is the number of bytes of plane `i` the
/// decoder may touch: `linesize[i] * (rows - 1)` plus one visible row.
///
/// Returns 0, or 1 if a slice asked for an unknown predictor (the frame is
/// still to be output), or a negative error.
///
/// # Safety
/// As for `magicyuv_rs_header`; in addition `data`, `len` and `linesize`
/// must be valid for reads of `nb_planes` entries, and each `data[i]` must
/// be valid for reads and writes of `len[i]` bytes, not overlapping the
/// others and not aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn magicyuv_rs_decode(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    data: *const *mut u8,
    len: *const usize,
    linesize: *const usize,
    nb_planes: i32,
) -> i32 {
    let Some(dec) = dec.as_ref() else {
        return MAGICYUV_RS_BAD_ARGS;
    };
    if data.is_null() || len.is_null() || linesize.is_null() || !(1..=4).contains(&nb_planes) {
        return MAGICYUV_RS_BAD_ARGS;
    }
    let n = nb_planes as usize;
    let (data, len, linesize) =
        (slice::from_raw_parts(data, n), slice::from_raw_parts(len, n), slice::from_raw_parts(linesize, n));
    // A length above isize::MAX cannot be a real buffer (and would make the
    // slices below invalid): it means the caller computed it wrongly.
    if data.iter().any(|p| p.is_null()) || len.iter().any(|&l| l > isize::MAX as usize) {
        return MAGICYUV_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);

    let result = if dec.bps() == 8 {
        let mut planes: [Plane<'_, u8>; 4] = std::array::from_fn(|i| match i < n {
            true => Plane { data: slice::from_raw_parts_mut(data[i], len[i]), stride: linesize[i] },
            false => Plane { data: &mut [], stride: 0 },
        });
        catch_unwind(AssertUnwindSafe(|| decode8(dec, pkt, &mut planes[..n])))
    } else {
        // 16-bit samples: the planes must be aligned and whole.
        if (0..n).any(|i| !(data[i] as usize).is_multiple_of(align_of::<u16>()) || len[i] % 2 != 0 || linesize[i] % 2 != 0)
        {
            return MAGICYUV_RS_BAD_ARGS;
        }
        let mut planes: [Plane<'_, u16>; 4] = std::array::from_fn(|i| match i < n {
            true => Plane {
                data: slice::from_raw_parts_mut(data[i].cast::<u16>(), len[i] / 2),
                stride: linesize[i] / 2,
            },
            false => Plane { data: &mut [], stride: 0 },
        });
        catch_unwind(AssertUnwindSafe(|| decode16(dec, pkt, &mut planes[..n])))
    };
    match result {
        Ok(Ok(unknown_predictor)) => i32::from(unknown_predictor),
        Ok(Err(_)) => MAGICYUV_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use magicyuv_core::pred;

    fn random(seed: &mut u64, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (*seed >> 56) as u8
            })
            .collect()
    }

    /// The AVX2 predictors and the portable ones agree on every row
    /// length from 1 to 150 samples.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx2_matches_portable() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for width in 1..=150 {
            let row = random(&mut seed, width);
            let top = random(&mut seed, width);
            let acc = random(&mut seed, 1)[0];

            let (mut a, mut b) = (row.clone(), row.clone());
            pred::left8(&mut a, acc);
            // SAFETY: AVX2 was detected above (here and below).
            unsafe { pred::avx2::left8(&mut b, acc) };
            assert_eq!(a, b, "left, width {width}");

            let (mut a, mut b) = (row.clone(), row.clone());
            pred::gradient8(&mut a, &top);
            unsafe { pred::avx2::gradient8(&mut b, &top) };
            assert_eq!(a, b, "gradient, width {width}");

            let (mut a, mut b) = (row.clone(), row.clone());
            pred::median8(&mut a, &top);
            unsafe { pred::avx2::median8(&mut b, &top) };
            assert_eq!(a, b, "median, width {width}");

            // 16 bits, including samples above the mask (as left behind
            // by a slice that ended early).
            let wide: Vec<u16> = random(&mut seed, 2 * width).chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            for mask in [0x3ff, 0xfff, 0x3fff] {
                let acc = wide[0] ^ 0x5555;
                let (mut a, mut b) = (wide.clone(), wide.clone());
                pred::left16(&mut a, mask, acc);
                unsafe { pred::avx2::left16(&mut b, mask, acc) };
                assert_eq!(a, b, "left 16, width {width}");
            }
        }
    }
}
