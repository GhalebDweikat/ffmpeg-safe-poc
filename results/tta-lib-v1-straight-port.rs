//! Safe Rust port of FFmpeg's TTA (True Audio) decoder: `tta.c`, with the
//! tables of `ttadata.c`, the adaptive filter of `ttadsp.c` (for which
//! FFmpeg has SSSE3 and SSE4 assembly in `x86/ttadsp.asm`), and the parts
//! of the little-endian bit reader (`get_bits.h`, `unary.h`) it uses.
//!
//! Version 1: a straight port. The bit reader reads one bit at a time, as
//! the C does.

#![forbid(unsafe_code)]
#![no_std]

pub const FORMAT_ENCRYPTED: u16 = 2;
pub const MAX_CHANNELS: usize = 16;

/// `ff_tta_shift_1`; `ff_tta_shift_16[k]` is `SHIFT_1[k + 4]`.
const SHIFT_1: [u32; 41] = [
    0x0000_0001, 0x0000_0002, 0x0000_0004, 0x0000_0008,
    0x0000_0010, 0x0000_0020, 0x0000_0040, 0x0000_0080,
    0x0000_0100, 0x0000_0200, 0x0000_0400, 0x0000_0800,
    0x0000_1000, 0x0000_2000, 0x0000_4000, 0x0000_8000,
    0x0001_0000, 0x0002_0000, 0x0004_0000, 0x0008_0000,
    0x0010_0000, 0x0020_0000, 0x0040_0000, 0x0080_0000,
    0x0100_0000, 0x0200_0000, 0x0400_0000, 0x0800_0000,
    0x1000_0000, 0x2000_0000, 0x4000_0000, 0x8000_0000,
    0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000,
    0x8000_0000, 0x8000_0000, 0x8000_0000, 0x8000_0000,
    0xFFFF_FFFF,
];

/// A Rice parameter never exceeds 35: it grows only while its sum is
/// above `SHIFT_1[k + 5]`, and `SHIFT_1[40]` is the largest 32-bit value.
/// The lookups below still use `get`, so that this is not a proof
/// obligation.
fn shift_1(k: u32) -> u32 {
    SHIFT_1.get(k as usize).copied().unwrap_or(u32::MAX)
}

fn shift_16(k: u32) -> u32 {
    shift_1(k + 4)
}

/// `ff_tta_filter_configs`, indexed by bytes per sample - 1.
const FILTER_CONFIGS: [u8; 4] = [10, 9, 10, 12];

/// `MIN_CACHE_BITS` of the C bit reader: the most bits one read returns.
const MIN_CACHE_BITS: u32 = 25;

/// Why a TTA header was refused, in the order the C decoder checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Fewer than 22 bytes.
    TooShort,
    /// Does not start with "TTA1".
    WrongSignature,
    /// Format above 2.
    InvalidFormat,
    /// Encrypted stream and no password given.
    MissingPassword,
    /// 0 or more than 16 channels.
    InvalidChannels,
    /// Sample rate 0.
    InvalidSampleRate,
    /// Not 1 to 24 bits per sample.
    UnsupportedSampleFormat,
    /// Sample rate above 0x7FFFFF.
    SampleRateTooLarge,
    /// Unreachable with the limits above; kept because the C has it.
    FrameLengthTooLarge,
}

/// The stream parameters in a TTA1 header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub format: u16,
    pub channels: u16,
    pub bits_per_raw_sample: u16,
    /// Bytes per sample: 1, 2 or 3.
    pub bps: u8,
    pub sample_rate: i32,
    pub data_length: u32,
    /// Samples per channel in every frame but the last.
    pub frame_length: u32,
    /// Samples per channel in the last frame; 0 if it is a full one.
    pub last_frame_length: u32,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Header {
    /// Parse the 22-byte header the demuxer passes as extradata.
    pub fn parse(extradata: &[u8], has_password: bool) -> Result<Header, HeaderError> {
        let Some(h) = extradata.first_chunk::<22>() else {
            return Err(HeaderError::TooShort);
        };
        if &h[..4] != b"TTA1" {
            return Err(HeaderError::WrongSignature);
        }
        let format = le16(h, 4);
        if format > 2 {
            return Err(HeaderError::InvalidFormat);
        }
        if format == FORMAT_ENCRYPTED && !has_password {
            return Err(HeaderError::MissingPassword);
        }
        let channels = le16(h, 6);
        let bits_per_raw_sample = le16(h, 8);
        let bps = (u32::from(bits_per_raw_sample) + 7) / 8;
        let sample_rate = le32(h, 10) as i32;
        let data_length = le32(h, 14);
        // (bytes 18..22: CRC32 of the header, not checked)

        if channels == 0 || usize::from(channels) > MAX_CHANNELS {
            return Err(HeaderError::InvalidChannels);
        }
        if sample_rate == 0 {
            return Err(HeaderError::InvalidSampleRate);
        }
        if !(1..=3).contains(&bps) {
            return Err(HeaderError::UnsupportedSampleFormat);
        }
        // An unsigned comparison in C, so negative rates are refused here.
        if sample_rate as u32 > 0x7F_FFFF {
            return Err(HeaderError::SampleRateTooLarge);
        }
        let frame_length = 256 * sample_rate as u32 / 245;
        let last_frame_length = data_length % frame_length;
        if frame_length >= u32::MAX / (u32::from(channels) * 4) {
            return Err(HeaderError::FrameLengthTooLarge);
        }
        Ok(Header {
            format,
            channels,
            bits_per_raw_sample,
            bps: bps as u8,
            sample_rate,
            data_length,
            frame_length,
            last_frame_length,
        })
    }
}

