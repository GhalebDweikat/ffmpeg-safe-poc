//! Unit tests on hand-made frames. The frame writer here is independent
//! of the decoder: it follows the FLAC format description.

use super::*;
use crate::crc::{crc16, crc8};

struct BitWriter {
    bytes: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { bytes: Vec::new(), nbits: 0 }
    }
    fn put(&mut self, n: u32, v: u64) {
        for i in (0..n).rev() {
            if self.nbits % 8 == 0 {
                self.bytes.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.nbits % 8);
            self.nbits += 1;
        }
    }
    fn align(&mut self) {
        self.nbits = self.bytes.len() * 8;
    }
    fn rice(&mut self, k: u32, v: i32) {
        let u = ((v << 1) ^ (v >> 31)) as u32;
        self.put(u >> k, 0);
        self.put(1, 1);
        self.put(k, u64::from(u & ((1 << k) - 1)));
    }
}

/// How one channel is coded.
enum Sub<'a> {
    Constant(i64),
    Verbatim(&'a [i64]),
    /// Order, Rice parameter, samples.
    Fixed(usize, u32, &'a [i64]),
    /// Coefficients (the last applies to the previous sample), shift, Rice
    /// parameter, samples.
    Lpc(&'a [i32], u32, u32, &'a [i64]),
}

fn subframe(w: &mut BitWriter, sub: &Sub, bps: u32, wasted: u32) {
    let kind = match sub {
        Sub::Constant(_) => 0,
        Sub::Verbatim(_) => 1,
        Sub::Fixed(order, ..) => 8 + *order as u64,
        Sub::Lpc(coeffs, ..) => 32 + coeffs.len() as u64 - 1,
    };
    w.put(1, 0);
    w.put(6, kind);
    if wasted > 0 {
        w.put(1, 1);
        w.put(wasted, 1);
    } else {
        w.put(1, 0);
    }
    let bps = bps - wasted;
    let sample = |w: &mut BitWriter, v: i64| w.put(bps, (v >> wasted) as u64);
    let residuals = |w: &mut BitWriter, k: u32, res: &[i64]| {
        w.put(2, 0);
        w.put(4, 0);
        w.put(4, u64::from(k));
        for &r in res {
            w.rice(k, r as i32);
        }
    };
    match sub {
        Sub::Constant(v) => sample(w, *v),
        Sub::Verbatim(s) => s.iter().for_each(|&v| sample(w, v)),
        Sub::Fixed(order, k, s) => {
            let s: Vec<i64> = s.iter().map(|&v| v >> wasted).collect();
            s[..*order].iter().for_each(|&v| w.put(bps, v as u64));
            let res: Vec<i64> = (*order..s.len())
                .map(|i| match order {
                    0 => s[i],
                    1 => s[i] - s[i - 1],
                    2 => s[i] - 2 * s[i - 1] + s[i - 2],
                    3 => s[i] - 3 * s[i - 1] + 3 * s[i - 2] - s[i - 3],
                    _ => s[i] - 4 * s[i - 1] + 6 * s[i - 2] - 4 * s[i - 3] + s[i - 4],
                })
                .collect();
            residuals(w, *k, &res);
        }
        Sub::Lpc(coeffs, qlevel, k, s) => {
            let order = coeffs.len();
            let s: Vec<i64> = s.iter().map(|&v| v >> wasted).collect();
            s[..order].iter().for_each(|&v| w.put(bps, v as u64));
            w.put(4, 14); // 15-bit coefficients
            w.put(5, u64::from(*qlevel));
            for &c in coeffs.iter().rev() {
                w.put(15, c as u64);
            }
            let res: Vec<i64> = (order..s.len())
                .map(|i| {
                    let p: i64 = (0..order).map(|j| i64::from(coeffs[j]) * s[i - order + j]).sum();
                    s[i] - (p >> qlevel)
                })
                .collect();
            residuals(w, *k, &res);
        }
    }
}

/// One frame. `assignment` is the channel assignment code (0 to 7:
/// independent channels, 8 left/side, 9 side/right, 10 mid/side).
fn frame(blocksize: usize, assignment: u64, bps: u32, wasted: u32, subs: &[Sub]) -> Vec<u8> {
    frame_numbered(&[0], blocksize, assignment, bps, wasted, subs)
}

/// A frame whose frame number is coded as the bytes `number`.
fn frame_numbered(
    number: &[u8],
    blocksize: usize,
    assignment: u64,
    bps: u32,
    wasted: u32,
    subs: &[Sub],
) -> Vec<u8> {
    let bps_code = match bps {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        _ => 7,
    };
    let mut w = BitWriter::new();
    w.put(15, 0x7ffc);
    w.put(1, 0);
    w.put(4, 7); // 16-bit block size follows
    w.put(4, 9); // 44100 Hz
    w.put(4, assignment);
    w.put(3, bps_code);
    w.put(1, 0);
    for &b in number {
        w.put(8, u64::from(b));
    }
    w.put(16, blocksize as u64 - 1);
    let crc = crc8(0, &w.bytes);
    w.put(8, u64::from(crc));
    for (ch, sub) in subs.iter().enumerate() {
        let side = matches!((assignment, ch), (8, 1) | (9, 0) | (10, 1));
        subframe(&mut w, sub, bps + u32::from(side), wasted);
    }
    w.align();
    let crc = crc16(0, &w.bytes);
    w.put(16, u64::from(crc));
    w.bytes
}

fn decoder() -> Decoder {
    Decoder::new(None, Request::default()).unwrap().0
}

fn expect_frame(dec: &mut Decoder, pkt: &[u8], opts: &Options) -> (Frame, Updates) {
    match dec.decode(pkt, opts) {
        (Outcome::Frame(f), upd) => (f, upd),
        other => panic!("no frame: {other:?}"),
    }
}

fn decode_s16(dec: &mut Decoder, pkt: &[u8], quirks: Quirks) -> Vec<i16> {
    let (f, _) = expect_frame(dec, pkt, &Options::default());
    let mut out = vec![0i16; f.samples * f.channels];
    dec.output(Output::S16(&mut out), quirks).unwrap();
    out
}

fn decode_s32(dec: &mut Decoder, pkt: &[u8]) -> Vec<i32> {
    let (f, _) = expect_frame(dec, pkt, &Options::default());
    let mut out = vec![0i32; f.samples * f.channels];
    dec.output(Output::S32(&mut out), Quirks::default()).unwrap();
    out
}

fn signal(n: usize, seed: u64, bits: u32) -> Vec<i64> {
    let mut s = seed;
    let mut v = 0i64;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v += ((s >> 40) as i64 & 0x3ff) - 0x200;
            v = v.clamp(-(1 << (bits - 2)), (1 << (bits - 2)) - 1);
            v
        })
        .collect()
}

