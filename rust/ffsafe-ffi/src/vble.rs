//! C entry points for the safe VBLE decoder.
//!
//! The decoder keeps a values buffer as large as a picture, so it lives in
//! a heap allocation that C holds as an opaque pointer.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use vble_core::{Decoder, Error, Options, Picture, Plane};

static USE_SIMD: AtomicBool = AtomicBool::new(true);

/// Test hook: turn the SSE2 predictor off (0) or back on, so that the
/// portable one can be exercised on x86-64.
#[no_mangle]
pub extern "C" fn vble_rs_set_simd(enabled: i32) {
    USE_SIMD.store(enabled != 0, Ordering::Relaxed);
}

pub const VBLE_RS_BAD_ARGS: i32 = -1;

/// `vble_rs_restore` flag: leave the chroma planes alone (`AV_CODEC_FLAG_GRAY`).
pub const VBLE_RS_GRAY: u32 = 1;
/// `vble_rs_restore` flag: FFmpeg's median predictor is the SSE2 routine,
/// which also overwrites the sample after the end of a row.
pub const VBLE_RS_SSE2_OVERWRITE: u32 = 2;

/// Create a decoder. Returns null on failure (no memory, or an empty picture).
#[no_mangle]
pub extern "C" fn vble_rs_new(width: i32, height: i32) -> *mut Decoder {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return ptr::null_mut();
    };
    match catch_unwind(|| Decoder::new(width, height)) {
        Ok(Some(dec)) => Box::into_raw(Box::new(dec)),
        _ => ptr::null_mut(),
    }
}

/// # Safety
/// `dec` must be null or a pointer from `vble_rs_new` that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn vble_rs_free(dec: *mut Decoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// The number of codes the decoder reads from every packet.
///
/// # Safety
/// `dec` must be null or a live pointer from `vble_rs_new`.
#[no_mangle]
pub unsafe extern "C" fn vble_rs_symbols(dec: *const Decoder) -> usize {
    dec.as_ref().map_or(0, Decoder::symbols)
}

