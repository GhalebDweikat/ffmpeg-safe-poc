//! Unit tests on hand-made packets. They are smoke tests; the real checks
//! are FFmpeg's tests and the differential fuzzer.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// Bits, first bit in the top bit of the first byte.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, len: u32, value: u32) {
        self.acc = self.acc << len | value as u64;
        self.n += len;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(pad, 0);
        }
        self.out
    }
}

/// The code of every symbol for a set of lengths, as the decoder hands
/// them out: longest first, symbols ascending, counting upwards.
fn canonical(lens: &[u8]) -> Vec<u32> {
    let mut order: Vec<usize> = (0..lens.len()).collect();
    order.sort_by_key(|&s| (core::cmp::Reverse(lens[s]), s));
    let mut code = 0u64;
    let mut out = vec![0; lens.len()];
    for s in order {
        out[s] = (code >> (32 - lens[s])) as u32;
        code += 1 << (32 - lens[s]);
    }
    out
}

/// A slice: flags, predictor, then the symbols coded with `lens`.
fn coded_slice(pred: u8, lens: &[u8], symbols: &[u16]) -> Vec<u8> {
    let codes = canonical(lens);
    let mut w = BitWriter::default();
    for &s in symbols {
        w.put(lens[s as usize] as u32, codes[s as usize]);
    }
    let mut out = vec![0, pred];
    out.extend(w.finish());
    out
}

struct Pkt {
    format: u8,
    flags: u8,
    width: u32,
    height: u32,
    slice_height: u32,
    /// Code lengths per plane.
    lens: Vec<Vec<u8>>,
    /// Slice bytes, `[plane][slice]`.
    slices: Vec<Vec<Vec<u8>>>,
}

fn build(p: &Pkt) -> Vec<u8> {
    let planes = p.slices.len();
    let nb = p.slices[0].len();
    let mut out = Vec::new();
    out.extend(b"MAGY");
    out.extend(32u32.to_le_bytes());
    out.extend([7, p.format, 12, 0, p.flags, 0, 32, 0]);
    out.extend(p.width.to_le_bytes());
    out.extend(p.height.to_le_bytes());
    out.extend(p.width.to_le_bytes());
    out.extend(p.slice_height.to_le_bytes());
    out.extend(0u32.to_le_bytes());

    let mut table = Vec::new();
    for lens in &p.lens {
        // Runs of equal lengths, at most 256 long.
        let mut i = 0;
        while i < lens.len() {
            let run = lens[i..].iter().take(256).take_while(|&&l| l == lens[i]).count();
            if run == 1 {
                table.push(lens[i]);
            } else {
                table.extend([lens[i] | 0x80, (run - 1) as u8]);
            }
            i += run;
        }
    }
    // Offsets count from the end of the 32-byte header.
    let mut offset = (out.len() + 4 * planes * nb + 1 + planes * nb + table.len() - 32) as u32;
    for plane in &p.slices {
        for s in plane {
            out.extend(offset.to_le_bytes());
            offset += s.len() as u32;
        }
    }
    out.push(planes as u8);
    out.extend(vec![0; planes * nb]);
    out.extend(table);
    for plane in &p.slices {
        for s in plane {
            out.extend(s);
        }
    }
    out
}

const PAD: usize = 3;

struct Picture<T> {
    /// Visible rows of every plane.
    planes: Vec<Vec<Vec<T>>>,
    unknown_predictor: bool,
}