fn interleave(a: &[i64], b: &[i64]) -> Vec<i64> {
    a.iter().zip(b).flat_map(|(&a, &b)| [a, b]).collect()
}

#[test]
fn verbatim_and_constant() {
    let a = signal(37, 1, 16);
    let pkt = frame(37, 1, 16, 0, &[Sub::Verbatim(&a), Sub::Constant(-1234)]);
    let mut dec = decoder();
    let (f, upd) = expect_frame(&mut dec, &pkt, &Options { check_crc: true, ..Default::default() });
    assert_eq!((f.samples, f.channels, f.bytes_read, f.crc_error), (37, 2, pkt.len(), false));
    assert_eq!(upd.channels, Some(2));
    assert_eq!(upd.sample_rate, Some(44100));
    assert_eq!(upd.bits_per_raw_sample, Some(16));
    assert_eq!(upd.sample_fmt, Some(SampleFmt::S16));
    let mut out = vec![0i16; 74];
    dec.output(Output::S16(&mut out), Quirks::default()).unwrap();
    let want: Vec<i16> = a.iter().flat_map(|&v| [v as i16, -1234]).collect();
    assert_eq!(out, want);
}

#[test]
fn fixed_predictors() {
    let a = signal(64, 2, 16);
    for order in 0..=4 {
        let pkt = frame(64, 0, 16, 0, &[Sub::Fixed(order, 7, &a)]);
        let got = decode_s16(&mut decoder(), &pkt, Quirks::default());
        assert_eq!(got, a.iter().map(|&v| v as i16).collect::<Vec<_>>(), "order {order}");
    }
}

