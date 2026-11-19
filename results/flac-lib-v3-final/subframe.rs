//! One channel of one frame: ports of `decode_subframe` and what it calls
//! in flacdec.c, and of the prediction loops in flacdsp.c.
//!
//! All sample arithmetic wraps, as the C code's does (it computes in
//! unsigned types, and FFmpeg's x86 assembly agrees with it bit for bit).
//!
//! Everything here is `#[inline(always)]`: the decoder is compiled twice,
//! once for any x86-64 CPU and once for AVX2, BMI and LZCNT, by inlining
//! it into two entry points (see `Decoder::decode_avx2`).

use std::cell::Cell;

use crate::bits::BitReader;
use crate::Invalid;

/// What decides between the two LPC loops.
#[derive(Clone, Copy)]
pub struct LpcRules {
    /// The stream's bits per sample (not the subframe's, which may be one
    /// more for a side channel and less by the wasted bits).
    pub stream_bps: i32,
    /// The `use_buggy_lpc` option.
    pub buggy_lpc: bool,
}

/// `decode_residuals`: the Rice-coded prediction residual of a subframe,
/// written to `out[pred_order..]`. `out` is one block long.
#[inline(always)]
fn decode_residuals(gb: &mut BitReader, out: &mut [i32], pred_order: usize) -> Result<(), Invalid> {
    let blocksize = out.len();
    let method_type = gb.get(2);
    let rice_order = gb.get(4);

    let samples = blocksize >> rice_order;
    let rice_bits = 4 + method_type;
    let rice_esc = (1 << rice_bits) - 1;

    if method_type > 1 {
        return Err(Invalid);
    }
    if samples << rice_order != blocksize {
        return Err(Invalid);
    }
    if pred_order > samples {
        return Err(Invalid);
    }

    // The first partition is short by the warm-up samples.
    let mut rest = &mut out[pred_order..];
    let mut len = samples - pred_order;
    for _ in 0..1usize << rice_order {
        let (part, after) = rest.split_at_mut(len);
        rest = after;
        len = samples;

        let tmp = gb.get(rice_bits);
        if tmp == rice_esc {
            let bits = gb.get(5);
            gb.read_signed_run(part, bits);
        } else {
            let real_limit = if tmp > 1 { (i32::MAX >> (tmp - 1)) + 2 } else { i32::MAX };
            gb.read_rice(part, tmp, real_limit as u32)?;
        }
    }
    Ok(())
}

/// The fixed predictors of orders 0 to 4, in place: `decode_subframe_fixed`
/// and `decode_subframe_fixed_wide` after the residual is read. The two C
/// functions compute the same thing modulo 2^32; this is the first one's
/// way, with running differences.
#[inline(always)]
fn fixed_predict(decoded: &mut [i32], order: usize) {
    let Some((warm, rest)) = decoded.split_at_mut_checked(order) else {
        return;
    };
    let w = |k: usize| warm[order - k];
    match order {
        1 => {
            let mut a = w(1);
            for x in rest {
                a = a.wrapping_add(*x);
                *x = a;
            }
        }
        2 => {
            let mut a = w(1);
            let mut b = a.wrapping_sub(w(2));
            for x in rest {
                b = b.wrapping_add(*x);
                a = a.wrapping_add(b);
                *x = a;
            }
        }
        3 => {
            let mut a = w(1);
            let mut b = a.wrapping_sub(w(2));
            let mut c = b.wrapping_sub(w(2)).wrapping_add(w(3));
            for x in rest {
                c = c.wrapping_add(*x);
                b = b.wrapping_add(c);
                a = a.wrapping_add(b);
                *x = a;
            }
        }
        4 => {
            let mut a = w(1);
            let mut b = a.wrapping_sub(w(2));
            let mut c = b.wrapping_sub(w(2)).wrapping_add(w(3));
            let mut d =
                c.wrapping_sub(w(2)).wrapping_add(w(3).wrapping_mul(2)).wrapping_sub(w(4));
            for x in rest {
                d = d.wrapping_add(*x);
                c = c.wrapping_add(d);
                b = b.wrapping_add(c);
                a = a.wrapping_add(b);
                *x = a;
            }
        }
        _ => {}
    }
}