trait Sample: Copy + PartialEq + core::fmt::Debug {
    const SENTINEL: Self;
    fn decode(dec: &Decoder, pkt: &[u8], planes: &mut [Plane<'_, Self>]) -> Result<bool, BadPlanes>;
}
impl Sample for u8 {
    const SENTINEL: u8 = 0xa5;
    fn decode(dec: &Decoder, pkt: &[u8], planes: &mut [Plane<'_, u8>]) -> Result<bool, BadPlanes> {
        dec.decode8(pkt, planes)
    }
}
impl Sample for u16 {
    const SENTINEL: u16 = 0xa5a5;
    fn decode(dec: &Decoder, pkt: &[u8], planes: &mut [Plane<'_, u16>]) -> Result<bool, BadPlanes> {
        dec.decode16(pkt, planes)
    }
}

/// Run a packet through the decoder the way the shim does. The frame
/// starts out filled with `stale`; rows have padding, which must come back
/// untouched.
fn decode<T: Sample>(pkt: &[u8], stale: T) -> Result<Picture<T>, Error> {
    let mut dec = Decoder::new();
    let h = dec.header(pkt)?;
    dec.tables(pkt, h.width, h.height)?;
    let (w, ht) = (h.width as usize, h.height as usize);
    let sizes: Vec<(usize, usize)> = (0..h.format.planes)
        .map(|i| {
            let (hs, vs) = h.format.shifts(i);
            (ceil_rshift(w, hs), ceil_rshift(ht, vs))
        })
        .collect();
    let mut bufs: Vec<Vec<T>> = sizes
        .iter()
        .map(|&(pw, ph)| {
            let mut b = vec![T::SENTINEL; (pw + PAD) * ph];
            for row in b.chunks_mut(pw + PAD) {
                row[..pw].fill(stale);
            }
            b
        })
        .collect();
    let mut planes: Vec<Plane<'_, T>> = bufs
        .iter_mut()
        .zip(&sizes)
        .map(|(b, &(pw, ph))| Plane { data: &mut b[..(pw + PAD) * (ph - 1) + pw], stride: pw + PAD })
        .collect();
    let unknown_predictor = T::decode(&dec, pkt, &mut planes).expect("planes fit");
    let planes = bufs
        .iter()
        .zip(&sizes)
        .map(|(b, &(pw, _))| {
            b.chunks(pw + PAD)
                .map(|row| {
                    assert!(row[pw..].iter().all(|&s| s == T::SENTINEL), "padding written");
                    row[..pw].to_vec()
                })
                .collect()
        })
        .collect();
    Ok(Picture { planes, unknown_predictor })
}

fn rng(seed: &mut u64) -> u32 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (*seed >> 33) as u32
}

/// Every symbol 8 bits long: the code of a symbol is the symbol.
fn flat8() -> Vec<u8> {
    vec![8; 256]
}

fn gray8(width: u32, height: u32, flags: u8, slice: Vec<u8>) -> Vec<u8> {
    build(&Pkt { format: 0x6b, flags, width, height, slice_height: height, lens: vec![flat8()], slices: vec![vec![slice]] })
}

/// Textbook predictors, on whole pictures of residuals.
fn predict(res: &[Vec<u32>], pred: u8, step: usize, mask: u32, mask_gradient: bool) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = Vec::new();
    for (y, row) in res.iter().enumerate() {
        let mut o = vec![0u32; row.len()];
        for x in 0..row.len() {
            let left = if x > 0 { o[x - 1] } else { 0 };
            let p = if y < step {
                left
            } else {
                let top = &out[y - step];
                match (pred, x) {
                    (_, 0) => top[0],
                    (LEFT, _) => left,
                    (GRADIENT, _) => left.wrapping_add(top[x]).wrapping_sub(top[x - 1]),
                    _ => {
                        let g = left as i32 + top[x] as i32 - top[x - 1] as i32;
                        let g = if mask_gradient { g & mask as i32 } else { g };
                        mid_pred(left as i32, top[x] as i32, g) as u32
                    }
                }
            };
            o[x] = p.wrapping_add(row[x]) & mask;
        }
        out.push(o);
    }
    out
}

#[test]
fn predictors_8bit() {
    let (w, h) = (45usize, 7usize);
    let mut seed = 1;
    let res: Vec<Vec<u32>> = (0..h).map(|_| (0..w).map(|_| rng(&mut seed) & 0xff).collect()).collect();
    for pred in [LEFT, GRADIENT, MEDIAN] {
        for interlaced in [false, true] {
            let mut slice = vec![0, pred];
            slice.extend(res.iter().flatten().map(|&r| r as u8));
            let pkt = gray8(w as u32, h as u32, if interlaced { 2 } else { 0 }, slice);
            let pic = decode::<u8>(&pkt, 0).unwrap();
            let want = predict(&res, pred, 1 + interlaced as usize, 0xff, true);
            let got: Vec<Vec<u32>> = pic.planes[0].iter().map(|r| r.iter().map(|&s| s as u32).collect()).collect();
            assert_eq!(got, want, "pred {pred} interlaced {interlaced}");
            assert!(!pic.unknown_predictor);
        }
    }
}

#[test]
fn predictors_10bit() {
    let (w, h) = (21usize, 6usize);
    let mut seed = 2;
    let res: Vec<Vec<u32>> = (0..h).map(|_| (0..w).map(|_| rng(&mut seed) & 0x3ff).collect()).collect();
    let lens = vec![10u8; 1024];
    let symbols: Vec<u16> = res.iter().flatten().map(|&r| r as u16).collect();
    for pred in [LEFT, GRADIENT, MEDIAN] {
        for interlaced in [false, true] {
            let pkt = build(&Pkt {
                format: 0x73,
                flags: if interlaced { 2 } else { 0 },
                width: w as u32,
                height: h as u32,
                slice_height: h as u32,
                lens: vec![lens.clone()],
                slices: vec![vec![coded_slice(pred, &lens, &symbols)]],
            });
            let pic = decode::<u16>(&pkt, 0).unwrap();
            let want = predict(&res, pred, 1 + interlaced as usize, 0x3ff, false);
            let got: Vec<Vec<u32>> = pic.planes[0].iter().map(|r| r.iter().map(|&s| s as u32).collect()).collect();
            assert_eq!(got, want, "pred {pred} interlaced {interlaced}");
        }
    }
}

#[test]
fn raw_slices() {
    // 8 bits: the bytes of the slice. Predictor 0 is unknown: nothing is
    // added, and the caller is told.
    let mut slice = vec![1, 0];
    slice.extend(0..12u8);
    let pic = decode::<u8>(&gray8(4, 3, 0, slice.clone()), 0x77).unwrap();
    assert_eq!(pic.planes[0], [[0, 1, 2, 3], [4, 5, 6, 7], [8, 9, 10, 11]]);
    assert!(pic.unknown_predictor);
    // One byte short: the slice is skipped and the frame keeps what it had.
    slice.pop();
    let pic = decode::<u8>(&gray8(4, 3, 0, slice), 0x77).unwrap();
    assert_eq!(pic.planes[0], [[0x77; 4]; 3]);

    // 10 bits: packed samples.
    let lens = vec![10u8; 1024];
    let mut w = BitWriter::default();
    w.put(8, 1);
    w.put(8, LEFT as u32);
    for s in [1000, 23, 1, 5, 6, 7] {
        w.put(10, s);
    }
    let pkt = build(&Pkt {
        format: 0x73,
        flags: 0,
        width: 3,
        height: 2,
        slice_height: 2,
        lens: vec![lens],
        slices: vec![vec![w.finish()]],
    });
    let pic = decode::<u16>(&pkt, 0).unwrap();
    assert_eq!(pic.planes[0], [[1000, 1023, 0], [(1000 + 5) & 1023, (1005 + 6) & 1023, (1011 + 7) & 1023]]);
}

/// Two 2-bit codes, two 9-bit codes, 252 10-bit codes: a quarter of the
/// code space (bits `11`) is unused, and short codes come in joint entries.
fn short_lens() -> Vec<u8> {
    let mut lens = vec![10u8; 256];
    lens[0] = 2;
    lens[1] = 2;
    lens[2] = 9;
    lens[3] = 9;
    lens
}

#[test]
fn joint_entries_decode_like_single_symbols() {
    let lens = short_lens();
    let (w, h) = (37usize, 5usize);
    let mut seed = 3;
    let symbols: Vec<u16> = (0..w * h)
        .map(|_| match rng(&mut seed) % 8 {
            0 => (rng(&mut seed) % 256) as u16,
            n => (n & 1) as u16,
        })
        .collect();
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: w as u32,
        height: h as u32,
        slice_height: h as u32,
        lens: vec![lens.clone()],
        slices: vec![vec![coded_slice(0, &lens, &symbols)]],
    });
    let pic = decode::<u8>(&pkt, 0).unwrap();
    let got: Vec<u16> = pic.planes[0].iter().flatten().map(|&s| s as u16).collect();
    assert_eq!(got, symbols);
}