/// `tta_check_crc64`: the CRC-64 of the password that seeds the filters of
/// an encrypted stream.
pub fn password_crc64(password: &[u8]) -> u64 {
    const POLY: u64 = 0x42F0_E1EB_A9EA_3693;
    let mut crc = u64::MAX;
    for &b in password {
        crc ^= u64::from(b) << 56;
        for _ in 0..8 {
            crc = (crc << 1) ^ (POLY & ((crc as i64 >> 63) as u64));
        }
    }
    crc ^ u64::MAX
}

const fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32_TABLE: [u32; 256] = crc32_table();

/// `tta_check_crc`: true if the last four bytes of the packet are the
/// CRC-32 (IEEE, little-endian) of the bytes before them. The packet must
/// have at least four bytes; a shorter one fails.
pub fn frame_crc_ok(packet: &[u8]) -> bool {
    let Some((body, tail)) = packet.split_last_chunk::<4>() else {
        return false;
    };
    let mut crc = u32::MAX;
    for &b in body {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    u32::from_le_bytes(*tail) == crc ^ u32::MAX
}

/// `TTAFilter`. The C arrays have 16 entries; the filter uses 8.
#[derive(Clone, Copy)]
struct Filter {
    shift: i32,
    round: i32,
    error: i32,
    qm: [i32; 8],
    dx: [i32; 8],
    dl: [i32; 8],
}

impl Filter {
    /// `ff_tta_filter_init`
    fn new(shift: u8) -> Filter {
        Filter {
            shift: i32::from(shift),
            round: shift_1(u32::from(shift) - 1) as i32,
            error: 0,
            qm: [0; 8],
            dx: [0; 8],
            dl: [0; 8],
        }
    }

    /// `tta_filter_process_c`
    fn process(&mut self, input: &mut i32) {
        let (qm, dx, dl) = (&mut self.qm, &mut self.dx, &mut self.dl);

        if self.error < 0 {
            for i in 0..8 {
                qm[i] = qm[i].wrapping_sub(dx[i]);
            }
        } else if self.error > 0 {
            for i in 0..8 {
                qm[i] = qm[i].wrapping_add(dx[i]);
            }
        }

        let mut round = self.round;
        for i in 0..8 {
            round = round.wrapping_add(dl[i].wrapping_mul(qm[i]));
        }

        dx[0] = dx[1];
        dx[1] = dx[2];
        dx[2] = dx[3];
        dx[3] = dx[4];
        dl[0] = dl[1];
        dl[1] = dl[2];
        dl[2] = dl[3];
        dl[3] = dl[4];

        dx[4] = (dl[4] >> 30) | 1;
        dx[5] = ((dl[5] >> 30) | 2) & !1;
        dx[6] = ((dl[6] >> 30) | 2) & !1;
        dx[7] = ((dl[7] >> 30) | 4) & !3;

        self.error = *input;
        *input = input.wrapping_add(round >> self.shift);

        dl[4] = dl[5].wrapping_neg();
        dl[5] = dl[6].wrapping_neg();
        dl[6] = input.wrapping_sub(dl[7]);
        dl[7] = *input;
        dl[5] = dl[5].wrapping_add(dl[6]);
        dl[4] = dl[4].wrapping_add(dl[5]);
    }
}

/// `TTARice`
#[derive(Clone, Copy)]
struct Rice {
    k0: u32,
    k1: u32,
    sum0: u32,
    sum1: u32,
}

impl Rice {
    /// `ff_tta_rice_init`
    fn new(k0: u32, k1: u32) -> Rice {
        Rice { k0, k1, sum0: shift_16(k0), sum1: shift_16(k1) }
    }
}

/// `TTAChannel`
#[derive(Clone, Copy)]
struct Channel {
    predictor: i32,
    filter: Filter,
    rice: Rice,
}

/// The little-endian bit reader of `get_bits.h`, as far as TTA uses it.
struct GetBits<'a> {
    buf: &'a [u8],
    index: usize,
    size_in_bits: usize,
}