#[test]
fn lpc_both_widths() {
    let coeffs = [-3000, 9000, -11000, 13000];
    // 16 bits with 15-bit coefficients of order 4: the 64-bit sum.
    let a = signal(100, 3, 16);
    let pkt = frame(100, 0, 16, 0, &[Sub::Lpc(&coeffs, 13, 9, &a)]);
    let got = decode_s16(&mut decoder(), &pkt, Quirks::default());
    assert_eq!(got, a.iter().map(|&v| v as i16).collect::<Vec<_>>());
    // 8 bits: 8 + 15 + 2 <= 32, the 32-bit sum.
    let a = signal(100, 4, 8);
    let pkt = frame(100, 0, 8, 0, &[Sub::Lpc(&coeffs, 13, 4, &a)]);
    let got = decode_s16(&mut decoder(), &pkt, Quirks::default());
    assert_eq!(got, a.iter().map(|&v| (v << 8) as i16).collect::<Vec<_>>());
    // 24 bits.
    let a = signal(100, 5, 24);
    let pkt = frame(100, 0, 24, 0, &[Sub::Lpc(&coeffs, 13, 9, &a)]);
    let got = decode_s32(&mut decoder(), &pkt);
    assert_eq!(got, a.iter().map(|&v| (v << 8) as i32).collect::<Vec<_>>());
}

#[test]
fn stereo_modes() {
    let l = signal(50, 6, 16);
    let r = signal(50, 7, 16);
    let side: Vec<i64> = l.iter().zip(&r).map(|(l, r)| l - r).collect();
    let mid: Vec<i64> = l.iter().zip(&r).map(|(l, r)| (l + r) >> 1).collect();
    let want: Vec<i16> = interleave(&l, &r).iter().map(|&v| v as i16).collect();
    let cases = [
        (8, Sub::Verbatim(&l), Sub::Fixed(2, 8, &side)),
        (9, Sub::Fixed(1, 8, &side), Sub::Verbatim(&r)),
        (10, Sub::Fixed(3, 8, &mid), Sub::Verbatim(&side)),
    ];
    for (assignment, s0, s1) in cases {
        let pkt = frame(50, assignment, 16, 0, &[s0, s1]);
        for saturate_stereo in [false, true] {
            let quirks = Quirks { saturate_stereo, ..Default::default() };
            assert_eq!(decode_s16(&mut decoder(), &pkt, quirks), want, "assignment {assignment}");
        }
        let (mut p0, mut p1) = (vec![0i16; 50], vec![0i16; 50]);
        let mut dec = decoder();
        let opts = Options { request: Request { want32: false, planar: true }, ..Default::default() };
        let (_, upd) = expect_frame(&mut dec, &pkt, &opts);
        assert_eq!(upd.sample_fmt, Some(SampleFmt::S16P));
        dec.output(Output::S16P(&mut [&mut p0, &mut p1]), Quirks::default()).unwrap();
        assert_eq!(p0, l.iter().map(|&v| v as i16).collect::<Vec<_>>());
        assert_eq!(p1, r.iter().map(|&v| v as i16).collect::<Vec<_>>());
    }
}

