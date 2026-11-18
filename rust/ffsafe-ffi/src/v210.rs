//! C entry points for the safe v210 decoder.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;

use std::sync::atomic::{AtomicBool, Ordering};

use v210_core::{decode, layout, Error, Layout, Planes};

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: turn the AVX2 decoder off (0) or back on, so that the
/// portable decoder can be exercised on a CPU that has AVX2.
#[no_mangle]
pub extern "C" fn v210_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

pub const V210_RS_BAD_ARGS: i32 = -1;

#[repr(C)]
pub struct V210RsLayout {
    pub stride: usize,
    pub offset: usize,
    /// Non-zero if the stride was guessed from the packet size; the value
    /// is the padding unit in bytes.
    pub small_padding: u32,
}

/// Work out where the rows are in a packet.
///
/// Returns 0, or 1 if unpadded rows are not supported at this size, or 2
/// if the packet is too small; or a negative error.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// and `out` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn v210_rs_layout(
    pkt: *const u8,
    pkt_size: usize,
    width: i32,
    height: i32,
    codec_tag: u32,
    custom_stride: i32,
    out: *mut V210RsLayout,
) -> i32 {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return V210_RS_BAD_ARGS;
    };
    if out.is_null() {
        return V210_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    match catch_unwind(|| layout(pkt, width, height, codec_tag, custom_stride)) {
        Ok(Ok(l)) => {
            out.write(V210RsLayout {
                stride: l.stride,
                offset: l.offset,
                small_padding: l.small_padding.map_or(0, |p| p as u32),
            });
            0
        }
        Ok(Err(Error::StridelessUnsupported)) => 1,
        Ok(Err(Error::PacketTooSmall)) => 2,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// # Safety
/// `ptr` must be valid for reads and writes of `len` 16-bit samples and not
/// aliased; it is checked for alignment here.
unsafe fn samples<'a>(ptr: *mut u8, len: usize) -> Option<&'a mut [u16]> {
    if ptr.is_null() || !(ptr as usize).is_multiple_of(align_of::<u16>()) {
        return None;
    }
    Some(slice::from_raw_parts_mut(ptr.cast::<u16>(), len))
}

/// Unpack a packet into three planes of 16-bit samples.
///
/// Lengths and strides are in samples. Each plane's length is what the
/// decoder may touch: `stride * (height - 1)` plus one visible row.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes; `y`, `u` and `v` must
/// be valid for reads and writes of `y_len`, `c_len` and `c_len` samples,
/// must not overlap, and must not be aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn v210_rs_decode(
    pkt: *const u8,
    pkt_size: usize,
    stride: usize,
    offset: usize,
    y: *mut u8,
    y_len: usize,
    y_stride: usize,
    u: *mut u8,
    v: *mut u8,
    c_len: usize,
    c_stride: usize,
    width: i32,
    height: i32,
) -> i32 {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return V210_RS_BAD_ARGS;
    };
    let (Some(y), Some(u), Some(v)) = (samples(y, y_len), samples(u, c_len), samples(v, c_len))
    else {
        return V210_RS_BAD_ARGS;
    };
    let pkt = crate::bytes(pkt, pkt_size);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut planes = Planes::new(y, u, v, y_stride, c_stride, width, height)?;
        let layout = Layout { stride, offset, small_padding: None };
        #[cfg(target_arch = "x86_64")]
        if USE_SIMD.load(Ordering::Relaxed) && std::is_x86_feature_detected!("avx2") {
            // SAFETY: `decode_avx2` is safe code compiled for AVX2; the
            // only requirement for calling it is that the CPU supports
            // AVX2, which was just checked.
            unsafe { v210_core::decode_avx2(pkt, &layout, &mut planes) };
            return Some(());
        }
        decode(pkt, &layout, &mut planes);
        Some(())
    }));
    match result {
        Ok(Some(())) => 0,
        Ok(None) => V210_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use v210_core::{decode, Layout, Planes};

    /// The AVX2 decoder and the portable one agree on every row length
    /// from 1 to 140 pixels.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx2_matches_portable() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut src = [0u8; 400];
        for b in src.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (seed >> 56) as u8;
        }
        for width in 1..=140 {
            let cw = (width + 1) / 2;
            let (mut y1, mut u1, mut v1) = ([0u16; 140], [0u16; 70], [0u16; 70]);
            let (mut y2, mut u2, mut v2) = ([0u16; 140], [0u16; 70], [0u16; 70]);
            let l = Layout { stride: 400, offset: 0, small_padding: None };
            let mut p1 = Planes::new(&mut y1, &mut u1, &mut v1, width, cw, width, 1).unwrap();
            let mut p2 = Planes::new(&mut y2, &mut u2, &mut v2, width, cw, width, 1).unwrap();
            decode(&src, &l, &mut p1);
            // SAFETY: AVX2 was detected above.
            unsafe { v210_core::decode_avx2(&src, &l, &mut p2) };
            assert_eq!((y1, u1, v1), (y2, u2, v2), "width {width}");
        }
    }
}