impl<'a> GetBits<'a> {
    fn new(buf: &'a [u8]) -> Self {
        GetBits { buf, index: 0, size_in_bits: buf.len() * 8 }
    }

    /// `get_bits_left`. The decoder never moves past the end, so this is
    /// not negative.
    fn left(&self) -> usize {
        self.size_in_bits - self.index
    }

    fn byte(&self, at: usize) -> u32 {
        u32::from(self.buf.get(at).copied().unwrap_or(0))
    }

    /// `get_bits1`, called only when a bit is left.
    fn get_bits1(&mut self) -> u32 {
        let r = (self.byte(self.index >> 3) >> (self.index & 7)) & 1;
        self.index += 1;
        r
    }

    /// `get_unary(gb, 0, len)`: count 1 bits up to a 0 bit, which is
    /// consumed, or up to `len` bits.
    fn get_unary(&mut self, len: usize) -> u32 {
        let mut i = 0;
        while i < len && self.get_bits1() != 0 {
            i += 1;
        }
        i as u32
    }

    /// `get_bits(gb, n)` for 1 to 25 bits, called only when `n` bits are
    /// left.
    fn get_bits(&mut self, n: u32) -> u32 {
        let at = self.index >> 3;
        let word = self.byte(at)
            | self.byte(at + 1) << 8
            | self.byte(at + 2) << 16
            | self.byte(at + 3) << 24;
        let r = (word >> (self.index & 7)) & ((1 << n) - 1);
        self.index += n as usize;
        r
    }

    /// `align_get_bits`
    fn align(&mut self) {
        self.index = (self.index + 7) & !7;
    }
}