#[test]
fn side_channel_of_33_bits() {
    // Left near the top and right near the bottom of 32 bits: the side
    // channel needs 33, its sample-to-sample differences are small.
    let l: Vec<i64> = [0, 3, 10, 4, 77].iter().map(|d| i64::from(i32::MAX) - d).collect();
    let r: Vec<i64> = [0, 5, 2, 30, 1].iter().map(|d| i64::from(i32::MIN) + d).collect();
    let side: Vec<i64> = l.iter().zip(&r).map(|(l, r)| l - r).collect();
    let mid: Vec<i64> = l.iter().zip(&r).map(|(l, r)| (l + r) >> 1).collect();
    let want: Vec<i32> = interleave(&l, &r).iter().map(|&v| v as i32).collect();
    let cases = [
        (8, Sub::Verbatim(&l), Sub::Verbatim(&side)),
        (9, Sub::Fixed(1, 4, &side), Sub::Verbatim(&r)),
        (10, Sub::Verbatim(&mid), Sub::Fixed(2, 5, &side)),
    ];
    for (assignment, s0, s1) in cases {
        let pkt = frame(5, assignment, 32, 0, &[s0, s1]);
        assert_eq!(decode_s32(&mut decoder(), &pkt), want, "assignment {assignment}");
    }
}

#[test]
fn wasted_bits() {
    let a: Vec<i64> = signal(40, 8, 12).iter().map(|v| v << 4).collect();
    let pkt = frame(40, 0, 16, 4, &[Sub::Fixed(2, 5, &a)]);
    let got = decode_s16(&mut decoder(), &pkt, Quirks::default());
    assert_eq!(got, a.iter().map(|&v| v as i16).collect::<Vec<_>>());
    // All sample bits zero where the wasted-bits count should end.
    let zeros = [0i64; 40];
    let mut pkt = frame(40, 0, 16, 0, &[Sub::Verbatim(&zeros)]);
    pkt[8] |= 1; // the "wasted bits" flag of the subframe header
    assert_eq!(decoder().decode(&pkt, &Options::default()).0, Outcome::Invalid);
}

#[test]
fn packets_that_are_not_frames() {
    let mut dec = decoder();
    let opts = Options::default();
    assert_eq!(dec.decode(b"\x7fFLAC\x01\x00\x00\x02fLaC", &opts).0, Outcome::Skip(13));
    assert_eq!(dec.decode(&[0x84, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], &opts).0, Outcome::Skip(12));
    assert_eq!(dec.decode(&[0xff, 0xf8, 0, 0, 0], &opts).0, Outcome::Skip(5));
    assert_eq!(dec.decode(&[0; 16], &opts).0, Outcome::Invalid);

    // An inline file header: STREAMINFO (mono, 20 bits, 96 kHz, blocks of
    // up to 4096), then a padding block flagged as the last.
    let mut head = b"fLaC\x00\x00\x00\x22".to_vec();
    let mut info = [0u8; 34];
    info[2..4].copy_from_slice(&4096u16.to_be_bytes());
    info[10..14].copy_from_slice(&((96000u32 << 12) | (19 << 4)).to_be_bytes());
    head.extend_from_slice(&info);
    head.extend_from_slice(&[0x81, 0, 0, 3, 9, 9, 9]);
    let a = signal(16, 9, 20);
    head.extend_from_slice(&frame(16, 0, 20, 0, &[Sub::Verbatim(&a)]));
    let (outcome, upd) = dec.decode(&head, &opts);
    assert_eq!(outcome, Outcome::Skip(49));
    assert_eq!(upd.sample_rate, Some(96000));
    assert_eq!(upd.channels, Some(1));
    assert_eq!(upd.bits_per_raw_sample, Some(20));
    assert_eq!(upd.sample_fmt, Some(SampleFmt::S32));
    assert_eq!(decode_s32(&mut dec, &head[49..]), a.iter().map(|&v| (v << 12) as i32).collect::<Vec<_>>());
    // A block of 4097 samples is more than the header allows.
    let long = [0i64; 4097];
    let pkt = frame(4097, 0, 20, 0, &[Sub::Constant(0)]);
    assert_eq!(dec.decode(&pkt, &opts).0, Outcome::Invalid);
    let _ = long;
    // The sample size cannot change.
    let pkt = frame(16, 0, 16, 0, &[Sub::Constant(0)]);
    assert_eq!(dec.decode(&pkt, &opts).0, Outcome::Invalid);
}