/// `decode_subframe_fixed_33bps` after the warm-up samples and the residual
/// are read.
#[inline(always)]
fn fixed_predict_33(decoded: &mut [i64], residual: &[i32], order: usize) {
    for i in order..decoded.len().min(residual.len()) {
        let r = i64::from(residual[i]);
        let p = |k: usize| decoded[i - k];
        decoded[i] = match order {
            0 => r,
            1 => r.wrapping_add(p(1)),
            2 => r.wrapping_add(p(1).wrapping_mul(2)).wrapping_sub(p(2)),
            3 => r
                .wrapping_add(p(1).wrapping_mul(3))
                .wrapping_sub(p(2).wrapping_mul(3))
                .wrapping_add(p(3)),
            _ => r
                .wrapping_add(p(1).wrapping_mul(4))
                .wrapping_sub(p(2).wrapping_mul(6))
                .wrapping_add(p(3).wrapping_mul(4))
                .wrapping_sub(p(4)),
        };
    }
}

/// `flac_lpc_16_c` / `ff_flac_lpc_16_sse4` for one predictor order: the
/// sum is kept in 32 bits.
///
/// The samples are walked as overlapping windows of `N + 1` cells, the
/// last being the sample to finish, so no index needs a bounds check.
///
/// Each sample depends on the one before it, so the loop is as fast as
/// the path from one sample to the next: one multiplication, an addition,
/// a shift, an addition. The four newest samples are carried in variables
/// and the newest is added last to keep that path short.
#[inline(always)]
fn lpc16_n<const N: usize>(decoded: &mut [i32], coeffs: &[i32; N], qlevel: u32) {
    const RECENT: usize = 4;
    if decoded.len() <= N {
        return;
    }
    let old = N.saturating_sub(RECENT);
    // recent[t] is the sample t + 1 back and c[t] its coefficient; for
    // orders below four the spare ones are zero.
    let mut c = [0i32; RECENT];
    let mut recent = [0i32; RECENT];
    for t in 0..RECENT.min(N) {
        c[t] = coeffs[N - 1 - t];
        recent[t] = decoded[N - 1 - t];
    }
    let [c0, c1, c2, c3] = c;
    let [mut r0, mut r1, mut r2, mut r3] = recent;
    let cells = Cell::from_mut(decoded).as_slice_of_cells();
    for w in cells.windows(N + 1) {
        let mut sum = 0i32;
        for j in 0..old {
            sum = sum.wrapping_add(coeffs[j].wrapping_mul(w[j].get()));
        }
        sum = sum
            .wrapping_add(c3.wrapping_mul(r3))
            .wrapping_add(c2.wrapping_mul(r2))
            .wrapping_add(c1.wrapping_mul(r1));
        // The newest sample last: it is the one the loop waits for.
        sum = sum.wrapping_add(c0.wrapping_mul(r0));
        let v = w[N].get().wrapping_add(sum >> qlevel);
        w[N].set(v);
        (r3, r2, r1, r0) = (r2, r1, r0, v);
    }
}

/// `flac_lpc_32_c` / `ff_flac_lpc_32_sse4` for one predictor order: the
/// sum is kept in 64 bits.
#[inline(always)]
fn lpc32_n<const N: usize>(decoded: &mut [i32], coeffs: &[i32; N], qlevel: u32) {
    let cells = Cell::from_mut(decoded).as_slice_of_cells();
    for w in cells.windows(N + 1) {
        let mut sum = 0i64;
        for j in 0..N {
            sum += i64::from(coeffs[j]) * i64::from(w[j].get());
        }
        w[N].set(w[N].get().wrapping_add((sum >> qlevel) as i32));
    }
}

/// `flac_lpc_33_c` for one predictor order.
#[inline(always)]
fn lpc33_n<const N: usize>(decoded: &mut [i64], residual: &[i32], coeffs: &[i32; N], qlevel: u32) {
    let cells = Cell::from_mut(decoded).as_slice_of_cells();
    for (w, &r) in cells.windows(N + 1).zip(residual.iter().skip(N)) {
        let mut sum = 0i64;
        for j in 0..N {
            sum = sum.wrapping_add(i64::from(coeffs[j]).wrapping_mul(w[j].get()));
        }
        w[N].set(i64::from(r).wrapping_add(sum >> qlevel));
    }
}

/// Hands the orders 1 to 32 to another macro, which makes a match arm for
/// each: every order gets a loop of its own with the taps unrolled.
macro_rules! orders {
    ($arms:ident) => {
        $arms!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16
               17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32)
    };
}