/// The frame's interleaved samples, in the format of the stream's bytes
/// per sample.
pub enum Samples<'a> {
    U8(&'a mut [u8]),
    S16(&'a mut [i16]),
    S32(&'a mut [i32]),
}

impl Samples<'_> {
    fn len(&self) -> usize {
        match self {
            Samples::U8(s) => s.len(),
            Samples::S16(s) => s.len(),
            Samples::S32(s) => s.len(),
        }
    }

    /// Store one sample of every channel, converted the way the C decoder
    /// converts its 32-bit buffer.
    fn put(&mut self, at: usize, group: &[i32]) {
        match self {
            Samples::U8(s) => {
                for (o, &v) in s[at..at + group.len()].iter_mut().zip(group) {
                    *o = (v as u8).wrapping_add(0x80);
                }
            }
            Samples::S16(s) => {
                for (o, &v) in s[at..at + group.len()].iter_mut().zip(group) {
                    *o = v as i16;
                }
            }
            Samples::S32(s) => {
                for (o, &v) in s[at..at + group.len()].iter_mut().zip(group) {
                    *o = v.wrapping_mul(256);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The output slice does not match the stream (a caller bug).
    BadOutput,
    /// `AVERROR_INVALIDDATA`
    InvalidData,
}

/// `PRED(x, k)` of the C decoder: `x * (2^k - 1) / 2^k`, rounded down.
fn pred(x: i32, k: u32) -> i32 {
    (((i64::from(x) << k) - i64::from(x)) >> k) as i32
}

/// A TTA decoder. Nothing is kept from one frame to the next but the
/// stream parameters.
pub struct Decoder {
    header: Header,
    /// Little-endian CRC-64 of the password.
    crc_pass: [u8; 8],
    ch: [Channel; MAX_CHANNELS],
}

impl Decoder {
    /// `password` is used by encrypted streams only (and required for
    /// them).
    pub fn new(extradata: &[u8], password: Option<&[u8]>) -> Result<Decoder, HeaderError> {
        let header = Header::parse(extradata, password.is_some())?;
        let crc_pass = match password {
            Some(p) if header.format == FORMAT_ENCRYPTED => password_crc64(p).to_le_bytes(),
            _ => [0; 8],
        };
        let ch = Channel { predictor: 0, filter: Filter::new(10), rice: Rice::new(10, 10) };
        Ok(Decoder { header, crc_pass, ch: [ch; MAX_CHANNELS] })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Decode one packet into `out`, which must hold a full frame:
    /// `frame_length * channels` samples of the stream's size. Returns
    /// the number of samples per channel that were decoded: the frame
    /// length, or the last frame's length.
    pub fn decode(&mut self, packet: &[u8], out: &mut Samples) -> Result<usize, Error> {
        let h = self.header;
        let channels = usize::from(h.channels);
        let mut framelen = h.frame_length as usize;
        let last_frame_length = h.last_frame_length as usize;
        let total = framelen.checked_mul(channels).ok_or(Error::BadOutput)?;
        let fits = match out {
            Samples::U8(_) => h.bps == 1,
            Samples::S16(_) => h.bps == 2,
            Samples::S32(_) => h.bps == 3,
        };
        if !fits || out.len() < total {
            return Err(Error::BadOutput);
        }

        // init per channel states
        for c in self.ch[..channels].iter_mut() {
            c.predictor = 0;
            c.filter = Filter::new(FILTER_CONFIGS[usize::from(h.bps) - 1]);
            if h.format == FORMAT_ENCRYPTED {
                for (q, &b) in c.filter.qm.iter_mut().zip(&self.crc_pass) {
                    *q = i32::from(b as i8);
                }
            }
            c.rice = Rice::new(10, 10);
        }

        let mut gb = GetBits::new(packet);
        let mut group = [0i32; MAX_CHANNELS];
        let mut cur_chan = 0;
        let mut i = 0;
        for _ in 0..total {
            let c = &mut self.ch[cur_chan];
            let rice = &mut c.rice;

            let mut unary = gb.get_unary(gb.left());
            let (depth, k);
            if unary == 0 {
                depth = 0;
                k = rice.k0;
            } else {
                depth = 1;
                k = rice.k1;
                unary -= 1;
            }

            if gb.left() < k as usize {
                return Err(Error::InvalidData);
            }

            let mut value: i32;
            if k != 0 {
                if k > MIN_CACHE_BITS || unary > (i32::MAX >> k) as u32 {
                    return Err(Error::InvalidData);
                }
                value = ((unary << k) + gb.get_bits(k)) as i32;
            } else {
                value = unary as i32;
            }

            if depth == 1 {
                rice.sum1 = rice.sum1.wrapping_add((value as u32).wrapping_sub(rice.sum1 >> 4));
                if rice.k1 > 0 && rice.sum1 < shift_16(rice.k1) {
                    rice.k1 -= 1;
                } else if rice.sum1 > shift_16(rice.k1 + 1) {
                    rice.k1 += 1;
                }
                value = value.wrapping_add(shift_1(rice.k0) as i32);
            }
            rice.sum0 = rice.sum0.wrapping_add((value as u32).wrapping_sub(rice.sum0 >> 4));
            if rice.k0 > 0 && rice.sum0 < shift_16(rice.k0) {
                rice.k0 -= 1;
            } else if rice.sum0 > shift_16(rice.k0 + 1) {
                rice.k0 += 1;
            }

            // extract coded value
            let mut p = ((value >> 1) ^ ((value & 1) - 1)).wrapping_add(1);

            // run hybrid filter
            c.filter.process(&mut p);

            // fixed order prediction
            p = p.wrapping_add(pred(c.predictor, if h.bps == 1 { 4 } else { 5 }));
            c.predictor = p;
            group[cur_chan] = p;

            // flip channels
            if cur_chan < channels - 1 {
                cur_chan += 1;
            } else {
                // decorrelate in case of multiple channels
                if channels > 1 {
                    let last = channels - 1;
                    group[last] = group[last].wrapping_add(group[last - 1] / 2);
                    for r in (0..last).rev() {
                        group[r] = group[r + 1].wrapping_sub(group[r]);
                    }
                }
                out.put(i * channels, &group[..channels]);
                cur_chan = 0;
                i += 1;
                // check for last frame
                if i == last_frame_length && gb.left() / 8 == 4 {
                    framelen = last_frame_length;
                    break;
                }
            }
        }

        gb.align();
        if gb.left() < 32 {
            return Err(Error::InvalidData);
        }
        // (the frame's CRC, checked before decoding if asked for)

        Ok(framelen)
    }
}

#[cfg(test)]
mod tests;