#[test]
fn extradata() {
    let mut info = [0u8; 34];
    info[2..4].copy_from_slice(&1152u16.to_be_bytes());
    info[10..14].copy_from_slice(&((44100u32 << 12) | (1 << 9) | (15 << 4)).to_be_bytes());
    let (_, upd) = Decoder::new(Some(&info), Request { want32: true, planar: false }).unwrap();
    assert_eq!(
        upd,
        Updates {
            channels: Some(2),
            sample_rate: Some(44100),
            bits_per_raw_sample: Some(16),
            sample_fmt: Some(SampleFmt::S32),
        }
    );
    let mut file = b"fLaC\x80\x00\x00\x22".to_vec();
    file.extend_from_slice(&info);
    assert!(Decoder::new(Some(&file), Request::default()).is_ok());
    assert!(Decoder::new(Some(&file[..41]), Request::default()).is_err());
    assert!(Decoder::new(Some(&info[..33]), Request::default()).is_err());
    info[3] = 15; // maximum block size 15
    info[2] = 0;
    assert!(Decoder::new(Some(&info), Request::default()).is_err());
}

#[test]
fn truncated_frame_and_crc() {
    let a = signal(32, 10, 16);
    let pkt = frame(32, 0, 16, 0, &[Sub::Verbatim(&a)]);
    let opts = Options { check_crc: true, ..Default::default() };
    // Without its last byte the frame still decodes: the missing byte is
    // read as zero, and FFmpeg's position does not go past the end by a
    // whole byte until the CRC is skipped.
    let (f, _) = expect_frame(&mut decoder(), &pkt[..pkt.len() - 1], &opts);
    assert_eq!(f.bytes_read, pkt.len());
    assert!(f.bytes_read > pkt.len() - 1);
    // A flipped sample bit: only the CRC notices.
    let mut bad = pkt.clone();
    bad[12] ^= 0x10;
    let (f, _) = expect_frame(&mut decoder(), &bad, &opts);
    assert!(f.crc_error);
    let (f, _) = expect_frame(&mut decoder(), &bad, &Options::default());
    assert!(!f.crc_error);
    // Two frames in one packet: the first reports its own length.
    let mut two = pkt.clone();
    two.extend_from_slice(&pkt);
    let (f, _) = expect_frame(&mut decoder(), &two, &opts);
    assert_eq!((f.bytes_read, f.crc_error), (pkt.len(), false));
}

#[test]
fn saturating_output_of_the_assembly() {
    // A 17-bit side channel makes right = left - side leave 16 bits.
    let l = [30000i64; 8];
    let side = [-30000i64; 8];
    let pkt = frame(8, 8, 16, 0, &[Sub::Verbatim(&l), Sub::Verbatim(&side)]);
    let c = decode_s16(&mut decoder(), &pkt, Quirks::default());
    assert_eq!(c[..2], [30000, 60000u16 as i16]);
    let asm = decode_s16(&mut decoder(), &pkt, Quirks { saturate_stereo: true, saturate_multi: true, saturate_8: true });
    assert_eq!(asm[..2], [30000, 32767]);
}

#[test]
fn frame_numbers() {
    let sub = [Sub::Constant(7)];
    let ok: [&[u8]; 6] = [
        &[0x41],
        &[0x7f],
        &[0xc2, 0x80],
        &[0xe1, 0x80, 0xbf],
        &[0xf4, 0x80, 0x80, 0x80],
        &[0xfd, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf],
    ];
    for number in ok {
        let pkt = frame_numbered(number, 16, 0, 16, 0, &sub);
        assert!(matches!(decoder().decode(&pkt, &Options::default()).0, Outcome::Frame(_)), "{number:x?}");
    }
    let bad: [&[u8]; 4] = [&[0x80], &[0xfe], &[0xc2, 0x7f], &[0xe1, 0x80, 0xc0]];
    for number in bad {
        let pkt = frame_numbered(number, 16, 0, 16, 0, &sub);
        assert_eq!(decoder().decode(&pkt, &Options::default()).0, Outcome::Invalid, "{number:x?}");
    }
}