/// Read the code lengths of a packet: everything the C decoder does before
/// it asks for a frame.
///
/// Returns 0 to go on, 1 for a packet of invalid size, 2 for an invalid
/// code; or a negative error. On 0 and 2, `version` receives the packet's
/// version number.
///
/// # Safety
/// `dec` must be a live pointer from `vble_rs_new`, not used by another
/// thread during the call; `pkt` must be valid for reads of `pkt_size`
/// bytes (or `pkt_size` is 0); `version` must be null or valid for one
/// write.
#[no_mangle]
pub unsafe extern "C" fn vble_rs_unpack(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    version: *mut u32,
) -> i32 {
    let Some(dec) = dec.as_mut() else {
        return VBLE_RS_BAD_ARGS;
    };
    let pkt = crate::bytes(pkt, pkt_size);
    if let (false, Some(v)) = (version.is_null(), vble_core::version(pkt)) {
        version.write(v);
    }
    match catch_unwind(AssertUnwindSafe(|| dec.unpack(pkt))) {
        Ok(Ok(())) => 0,
        Ok(Err(Error::PacketSize)) => 1,
        Ok(Err(Error::InvalidCode)) => 2,
        Ok(Err(Error::BadCall)) => VBLE_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

/// Rebuild the picture from the packet that was last given to
/// `vble_rs_unpack` (and accepted).
///
/// Each plane's length is what the decoder may touch: `stride * (rows - 1)`
/// plus one visible row; chroma is `(width + 1) / 2` by `(height + 1) / 2`.
///
/// # Safety
/// As for `vble_rs_unpack`; in addition `y`, `u` and `v` must be valid for
/// reads and writes of `y_len`, `u_len` and `v_len` bytes, must not
/// overlap, and must not be aliased during the call.
#[no_mangle]
pub unsafe extern "C" fn vble_rs_restore(
    dec: *mut Decoder,
    pkt: *const u8,
    pkt_size: usize,
    y: *mut u8,
    y_len: usize,
    y_stride: usize,
    u: *mut u8,
    u_len: usize,
    u_stride: usize,
    v: *mut u8,
    v_len: usize,
    v_stride: usize,
    width: i32,
    height: i32,
    flags: u32,
) -> i32 {
    let (Some(dec), Ok(width), Ok(height)) =
        (dec.as_mut(), usize::try_from(width), usize::try_from(height))
    else {
        return VBLE_RS_BAD_ARGS;
    };
    if y.is_null() || u.is_null() || v.is_null() {
        return VBLE_RS_BAD_ARGS;
    }
    let pkt = crate::bytes(pkt, pkt_size);
    let y = slice::from_raw_parts_mut(y, y_len);
    let u = slice::from_raw_parts_mut(u, u_len);
    let v = slice::from_raw_parts_mut(v, v_len);
    let opts = Options {
        gray: flags & VBLE_RS_GRAY != 0,
        sse2_overwrite: flags & VBLE_RS_SSE2_OVERWRITE != 0,
    };

    let result = catch_unwind(AssertUnwindSafe(|| {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let mut pic = Picture {
            y: Plane::new(y, y_stride, width, height)?,
            u: Plane::new(u, u_stride, cw, ch)?,
            v: Plane::new(v, v_stride, cw, ch)?,
        };
        #[cfg(target_arch = "x86_64")]
        if USE_SIMD.load(Ordering::Relaxed) && std::is_x86_feature_detected!("sse2") {
            // SAFETY: `restore_sse2` is safe code compiled for SSE2; the
            // only requirement for calling it is that the CPU supports
            // SSE2, which was just checked.
            return unsafe { dec.restore_sse2(pkt, &mut pic, opts) }.ok();
        }
        dec.restore(pkt, &mut pic, opts).ok()
    }));
    match result {
        Ok(Some(())) => 0,
        Ok(None) => VBLE_RS_BAD_ARGS,
        Err(_) => crate::FFSAFE_PANIC,
    }
}

#[cfg(test)]
mod tests {
    use vble_core::{Decoder, Options, Picture, Plane};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 40) as u32
        }
    }

    /// A packet of `n` codes: `flat` makes them all zero, otherwise their
    /// lengths are spread over 0 to 8 and their bits are random.
    fn packet(n: usize, flat: bool, rng: &mut Rng) -> Vec<u8> {
        let (mut out, mut acc, mut fill) = (vec![1, 0, 0, 0], 0u64, 0);
        let mut put = |v: u32, bits: u32, out: &mut Vec<u8>| {
            acc |= u64::from(v) << fill;
            fill += bits;
            while fill >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                fill -= 8;
            }
        };
        let lens: Vec<u32> = (0..n).map(|_| if flat { 0 } else { rng.next() % 9 }).collect();
        for &len in &lens {
            put(1 << len, len + 1, &mut out);
        }
        for &len in &lens {
            put(rng.next() & ((1 << len) - 1), len, &mut out);
        }
        put(0, 7, &mut out);
        out
    }

    type Planes = (Vec<u8>, Vec<u8>, Vec<u8>);

    fn decode(dec: &mut Decoder, pkt: &[u8], w: usize, h: usize, opts: Options, sse2: bool) -> Planes {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (mut y, mut u, mut v) = (vec![0x55; w * h], vec![0x55; cw * ch], vec![0x55; cw * ch]);
        let mut pic = Picture {
            y: Plane::new(&mut y, w, w, h).unwrap(),
            u: Plane::new(&mut u, cw, cw, ch).unwrap(),
            v: Plane::new(&mut v, cw, cw, ch).unwrap(),
        };
        dec.unpack(pkt).unwrap();
        if sse2 {
            // SAFETY: the callers checked that the CPU has SSE2.
            unsafe { dec.restore_sse2(pkt, &mut pic, opts) }.unwrap();
        } else {
            dec.restore(pkt, &mut pic, opts).unwrap();
        }
        (y, u, v)
    }

    /// The SSE2 predictor and the portable one agree, at sizes with one
    /// band and several, full and partial, even and odd.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn sse2_matches_portable() {
        if !std::is_x86_feature_detected!("sse2") {
            return;
        }
        let mut rng = Rng(3);
        for (w, h) in [(1, 1), (1, 9), (2, 2), (3, 5), (16, 15), (17, 16), (33, 31), (40, 46), (131, 20), (64, 64)] {
            let mut dec = Decoder::new(w, h).unwrap();
            for sse2_overwrite in [false, true] {
                let pkt = packet(dec.symbols(), false, &mut rng);
                let opts = Options { gray: false, sse2_overwrite };
                let portable = decode(&mut dec, &pkt, w, h, opts, false);
                let sse2 = decode(&mut dec, &pkt, w, h, opts, true);
                assert!(portable == sse2, "{w}x{h}");
            }
        }
    }

    /// Not a test: prints how long the stages take on a 1280x720 frame.
    /// `cargo test --release -p ffsafe-ffi --no-default-features --features vble -- --ignored --nocapture`
    #[test]
    #[ignore]
    #[cfg(target_arch = "x86_64")]
    fn timing() {
        use std::time::Instant;
        let (w, h) = (1280, 720);
        let mut rng = Rng(7);
        let mut dec = Decoder::new(w, h).unwrap();
        for flat in [false, true] {
            let pkt = packet(dec.symbols(), flat, &mut rng);
            let mut best = [f64::MAX; 3];
            for _ in 0..40 {
                let t0 = Instant::now();
                dec.unpack(&pkt).unwrap();
                let t1 = Instant::now();
                let a = decode(&mut dec, &pkt, w, h, Options::default(), false);
                let t2 = Instant::now();
                let b = decode(&mut dec, &pkt, w, h, Options::default(), true);
                let t3 = Instant::now();
                assert!(a == b);
                for (b, t) in best.iter_mut().zip([t1 - t0, t2 - t1, t3 - t2]) {
                    *b = b.min(t.as_secs_f64() * 1e6);
                }
            }
            // The two decodes include a second unpack and three allocations.
            println!(
                "flat={flat}: {} bytes, unpack {:.0} us, unpack + restore {:.0} us portable, {:.0} us SSE2",
                pkt.len(),
                best[0],
                best[1],
                best[2]
            );
        }
    }
}