/// Not inlined, unlike the rest: this way it is compiled once, for plain
/// x86-64, also when called from the AVX2 build of the decoder. With AVX2
/// allowed the compiler packs the products of a sample into one vector
/// multiplication (`vpmulld`, ten cycles) that then sits on the path from
/// each sample to the next; order 8 ran 1.3 times slower than FFmpeg's
/// assembly that way and runs 0.75 times as long without.
#[inline(never)]
fn lpc16(decoded: &mut [i32], coeffs: &[i32], qlevel: u32) {
    macro_rules! arms {
        ($($n:literal)*) => {
            match coeffs.len() {
                $($n => {
                    if let Ok(c) = <&[i32; $n]>::try_from(coeffs) {
                        lpc16_n::<$n>(decoded, c, qlevel)
                    }
                })*
                _ => {}
            }
        };
    }
    orders!(arms);
}

#[inline(always)]
fn lpc32(decoded: &mut [i32], coeffs: &[i32], qlevel: u32) {
    macro_rules! arms {
        ($($n:literal)*) => {
            match coeffs.len() {
                $($n => {
                    if let Ok(c) = <&[i32; $n]>::try_from(coeffs) {
                        lpc32_n::<$n>(decoded, c, qlevel)
                    }
                })*
                _ => {}
            }
        };
    }
    orders!(arms);
}

#[inline(always)]
fn lpc33(decoded: &mut [i64], residual: &[i32], coeffs: &[i32], qlevel: u32) {
    macro_rules! arms {
        ($($n:literal)*) => {
            match coeffs.len() {
                $($n => {
                    if let Ok(c) = <&[i32; $n]>::try_from(coeffs) {
                        lpc33_n::<$n>(decoded, residual, c, qlevel)
                    }
                })*
                _ => {}
            }
        };
    }
    orders!(arms);
}

/// `lpc_analyze_remodulate`: for streams of at most 16 bits whose
/// subframe went through the 64-bit loop. If any sample left the range of
/// `bps` bits, the prediction is undone and done again with 32-bit sums
/// (old encoders computed it that way).
#[inline(always)]
fn lpc_analyze_remodulate(decoded: &mut [i32], coeffs: &[i32], qlevel: u32, bps: i32) {
    let order = coeffs.len();
    let len = decoded.len();
    let ebps = 1u32 << (bps - 1);
    let mut sigma = 0u32;
    for &d in &decoded[order..] {
        sigma |= (d as u32).wrapping_add(ebps);
    }
    if sigma < 2 * ebps {
        return;
    }
    remodulate(decoded, coeffs, qlevel, order, len);
}

/// The rare second half of `lpc_analyze_remodulate`, kept out of line.
#[inline(never)]
fn remodulate(decoded: &mut [i32], coeffs: &[i32], qlevel: u32, order: usize, len: usize) {
    for i in (order..len).rev() {
        let mut p = 0i64;
        for j in 0..order {
            p += i64::from(coeffs[j]) * i64::from(decoded[i - order + j]);
        }
        decoded[i] = decoded[i].wrapping_sub((p >> qlevel) as i32);
    }
    for i in order..len {
        let mut sum = 0i32;
        for j in 0..order {
            sum = sum.wrapping_add(coeffs[j].wrapping_mul(decoded[i - order + j]));
        }
        decoded[i] = decoded[i].wrapping_add(sum >> qlevel);
    }
}

/// The coefficient precision, shift and coefficients of an LPC subframe.
/// `coeffs[order - 1]` applies to the previous sample.
#[inline(always)]
fn lpc_parameters(gb: &mut BitReader, coeffs: &mut [i32]) -> Result<(u32, u32), Invalid> {
    let coeff_prec = gb.get(4) + 1;
    if coeff_prec == 16 {
        return Err(Invalid);
    }
    let qlevel = gb.get_signed(5);
    if qlevel < 0 {
        return Err(Invalid);
    }
    for c in coeffs.iter_mut().rev() {
        *c = gb.get_signed(coeff_prec);
    }
    Ok((coeff_prec, qlevel as u32))
}