#[test]
fn slice_ends_early() {
    // Three symbols for a row of eight. The first comes through the joint
    // table, which stores eight bytes: the symbol, five zeros, the code
    // length and the symbol count. The next two overwrite two of those.
    let pic = decode::<u8>(&gray8(8, 2, 0, vec![0, 0, 50, 60, 70]), 0x77).unwrap();
    assert_eq!(pic.planes[0], [[50, 60, 70, 0, 0, 0, 8, 1], [0x77; 8]]);
    // With a predictor, it runs over all of it.
    let pic = decode::<u8>(&gray8(8, 2, 0, vec![0, LEFT, 50, 60, 70]), 1).unwrap();
    assert_eq!(pic.planes[0], [[50, 110, 180, 180, 180, 180, 188, 189], [51, 52, 53, 54, 55, 56, 57, 58]]);
}

#[test]
fn bits_past_the_slice_are_decoded() {
    // Two planes' worth of slices in one plane: slice 0 holds one byte, so
    // its reader's first fill takes in the start of slice 1 as well. With
    // 2-bit codes, the row of slice 0 gets four symbols from its own byte;
    // the reader then has no bits left and stops.
    let lens = short_lens();
    let s0 = vec![0, 0, 0b01_10_01_01];
    let s1 = coded_slice(0, &lens, &[1, 1, 1, 1, 1, 1]);
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: 6,
        height: 2,
        slice_height: 1,
        lens: vec![lens.clone()],
        slices: vec![vec![s0, s1]],
    });
    let pic = decode::<u8>(&pkt, 0x77).unwrap();
    assert_eq!(pic.planes[0], [[0, 1, 0, 0, 0x77, 0x77], [1, 1, 1, 1, 1, 1]]);

    // A 10-bit code that starts in the last two bits of the slice takes
    // its other eight bits from what follows: here slice 1's flags byte.
    let s0 = vec![0, 0, 0b01_10_01_00];
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: 6,
        height: 2,
        slice_height: 1,
        lens: vec![lens.clone()],
        slices: vec![vec![s0, coded_slice(0, &lens, &[1; 6])]],
    });
    let pic = decode::<u8>(&pkt, 0x77).unwrap();
    // Codes 0..251 (10 bits) are symbols 4..255: code 0 is symbol 4.
    assert_eq!(pic.planes[0][0], [0, 1, 0, 4, 0x77, 0x77]);
}

