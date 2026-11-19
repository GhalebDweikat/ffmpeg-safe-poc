//! Unit tests. They use a small TTA encoder (a port of the relevant parts
//! of FFmpeg's `ttaenc.c` and `ttaencdsp.c`) to make packets.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

struct BitWriter {
    out: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), nbits: 0 }
    }

    fn bit(&mut self, b: u32) {
        if self.nbits % 8 == 0 {
            self.out.push(0);
        }
        if b != 0 {
            *self.out.last_mut().unwrap() |= 1 << (self.nbits % 8);
        }
        self.nbits += 1;
    }

    fn bits(&mut self, n: u32, v: u32) {
        for i in 0..n {
            self.bit(v.checked_shr(i).unwrap_or(0) & 1);
        }
    }

    /// Pad to a byte and append the CRC-32.
    fn finish(mut self) -> Vec<u8> {
        self.nbits = self.out.len() * 8;
        let crc = crc32(&self.out);
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// One Rice code for `outval`, with the encoder's parameter adaptation.
fn put_code(w: &mut BitWriter, rice: &mut Rice, mut outval: u32) {
    let mut k = rice.k[0];
    rice.sum[0] = rice.sum[0].wrapping_add(outval.wrapping_sub(rice.sum[0] >> 4));
    if rice.k[0] > 0 && rice.sum[0] < shift_16(rice.k[0]) {
        rice.k[0] -= 1;
    } else if rice.sum[0] > shift_16(rice.k[0] + 1) {
        rice.k[0] += 1;
    }
    if outval >= shift_1(k) {
        outval -= shift_1(k);
        k = rice.k[1];
        rice.sum[1] = rice.sum[1].wrapping_add(outval.wrapping_sub(rice.sum[1] >> 4));
        if rice.k[1] > 0 && rice.sum[1] < shift_16(rice.k[1]) {
            rice.k[1] -= 1;
        } else if rice.sum[1] > shift_16(rice.k[1] + 1) {
            rice.k[1] += 1;
        }
        for _ in 0..1 + (outval >> k) {
            w.bit(1);
        }
    }
    w.bit(0);
    if k != 0 {
        w.bits(k, outval & (shift_1(k) - 1));
    }
}

/// `ttaenc_filter_process_c`
fn enc_filter(f: &mut Filter, input: &mut i32, shift: i32) {
    if f.error < 0 {
        for i in 0..8 {
            f.qm[i] = f.qm[i].wrapping_sub(f.dx[i]);
        }
    } else if f.error > 0 {
        for i in 0..8 {
            f.qm[i] = f.qm[i].wrapping_add(f.dx[i]);
        }
    }
    let mut round: i32 = 1 << (shift - 1);
    for i in 0..8 {
        round = round.wrapping_add(f.dl[i].wrapping_mul(f.qm[i]));
    }
    for i in 0..4 {
        f.dx[i] = f.dx[i + 1];
        f.dl[i] = f.dl[i + 1];
    }
    f.dx[4] = (f.dl[4] >> 30) | 1;
    f.dx[5] = ((f.dl[5] >> 30) | 2) & !1;
    f.dx[6] = ((f.dl[6] >> 30) | 2) & !1;
    f.dx[7] = ((f.dl[7] >> 30) | 4) & !3;
    f.dl[4] = f.dl[5].wrapping_neg();
    f.dl[5] = f.dl[6].wrapping_neg();
    f.dl[6] = input.wrapping_sub(f.dl[7]);
    f.dl[7] = *input;
    f.dl[5] = f.dl[5].wrapping_add(f.dl[6]);
    f.dl[4] = f.dl[4].wrapping_add(f.dl[5]);
    *input = input.wrapping_sub(round >> shift);
    f.error = *input;
}

/// Encode interleaved samples (already in the range of `bps` bytes).
fn encode(samples: &[i32], channels: usize, bps: u8, password: Option<&[u8]>) -> Vec<u8> {
    let mut ch: Vec<Channel<Filter>> = (0..channels)
        .map(|_| {
            let mut c = Channel {
                predictor: 0,
                filter: Filter::new([0; 8]),
                rice: Rice::new(10, 10),
            };
            if let Some(p) = password {
                for (q, b) in c.filter.qm.iter_mut().zip(password_crc64(p).to_le_bytes()) {
                    *q = i32::from(b as i8);
                }
            }
            c
        })
        .collect();
    let mut w = BitWriter::new();
    let mut res = 0;
    for (n, &s) in samples.iter().enumerate() {
        let cur = n % channels;
        let c = &mut ch[cur];
        let mut value = s;
        if channels > 1 {
            if cur < channels - 1 {
                res = samples[n + 1].wrapping_sub(value);
                value = res;
            } else {
                value = value.wrapping_sub(res / 2);
            }
        }
        let temp = value;
        value = value.wrapping_sub(pred(c.predictor, if bps == 1 { 4 } else { 5 }));
        c.predictor = temp;
        enc_filter(&mut c.filter, &mut value, i32::from(FILTER_CONFIGS[usize::from(bps) - 1]));
        let outval = if value > 0 {
            ((value as u32) << 1).wrapping_sub(1)
        } else {
            (value.wrapping_neg() as u32) << 1
        };
        put_code(&mut w, &mut c.rice, outval);
    }
    w.finish()
}

fn header(format: u16, channels: u16, bits: u16, rate: u32, data_length: u32) -> [u8; 22] {
    let mut h = [0u8; 22];
    h[..4].copy_from_slice(b"TTA1");
    h[4..6].copy_from_slice(&format.to_le_bytes());
    h[6..8].copy_from_slice(&channels.to_le_bytes());
    h[8..10].copy_from_slice(&bits.to_le_bytes());
    h[10..14].copy_from_slice(&rate.to_le_bytes());
    h[14..18].copy_from_slice(&data_length.to_le_bytes());
    h
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

/// A test signal: a slow triangle wave plus noise of `noise_bits` bits.
fn signal(n: usize, bits: u32, noise_bits: u32, seed: u64) -> Vec<i32> {
    let mut r = Lcg(seed);
    let amp = 1i64 << (bits - 2);
    (0..n)
        .map(|i| {
            let ph = (i as i64 * 37) % 4000;
            let tri = if ph < 2000 { ph - 1000 } else { 3000 - ph };
            let noise = if noise_bits > 0 {
                i64::from(r.next() & ((1 << noise_bits) - 1)) - (1 << (noise_bits - 1))
            } else {
                0
            };
            (tri * amp / 1000 + noise) as i32
        })
        .collect()
}

/// Decode `packet` and return what a caller sees: the samples as 32-bit
/// values in the frame's format.
fn decode(dec: &mut Decoder, packet: &[u8]) -> Result<Vec<i32>, Error> {
    let h = *dec.header();
    let total = h.frame_length as usize * usize::from(h.channels);
    match h.bps {
        1 => {
            let mut o = vec![0u8; total];
            let n = dec.decode(packet, &mut Samples::U8(&mut o))?;
            Ok(o[..n * usize::from(h.channels)].iter().map(|&v| i32::from(v)).collect())
        }
        2 => {
            let mut o = vec![0i16; total];
            let n = dec.decode(packet, &mut Samples::S16(&mut o))?;
            Ok(o[..n * usize::from(h.channels)].iter().map(|&v| i32::from(v)).collect())
        }
        _ => {
            let mut o = vec![0i32; total];
            let n = dec.decode(packet, &mut Samples::S32(&mut o))?;
            Ok(o[..n * usize::from(h.channels)].to_vec())
        }
    }
}

/// What the decoder should output for these source samples.
fn expected(samples: &[i32], bps: u8) -> Vec<i32> {
    samples
        .iter()
        .map(|&s| match bps {
            1 => i32::from((s as u8).wrapping_add(0x80)),
            2 => s,
            _ => s * 256,
        })
        .collect()
}

#[test]
fn round_trip_all_formats() {
    // rate 245 gives 256 samples per frame
    for &(bits, bps) in &[(8u16, 1u8), (16, 2), (24, 3), (12, 2), (20, 3)] {
        for channels in 1..=MAX_CHANNELS {
            for &noise in &[0u32, 3, u32::from(bits) - 2] {
                let src = signal(256 * channels, u32::from(bits), noise, 7 + channels as u64);
                let pkt = encode(&src, channels, bps, None);
                let mut dec =
                    Decoder::new(&header(1, channels as u16, bits, 245, 0), None).unwrap();
                assert!(frame_crc_ok(&pkt));
                let got = decode(&mut dec, &pkt).unwrap();
                assert_eq!(got, expected(&src, bps), "bits {bits} channels {channels} noise {noise}");
            }
        }
    }
}

#[test]
fn silence_uses_rice_parameter_zero() {
    // All-zero input drives k0 down to 0, where a code is one bit.
    let src = vec![0i32; 2 * 256];
    let pkt = encode(&src, 2, 2, None);
    assert!(pkt.len() < 2 * 256 * 3 / 8 + 4 + 64);
    let mut dec = Decoder::new(&header(1, 2, 16, 245, 0), None).unwrap();
    assert_eq!(decode(&mut dec, &pkt).unwrap(), src);
}

#[test]
fn encrypted_round_trip_needs_the_password() {
    let src = signal(256 * 2, 16, 6, 3);
    let pkt = encode(&src, 2, 2, Some(b"ffmpeg"));
    assert_eq!(Decoder::new(&header(2, 2, 16, 245, 0), None).err(), Some(HeaderError::MissingPassword));
    let mut dec = Decoder::new(&header(2, 2, 16, 245, 0), Some(b"ffmpeg")).unwrap();
    assert_eq!(decode(&mut dec, &pkt).unwrap(), src);
    let mut wrong = Decoder::new(&header(2, 2, 16, 245, 0), Some(b"FFmpeg")).unwrap();
    assert_ne!(decode(&mut wrong, &pkt).unwrap(), src);
    // A password given for a plain stream is ignored.
    let plain = encode(&src, 2, 2, None);
    let mut dec = Decoder::new(&header(1, 2, 16, 245, 0), Some(b"ffmpeg")).unwrap();
    assert_eq!(decode(&mut dec, &plain).unwrap(), src);
}

#[test]
fn password_crc() {
    assert_eq!(password_crc64(b""), 0);
    // CRC-64/WE of "123456789"
    assert_eq!(password_crc64(b"123456789"), 0x62EC_59E3_F1A4_F00A);
}

#[test]
fn last_frame_is_short() {
    // data_length 256 + 100: the last frame has 100 samples.
    let src = signal(100 * 2, 16, 4, 11);
    let pkt = encode(&src, 2, 2, None);
    let mut dec = Decoder::new(&header(1, 2, 16, 245, 356), None).unwrap();
    assert_eq!(dec.header().last_frame_length, 100);
    assert_eq!(decode(&mut dec, &pkt).unwrap(), src);
    // The same packet is too short for a stream whose last frame is not
    // 100 samples long.
    let mut dec = Decoder::new(&header(1, 2, 16, 245, 355), None).unwrap();
    assert_eq!(decode(&mut dec, &pkt), Err(Error::InvalidData));
    // A full frame is still a full frame when the header says the last
    // one is short.
    let full = signal(256 * 2, 16, 4, 12);
    let pkt = encode(&full, 2, 2, None);
    let mut dec = Decoder::new(&header(1, 2, 16, 245, 356), None).unwrap();
    assert_eq!(decode(&mut dec, &pkt).unwrap(), full);
}

#[test]
fn short_frame_cut_needs_exactly_four_bytes_left() {
    let src = signal(100, 16, 4, 5);
    let mut pkt = encode(&src, 1, 2, None);
    let mut dec = Decoder::new(&header(1, 1, 16, 245, 100), None).unwrap();
    assert_eq!(decode(&mut dec, &pkt).unwrap().len(), 100);
    // One more byte after the CRC: decoding goes on past sample 100 and
    // runs out of data.
    pkt.push(0);
    assert_eq!(decode(&mut dec, &pkt), Err(Error::InvalidData));
}

#[test]
fn truncated_and_padded_packets() {
    let src = signal(256, 16, 5, 9);
    let pkt = encode(&src, 1, 2, None);
    let mut dec = Decoder::new(&header(1, 1, 16, 245, 0), None).unwrap();
    // Fewer than 32 bits after the last sample: refused.
    for cut in 1..=8 {
        assert_eq!(decode(&mut dec, &pkt[..pkt.len() - cut]), Err(Error::InvalidData), "cut {cut}");
    }
    assert_eq!(decode(&mut dec, &[]), Err(Error::InvalidData));
    // Extra bytes are ignored, and so is a wrong CRC.
    let mut long = pkt.clone();
    long.extend_from_slice(&[0xAA; 9]);
    assert_eq!(decode(&mut dec, &long).unwrap(), src);
    assert!(!frame_crc_ok(&long));
    assert!(!frame_crc_ok(&pkt[..3]));
}

#[test]
fn unary_run_to_the_end_of_the_packet() {
    // All ones: the first code's run of ones ends with the packet, then
    // there are no bits for the remainder.
    let mut dec = Decoder::new(&header(1, 1, 16, 245, 0), None).unwrap();
    assert_eq!(decode(&mut dec, &[0xFF; 300]), Err(Error::InvalidData));
    // A run of 2^21 + 1 ones with k = 10 makes the value overflow 31 bits.
    let mut big = vec![0xFFu8; (1 << 18) + 1];
    big.extend_from_slice(&[0; 64]);
    assert_eq!(decode(&mut dec, &big), Err(Error::InvalidData));
}

#[test]
fn large_values_push_the_rice_parameter_past_25() {
    // Values a dozen times larger than the current parameter allows for
    // make it grow with every code. Once it passes 25 the decoder refuses
    // the packet, however much data follows.
    let mut w = BitWriter::new();
    let mut rice = Rice::new(10, 10);
    let mut codes = 0;
    while rice.k[0].max(rice.k[1]) <= 25 {
        let big = 12 << rice.k[0].max(rice.k[1]);
        put_code(&mut w, &mut rice, big);
        codes += 1;
    }
    assert!(codes < 100);
    // Whichever of the two parameters passed 25, use it: a code without
    // a run of ones reads k0 bits, one with a run reads k1 bits.
    w.bits(31, 0);
    w.bits(32, u32::MAX);
    w.bits(32, u32::MAX);
    for _ in 0..300 {
        w.bits(16, 0x5555);
    }
    let pkt = w.finish();
    let mut dec = Decoder::new(&header(1, 1, 16, 245, 0), None).unwrap();
    assert_eq!(decode(&mut dec, &pkt), Err(Error::InvalidData));
    // The same codes while the parameter is still small decode fine.
    let mut w = BitWriter::new();
    let mut rice = Rice::new(10, 10);
    for n in 0..256 {
        let v = if n < 8 { 12 << rice.k[0].max(rice.k[1]) } else { 0 };
        put_code(&mut w, &mut rice, v);
    }
    assert!(rice.k[0] < 25 && rice.k[1] < 25);
    assert_eq!(decode(&mut dec, &w.finish()).unwrap().len(), 256);
}

#[test]
fn header_checks() {
    use HeaderError::*;
    let ok = header(1, 2, 16, 44100, 1_000_000);
    let h = Header::parse(&ok, false).unwrap();
    assert_eq!((h.frame_length, h.last_frame_length, h.bps), (46080, 1_000_000 % 46080, 2));
    assert_eq!(Header::parse(&ok[..21], false), Err(TooShort));
    let mut bad = ok;
    bad[3] = b'2';
    assert_eq!(Header::parse(&bad, false), Err(WrongSignature));
    assert_eq!(Header::parse(&header(3, 2, 16, 44100, 0), false), Err(InvalidFormat));
    assert_eq!(Header::parse(&header(2, 2, 16, 44100, 0), false), Err(MissingPassword));
    assert!(Header::parse(&header(0, 2, 16, 44100, 0), false).is_ok());
    assert_eq!(Header::parse(&header(1, 0, 16, 44100, 0), false), Err(InvalidChannels));
    assert_eq!(Header::parse(&header(1, 17, 16, 44100, 0), false), Err(InvalidChannels));
    assert_eq!(Header::parse(&header(1, 2, 16, 0, 0), false), Err(InvalidSampleRate));
    assert_eq!(Header::parse(&header(1, 2, 0, 44100, 0), false), Err(UnsupportedSampleFormat));
    assert_eq!(Header::parse(&header(1, 2, 25, 44100, 0), false), Err(UnsupportedSampleFormat));
    assert_eq!(Header::parse(&header(1, 2, 16, 0x80_0000, 0), false), Err(SampleRateTooLarge));
    assert_eq!(Header::parse(&header(1, 2, 16, 0xFFFF_FFFF, 0), false), Err(SampleRateTooLarge));
    assert_eq!(Header::parse(&header(1, 1, 8, 1, 5), false).unwrap().frame_length, 1);
}

#[test]
fn output_slice_must_match() {
    let dec = Decoder::new(&header(1, 2, 16, 245, 0), None).unwrap();
    let mut small = [0i16; 511];
    assert_eq!(dec.decode(&[0; 100], &mut Samples::S16(&mut small)), Err(Error::BadOutput));
    let mut wrong = [0u8; 512];
    assert_eq!(dec.decode(&[0; 100], &mut Samples::U8(&mut wrong)), Err(Error::BadOutput));
}

#[test]
fn random_packets_never_panic() {
    let mut r = Lcg(99);
    for case in 0..300 {
        let channels = 1 + (r.next() % 16) as u16;
        let bits = [8u16, 16, 24][(r.next() % 3) as usize];
        let rate = 1 + r.next() % 300;
        let mut dec =
            Decoder::new(&header(1, channels, bits, rate, r.next() % 1000), None).unwrap();
        let len = (r.next() % 3000) as usize;
        let ones = r.next() % 8;
        let pkt: Vec<u8> = (0..len)
            .map(|_| {
                let b = r.next() as u8;
                if ones == 0 { b | (r.next() as u8) } else { b & (r.next() as u8) }
            })
            .collect();
        let _ = decode(&mut dec, &pkt);
        let _ = case;
    }
}

/// The bit reader of the straight port, one bit at a time, as the
/// reference for [`Bits`].
struct SlowBits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl SlowBits<'_> {
    fn left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    fn bit(&self, at: usize) -> u32 {
        u32::from(self.data[at / 8] >> (at % 8)) & 1
    }

    /// A run of ones and the zero after it; an error if the packet ends
    /// first.
    fn unary(&mut self) -> Result<u32, Error> {
        let mut n = 0;
        while self.pos + n < self.data.len() * 8 {
            if self.bit(self.pos + n) == 0 {
                self.pos += n + 1;
                return Ok(n as u32);
            }
            n += 1;
        }
        Err(Error::InvalidData)
    }

    fn take(&mut self, k: u32) -> Result<u32, Error> {
        if self.left() < k as usize {
            return Err(Error::InvalidData);
        }
        let v = (0..k as usize).fold(0, |v, i| v | self.bit(self.pos + i) << i);
        self.pos += k as usize;
        Ok(v)
    }
}

/// The 64-bit window reader agrees with the bit-at-a-time one for every
/// mix of runs and fields, in particular for runs of ones as long as the
/// window (56 to 64 bits) and in the last bytes of a packet, where the
/// window is filled a byte at a time.
#[test]
fn bit_window_matches_bit_at_a_time_reader() {
    let mut r = Lcg(2024);
    let mut ops = 0u32;
    let mut long_runs = 0u32;
    for case in 0..60_000 {
        let len = (r.next() % 40) as usize;
        let ones = 1 + r.next() % 12;
        // Mostly 0xFF; the other bytes are random, or 0x7F and 0xFE, which
        // end or start a run at a byte boundary.
        let data: Vec<u8> = (0..len)
            .map(|_| match (r.next() % ones, r.next() % 4) {
                (0, 0) => r.next() as u8,
                (0, 1) => 0xFE,
                (0, _) => 0x7F,
                _ => 0xFF,
            })
            .collect();
        let mut fast = Bits::new(&data);
        let mut slow = SlowBits { data: &data, pos: 0 };
        loop {
            let (a, b) = if r.next() % 3 != 0 {
                (fast.unary(), slow.unary())
            } else {
                let k = r.next() % 26;
                (fast.take(k), slow.take(k))
            };
            assert_eq!(a, b, "case {case} data {data:02x?} at bit {}", slow.pos);
            if a.is_err() {
                break;
            }
            assert_eq!(fast.left(), slow.left(), "case {case} data {data:02x?}");
            assert!(fast.count <= 63, "case {case}: {} bits in the window", fast.count);
            ops += 1;
            long_runs += u32::from(a.is_ok_and(|v| (56..=64).contains(&v)));
        }
    }
    assert!(ops > 100_000 && long_runs > 1000, "{ops} operations, {long_runs} long runs");
}

/// Found by the differential fuzzer (seed 1005, case 32196): with exactly
/// 56 bits in the window and fewer than 8 bytes after them, topping the
/// window up byte by byte made it 64 bits, and a run of 63 ones then
/// shifted the window by 64, which leaves it unchanged.
#[test]
fn run_of_63_ones_in_a_full_window_at_the_end_of_a_packet() {
    // 5 bits, then 63 ones and a zero starting at a byte boundary 14
    // bytes before the end, then a 12-bit field.
    let mut data = vec![0u8; 3];
    data.extend_from_slice(&[0xFF; 7]);
    data.extend_from_slice(&[0x7F, 0xE3, 0x0E, 0x8F, 0x44, 0x33, 0x66]);
    let mut bits = Bits::new(&data);
    assert_eq!(bits.take(19), Ok(0));
    assert_eq!(bits.unary(), Ok(0));
    assert_eq!(bits.take(4), Ok(0));
    assert_eq!(bits.unary(), Ok(63));
    assert_eq!(bits.take(12), Ok(0xEE3));
    assert_eq!(bits.left(), 36);
}

/// The two packets on which the differential fuzzer found the bug above,
/// with the output of FFmpeg's C decoder for them. Both end in seven 0xFF
/// bytes and 0x7F, fourteen bytes before the end of the packet.
#[test]
fn packets_the_fuzzer_found_decode_like_the_c_decoder() {
    // Seed 1005, case 32196: 24-bit stereo, 297 Hz, a last frame of 85
    // samples. The port's last two samples were wrong.
    let packet = include_bytes!("../testdata/fuzz-seed1005-case32196.bin");
    let want: Vec<i32> = include_bytes!("../testdata/fuzz-seed1005-c-output.raw")
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let mut dec = Decoder::new(&header(1, 2, 24, 297, 85), None).unwrap();
    assert_eq!(want.len(), 85 * 2);
    assert_eq!(decode(&mut dec, packet).unwrap(), want);

    // Seed 1010, case 224945: 8-bit mono, 165 Hz, format 0. The port
    // refused the packet.
    let packet = include_bytes!("../testdata/fuzz-seed1010-case224945.bin");
    let want: Vec<i32> = include_bytes!("../testdata/fuzz-seed1010-c-output.raw")
        .iter()
        .map(|&b| i32::from(b))
        .collect();
    let mut dec = Decoder::new(&header(0, 1, 8, 165, 0xF746_F651), None).unwrap();
    assert_eq!(want.len(), 172);
    assert_eq!(decode(&mut dec, packet).unwrap(), want);
}