/// `decode_subframe_lpc`.
#[inline(always)]
fn decode_subframe_lpc(
    gb: &mut BitReader,
    decoded: &mut [i32],
    order: usize,
    bps: i32,
    rules: LpcRules,
) -> Result<(), Invalid> {
    // (The C code stores the warm-up samples before it finds that the
    // block is shorter than the predictor, and then fails as well.)
    if order > decoded.len() {
        return Err(Invalid);
    }
    for d in &mut decoded[..order] {
        *d = gb.get_signed(bps as u32);
    }
    let mut coeffs = [0i32; 32];
    let coeffs = &mut coeffs[..order];
    let (coeff_prec, qlevel) = lpc_parameters(gb, coeffs)?;
    decode_residuals(gb, decoded, order)?;

    let narrow = if rules.buggy_lpc {
        rules.stream_bps <= 16
    } else {
        bps <= 16 && bps + coeff_prec as i32 + order.ilog2() as i32 <= 32
    };
    if narrow {
        lpc16(decoded, coeffs, qlevel);
    } else {
        lpc32(decoded, coeffs, qlevel);
        if rules.stream_bps <= 16 {
            lpc_analyze_remodulate(decoded, coeffs, qlevel, bps);
        }
    }
    Ok(())
}

/// `decode_subframe_lpc_33bps`.
#[inline(always)]
fn decode_subframe_lpc_33(
    gb: &mut BitReader,
    decoded: &mut [i64],
    residual: &mut [i32],
    order: usize,
) -> Result<(), Invalid> {
    if order > decoded.len() {
        return Err(Invalid);
    }
    for d in &mut decoded[..order] {
        *d = gb.get_signed33();
    }
    let mut coeffs = [0i32; 32];
    let coeffs = &mut coeffs[..order];
    let (_, qlevel) = lpc_parameters(gb, coeffs)?;
    decode_residuals(gb, residual, order)?;
    lpc33(decoded, residual, coeffs, qlevel);
    Ok(())
}

/// `decode_subframe`: one channel. `bps` is the channel's sample size, 33
/// for the side channel of a 32-bit stream. `decoded` and `decoded_33` are
/// one block long; a 33-bit channel ends up in `decoded_33` and uses
/// `decoded` for its residual.
#[inline(always)]
pub fn decode_subframe(
    gb: &mut BitReader,
    decoded: &mut [i32],
    decoded_33: &mut [i64],
    mut bps: i32,
    rules: LpcRules,
) -> Result<(), Invalid> {
    if gb.get1() {
        return Err(Invalid);
    }
    let kind = gb.get(6);

    let mut wasted = 0;
    if gb.get1() {
        let left = gb.bits_left();
        if left <= 0
            || (left < i64::from(bps) && gb.peek(left as u32) == 0)
            || gb.peek(bps as u32 - 1) == 0
        {
            return Err(Invalid);
        }
        wasted = 1 + gb.get_unary(gb.bits_left()) as i32;
        bps -= wasted;
    }

    match kind {
        0 => {
            if bps < 33 {
                decoded.fill(gb.get_signed(bps as u32));
            } else {
                decoded_33.fill(gb.get_signed33());
            }
        }
        1 => {
            if bps < 33 {
                gb.read_signed_run(decoded, bps as u32);
            } else {
                for d in decoded_33.iter_mut() {
                    *d = gb.get_signed33();
                }
            }
        }
        8..=12 => {
            let order = (kind & 7) as usize;
            if order > decoded.len() {
                return Err(Invalid);
            }
            if bps < 33 {
                for d in &mut decoded[..order] {
                    *d = gb.get_signed(bps as u32);
                }
                decode_residuals(gb, decoded, order)?;
                fixed_predict(decoded, order);
            } else {
                for d in &mut decoded_33[..order] {
                    *d = gb.get_signed33();
                }
                decode_residuals(gb, decoded, order)?;
                fixed_predict_33(decoded_33, decoded, order);
            }
        }
        32..=63 => {
            let order = (kind & 31) as usize + 1;
            if bps < 33 {
                decode_subframe_lpc(gb, decoded, order, bps, rules)?;
            } else {
                decode_subframe_lpc_33(gb, decoded_33, decoded, order)?;
            }
        }
        _ => return Err(Invalid),
    }

    if wasted != 0 {
        if wasted + bps == 33 {
            // flac_wasted_33_c
            for (wide, &d) in decoded_33.iter_mut().zip(decoded.iter()) {
                *wide = ((i64::from(d) as u64) << wasted) as i64;
            }
        } else if wasted < 32 {
            // flac_wasted_32_c
            for d in decoded.iter_mut() {
                *d = ((*d as u32) << wasted) as i32;
            }
        }
    }
    Ok(())
}
