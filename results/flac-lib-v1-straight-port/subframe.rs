//! One channel of one frame: ports of `decode_subframe` and what it calls
//! in flacdec.c, and of the prediction loops in flacdsp.c.
//!
//! All sample arithmetic wraps, as the C code's does (it computes in
//! unsigned types, and FFmpeg's x86 assembly agrees with it bit for bit).

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

    let mut pos = pred_order;
    let mut i = pred_order;
    for _ in 0..1usize << rice_order {
        let mut tmp = gb.get(rice_bits);
        if tmp == rice_esc {
            tmp = gb.get(5);
            while i < samples {
                out[pos] = gb.get_signed(tmp);
                pos += 1;
                i += 1;
            }
        } else {
            let real_limit = if tmp > 1 { (i32::MAX >> (tmp - 1)) + 2 } else { i32::MAX };
            while i < samples {
                // get_sr_golomb_flac
                let v = gb.get_ur_golomb(tmp, real_limit as u32).unwrap_or(u32::MAX);
                let v = ((v >> 1) ^ (v & 1).wrapping_neg()) as i32;
                if v == i32::MIN {
                    return Err(Invalid);
                }
                out[pos] = v;
                pos += 1;
                i += 1;
            }
        }
        i = 0;
    }
    Ok(())
}

/// The fixed predictors of orders 0 to 4, in place: `decode_subframe_fixed`
/// and `decode_subframe_fixed_wide` after the residual is read. The two C
/// functions compute the same thing modulo 2^32.
fn fixed_predict(decoded: &mut [i32], order: usize) {
    for i in order..decoded.len() {
        let r = decoded[i];
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

/// `decode_subframe_fixed_33bps` after the warm-up samples and the residual
/// are read.
fn fixed_predict_33(decoded: &mut [i64], residual: &[i32], order: usize) {
    for i in order..decoded.len() {
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

/// `flac_lpc_16_c` / `ff_flac_lpc_16_sse4`: the sum is kept in 32 bits.
fn lpc16(decoded: &mut [i32], coeffs: &[i32], qlevel: u32) {
    let order = coeffs.len();
    for i in order..decoded.len() {
        let mut sum = 0i32;
        for j in 0..order {
            sum = sum.wrapping_add(coeffs[j].wrapping_mul(decoded[i - order + j]));
        }
        decoded[i] = decoded[i].wrapping_add(sum >> qlevel);
    }
}

/// `flac_lpc_32_c` / `ff_flac_lpc_32_sse4`: the sum is kept in 64 bits.
fn lpc32(decoded: &mut [i32], coeffs: &[i32], qlevel: u32) {
    let order = coeffs.len();
    for i in order..decoded.len() {
        let mut sum = 0i64;
        for j in 0..order {
            sum += i64::from(coeffs[j]) * i64::from(decoded[i - order + j]);
        }
        decoded[i] = decoded[i].wrapping_add((sum >> qlevel) as i32);
    }
}

/// `flac_lpc_33_c`.
fn lpc33(decoded: &mut [i64], residual: &[i32], coeffs: &[i32], qlevel: u32) {
    let order = coeffs.len();
    for i in order..decoded.len() {
        let mut sum = 0i64;
        for j in 0..order {
            sum = sum.wrapping_add(i64::from(coeffs[j]).wrapping_mul(decoded[i - order + j]));
        }
        decoded[i] = i64::from(residual[i]).wrapping_add(sum >> qlevel);
    }
}

/// `lpc_analyze_remodulate`: for streams of at most 16 bits whose
/// subframe went through the 64-bit loop. If any sample left the range of
/// `bps` bits, the prediction is undone and done again with 32-bit sums
/// (old encoders computed it that way).
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

    for i in (order..len).rev() {
        let mut p = 0i64;
        for j in 0..order {
            p += i64::from(coeffs[j]) * i64::from(decoded[i - order + j]);
        }
        decoded[i] = decoded[i].wrapping_sub((p >> qlevel) as i32);
    }
    lpc16(decoded, coeffs, qlevel);
}

/// The coefficient precision, shift and coefficients of an LPC subframe.
/// `coeffs[order - 1]` applies to the previous sample.
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
                for d in decoded.iter_mut() {
                    *d = gb.get_signed(bps as u32);
                }
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
