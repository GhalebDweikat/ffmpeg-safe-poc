//! C entry points for the safe Apple ProRes decoder.
//!
//! The decoder keeps no state between frames, so there is no object to
//! own: one call reads the frame header, a second decodes the packet into
//! the picture the shim has set up from it.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use prores_core::{decode_frame, Error, FrameHeader, HeaderError, Idct, Options, Picture, Plane};

pub const PRORES_RS_BAD_ARGS: i32 = -1;

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: turn the AVX2 decoder off (0) or back on, so that the
/// portable decoder can be exercised on a CPU that has AVX2.
#[no_mangle]
pub extern "C" fn prores_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

/// What the shim needs from the frame header.
#[repr(C)]
pub struct ProresRsHeader {
    pub width: i32,
    pub height: i32,
    /// 0 progressive, 1 interlaced top field first, 2 and 3 interlaced
    /// bottom field first.
    pub frame_type: u8,
    /// 0 none, 1 and 2 alpha of 8 and 16 bits, above 2 invalid.
    pub alpha_info: u8,
    pub chroma444: u8,
    pub color_primaries: u8,
    pub color_trc: u8,
    pub colorspace: u8,
    /// A quantisation matrix is announced but not there.
    pub truncated: u8,
}

/// Read the frame header.
///
/// Returns 0, or 1 if this is not a usable ProRes frame, or 2 for an
/// unsupported bitstream version; or a negative error.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes (or `pkt_size` is 0),
/// and `out` must be valid for one write.
#[no_mangle]
pub unsafe extern "C" fn prores_rs_header(
    pkt: *const u8,
    pkt_size: usize,
    out: *mut ProresRsHeader,
) -> i32 {
    if out.is_null() {
        return PRORES_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    match catch_unwind(|| FrameHeader::parse(pkt)) {
        Ok(Ok(h)) => {
            out.write(ProresRsHeader {
                width: h.width as i32,
                height: h.height as i32,
                frame_type: h.frame_type,
                alpha_info: h.alpha_info,
                chroma444: u8::from(h.chroma444),
                color_primaries: h.color_primaries,
                color_trc: h.color_trc,
                colorspace: h.colorspace,
                truncated: u8::from(h.truncated),
            });
            0
        }
        Ok(Err(HeaderError::Invalid)) => 1,
        Ok(Err(HeaderError::UnsupportedVersion)) => 2,
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

/// Decode a packet whose header `prores_rs_header` accepted into a picture
/// of the header's size and layout.
///
/// `planes`, `lens` and `strides` describe luma, Cb, Cr and alpha, in
/// 16-bit samples; a plane's length is what the decoder may touch:
/// `stride * (height - 1)` plus one visible row. The alpha plane is null
/// when the stream has no alpha or it is skipped.
///
/// `idct` selects the inverse DCT and with it the sample depth: 0 for
/// FFmpeg's C code for 10 bits, 1 for its x86 assembly for 10 bits, 2 for
/// its C code for 12 bits.
///
/// Returns 0 for a frame, 1 for a frame in which some slices could not be
/// decoded, 2 for invalid data, 3 for a picture without slices, 4 if every
/// slice of a picture failed; or a negative error.
///
/// # Safety
/// `pkt` must be valid for reads of `pkt_size` bytes. `planes`, `lens` and
/// `strides` must each point to four elements. Every non-null plane must
/// be valid for reads and writes of its length in samples; the planes must
/// not overlap and must not be aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn prores_rs_decode(
    pkt: *const u8,
    pkt_size: usize,
    planes: *const *mut u8,
    lens: *const usize,
    strides: *const usize,
    idct: i32,
    skip_alpha: i32,
    gray: i32,
    top_field_first: i32,
) -> i32 {
    if planes.is_null() || lens.is_null() || strides.is_null() {
        return PRORES_RS_BAD_ARGS;
    }
    let planes = slice::from_raw_parts(planes, 4);
    let lens = slice::from_raw_parts(lens, 4);
    let strides = slice::from_raw_parts(strides, 4);
    let idct = match idct {
        0 => Idct::C10,
        1 => Idct::Asm10,
        2 => Idct::C12,
        _ => return PRORES_RS_BAD_ARGS,
    };
    let (Some(y), Some(u), Some(v)) =
        (samples(planes[0], lens[0]), samples(planes[1], lens[1]), samples(planes[2], lens[2]))
    else {
        return PRORES_RS_BAD_ARGS;
    };
    let a = if planes[3].is_null() {
        None
    } else {
        match samples(planes[3], lens[3]) {
            Some(a) => Some(a),
            None => return PRORES_RS_BAD_ARGS,
        }
    };
    let pkt = crate::bytes(pkt, pkt_size);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let hdr = FrameHeader::parse(pkt).ok()?;
        let (w, h) = (hdr.width, hdr.height);
        let cw = if hdr.chroma444 { w } else { w.div_ceil(2) };
        let mut pic = Picture {
            luma: Plane::new(y, strides[0], w, h)?,
            cb: Plane::new(u, strides[1], cw, h)?,
            cr: Plane::new(v, strides[2], cw, h)?,
            alpha: match a {
                Some(a) => Some(Plane::new(a, strides[3], w, h)?),
                None => None,
            },
        };
        let opts = Options {
            idct,
            skip_alpha: skip_alpha != 0,
            gray: gray != 0,
            top_field_first: top_field_first != 0,
        };
        #[cfg(target_arch = "x86_64")]
        if USE_SIMD.load(Ordering::Relaxed) && std::is_x86_feature_detected!("avx2") {
            // SAFETY: `decode_frame_avx2` is safe code compiled for AVX2;
            // the only requirement for calling it is that the CPU supports
            // AVX2, which was just checked.
            return Some(unsafe { prores_core::decode_frame_avx2(pkt, &hdr, &opts, &mut pic) });
        }
        Some(decode_frame(pkt, &hdr, &opts, &mut pic))
    }));
    match result {
        Ok(Some(Ok(d))) => i32::from(d.slice_errors),
        Ok(Some(Err(Error::InvalidData))) => 2,
        Ok(Some(Err(Error::NoSlices))) => 3,
        Ok(Some(Err(Error::AllSlicesFailed))) => 4,
        Ok(Some(Err(Error::Layout))) | Ok(None) => PRORES_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use prores_core::{idct_block, Idct, PERM};

    /// Blocks of every kind: sparse, dense, small and extreme values, with
    /// small and large quantisers.
    fn test_blocks(mut each: impl FnMut(&[i16; 64], &[i16; 64])) {
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for case in 0..200_000usize {
            let mut b = [0i16; 64];
            let mut q = [0i16; 64];
            let density = [1, 2, 8, 64][case % 4];
            let range = [16u32, 600, 5000, 65536][(case / 4) % 4];
            for v in b.iter_mut() {
                if rnd() % 64 < density {
                    *v = (rnd() % range) as i16 - (range / 2) as i16;
                }
            }
            if case % 16 >= 12 {
                // Values on the wrap-around points of the 16-bit sums.
                for _ in 0..3 {
                    let v = [32767, -32768, 32766, 24575, 24568, -32767, 16384, 24567];
                    b[(rnd() % 64) as usize] = v[(rnd() % 8) as usize];
                }
            }
            let scale = [1u32, 4, 16, 3000][(case / 16) % 4];
            for v in q.iter_mut() {
                *v = (1 + rnd() % scale) as i16;
            }
            if case % 7 == 0 {
                q = [1; 64];
            }
            each(&b, &q);
        }
    }

    /// The AVX2 transform and the portable one agree, for each of the
    /// three inverse DCTs.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx2_matches_portable() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        for kind in [Idct::C10, Idct::Asm10, Idct::C12] {
            test_blocks(|b, q| {
                // SAFETY: AVX2 was detected above.
                let simd = unsafe { prores_core::idct_block_avx2(kind, b, q) };
                assert_eq!(simd, idct_block(kind, b, q), "{kind:?} {b:?} {q:?}");
            });
        }
    }

    /// The first coefficient of a stored block is the DC coefficient.
    #[test]
    fn dc_is_stored_first() {
        assert_eq!(PERM[0], 0);
    }
}