#[test]
fn bits_that_are_no_code() {
    let lens = short_lens();
    // In the last seven samples of a row symbols are read one at a time,
    // and bits that match no code give -1 without being consumed.
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: 5,
        height: 2,
        slice_height: 2,
        lens: vec![lens.clone()],
        slices: vec![vec![vec![0, 0, 0b01_10_11_01, 0x55]]],
    });
    let pic = decode::<u8>(&pkt, 0x77).unwrap();
    assert_eq!(pic.planes[0], [[0, 1, 255, 255, 255], [255; 5]]);
    // Before that, through the joint table, they end the slice.
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: 12,
        height: 2,
        slice_height: 2,
        lens: vec![lens.clone()],
        slices: vec![vec![vec![0, LEFT, 0b11_00_00_00, 0x55]]],
    });
    let pic = decode::<u8>(&pkt, 0x77).unwrap();
    let mut row = [0x77; 12];
    row[0] = 255;
    assert_eq!(pic.planes[0], [row, [0x77; 12]]);
}

#[test]
fn long_codes_use_sub_tables() {
    // Two 32-bit codes, then one each of 31 down to 8 bits, then 8-bit
    // codes. Everything longer than 12 bits shares one 12-bit prefix: a
    // sub-table of 12 bits with a sub-table of 8 bits below it.
    let mut lens = vec![8u8; 256];
    lens[0] = 32;
    for (i, l) in lens.iter_mut().enumerate().take(26).skip(1) {
        *l = 33 - i as u8;
    }
    let symbols: Vec<u16> = (0..40).map(|i| [255, 0, 1, 100, 7, 20, 19, 21, 13, 30][i % 10]).collect();
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: 8,
        height: 5,
        slice_height: 5,
        lens: vec![lens.clone()],
        slices: vec![vec![coded_slice(0, &lens, &symbols)]],
    });
    let pic = decode::<u8>(&pkt, 0).unwrap();
    let got: Vec<u16> = pic.planes[0].iter().flatten().map(|&s| s as u16).collect();
    assert_eq!(got, symbols);
}

#[test]
fn slices_and_subsampled_planes() {
    // 4:2:0, 6x5, two rows per slice: three slices, the last one a single
    // luma row and a single chroma row.
    let lens = vec![flat8(); 3];
    let slice = |first: u8, n: usize| -> Vec<u8> {
        let mut s = vec![1, LEFT];
        s.extend((0..n).map(|i| if i == 0 { first } else { 1 }));
        s
    };
    let pkt = build(&Pkt {
        format: 0x69,
        flags: 0,
        width: 6,
        height: 5,
        slice_height: 2,
        lens,
        slices: vec![
            vec![slice(10, 12), slice(20, 12), slice(30, 6)],
            vec![slice(40, 3), slice(50, 3), slice(60, 3)],
            vec![slice(70, 3), slice(80, 3), slice(90, 3)],
        ],
    });
    let pic = decode::<u8>(&pkt, 0).unwrap();
    let row = |first: u8, n: u8| -> Vec<u8> { (0..n).map(|i| first + i).collect() };
    // The second row of a slice starts from the first sample of the row above.
    assert_eq!(pic.planes[0], [row(10, 6), row(11, 6), row(20, 6), row(21, 6), row(30, 6)]);
    assert_eq!(pic.planes[1], [row(40, 3), row(50, 3), row(60, 3)]);
    assert_eq!(pic.planes[2], [row(70, 3), row(80, 3), row(90, 3)]);
}

#[test]
fn rgb_adds_green() {
    let raw = |v: u8| vec![1, LEFT, v, 0];
    let pkt = build(&Pkt {
        format: 0x65,
        flags: 0,
        width: 2,
        height: 1,
        slice_height: 1,
        lens: vec![flat8(); 3],
        slices: vec![vec![raw(250)], vec![raw(10)], vec![raw(3)]],
    });
    let pic = decode::<u8>(&pkt, 0).unwrap();
    assert_eq!(pic.planes, [[[4, 4]], [[10, 10]], [[13, 13]]]);

    // 10 bits: the sum is masked.
    let lens = vec![10u8; 1024];
    let s = |v: u16| coded_slice(LEFT, &lens, &[v, 0]);
    let pkt = build(&Pkt {
        format: 0x6d,
        flags: 0,
        width: 2,
        height: 1,
        slice_height: 1,
        lens: vec![lens.clone(); 3],
        slices: vec![vec![s(1020)], vec![s(10)], vec![s(3)]],
    });
    let pic = decode::<u16>(&pkt, 0).unwrap();
    assert_eq!(pic.planes, [[[6, 6]], [[10, 10]], [[13, 13]]]);
}

#[test]
fn refused_packets() {
    let good = gray8(4, 2, 0, vec![1, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(decode::<u8>(&good, 0).is_ok());
    let with = |at: usize, v: u8| {
        let mut p = good.clone();
        p[at] = v;
        decode::<u8>(&p, 0).err()
    };
    assert_eq!(with(0, b'N'), Some(Error::Invalid));
    assert_eq!(with(4, 31), Some(Error::HeaderSize(31)));
    assert_eq!(with(8, 6), Some(Error::Version(6)));
    assert_eq!(with(9, 0x64), Some(Error::Format(0x64)));
    assert_eq!(with(24, 5), Some(Error::SliceWidth(5)));
    assert_eq!(with(28, 0), Some(Error::SliceHeight(0)));
    // Interlaced needs two rows per slice.
    let mut p = good.clone();
    p[12] = 2;
    p[28] = 1;
    assert_eq!(decode::<u8>(&p, 0).err(), Some(Error::ImpossibleSliceHeight));
    // The number of planes after the slice offsets.
    assert_eq!(with(40, 2), Some(Error::Invalid));
    // Code lengths: 0 and 33 are not lengths; 255 codes are too few.
    assert_eq!(with(42, 0x80), Some(Error::HuffmanCodes));
    assert_eq!(with(42, 0x80 | 33), Some(Error::HuffmanCodes));
    assert_eq!(with(43, 254), Some(Error::HuffmanShort));
    // 256 codes of 7 bits do not fit.
    assert_eq!(with(42, 0x80 | 7), Some(Error::HuffmanBuild));
    assert_eq!(decode::<u8>(&good[..35], 0).err(), Some(Error::Invalid));
}

#[test]
fn code_lengths_must_line_up() {
    // 253 codes of 10 bits followed by 2-bit codes: the next 2-bit code
    // would not start on a 2-bit boundary, which the builder refuses.
    let mut lens = vec![10u8; 256];
    lens[0] = 2;
    lens[1] = 2;
    lens[2] = 2;
    let pkt = build(&Pkt {
        format: 0x6b,
        flags: 0,
        width: 4,
        height: 1,
        slice_height: 1,
        lens: vec![lens],
        slices: vec![vec![vec![0, 0, 0, 0]]],
    });
    assert_eq!(decode::<u8>(&pkt, 0).err(), Some(Error::HuffmanBuild));
}
