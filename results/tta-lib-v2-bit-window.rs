//! Safe Rust port of FFmpeg's TTA (True Audio) decoder: `tta.c`, with the
//! tables of `ttadata.c`, the adaptive filter of `ttadsp.c` (for which
//! FFmpeg has SSSE3 and SSE4 assembly in `x86/ttadsp.asm`), and the parts
//! of the little-endian bit reader (`get_bits.h`, `unary.h`) it uses.
//!
//! Unlike the C, the bit reader here works on a 64-bit window and finds
//! the end of a run of ones with a bit count, and the Rice parameter
//! adaptation and the filter's sign step are written without branches.
//! The results are the same for every packet.

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

/// The same table padded to 64 entries, so that an index can be masked
/// instead of bounds-checked. A Rice parameter never exceeds 35 (it grows
/// only while its sum is above `SHIFT_1[k + 5]`, and `SHIFT_1[40]` is the
/// largest 32-bit value), so the padding is never read.
const fn padded_shift_1() -> [u32; 64] {
    let mut t = [u32::MAX; 64];
    let mut i = 0;
    while i < SHIFT_1.len() {
        t[i] = SHIFT_1[i];
        i += 1;
    }
    t
}

static SHIFT_1X: [u32; 64] = padded_shift_1();

#[inline(always)]
fn shift_1(k: u32) -> u32 {
    SHIFT_1X[(k & 63) as usize]
}

#[inline(always)]
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

    /// `tta_filter_process_c` / `ff_tta_filter_process_sse4`: takes the
    /// decoded residual, returns the filtered value.
    #[inline(always)]
    fn process(&mut self, v: i32) -> i32 {
        // qm += dx * sign(error), without a branch
        let neg = self.error >> 31;
        let nonzero = -i32::from(self.error != 0);
        let mut sum = self.round;
        for i in 0..8 {
            let d = (self.dx[i] ^ neg).wrapping_sub(neg) & nonzero;
            self.qm[i] = self.qm[i].wrapping_add(d);
            sum = sum.wrapping_add(self.dl[i].wrapping_mul(self.qm[i]));
        }

        let out = v.wrapping_add(sum >> self.shift);

        let (dx, dl) = (self.dx, self.dl);
        self.dx = [
            dx[1],
            dx[2],
            dx[3],
            dx[4],
            (dl[4] >> 30) | 1,
            ((dl[5] >> 30) | 2) & !1,
            ((dl[6] >> 30) | 2) & !1,
            ((dl[7] >> 30) | 4) & !3,
        ];
        // dl[4] = -dl[5]; dl[5] = -dl[6]; dl[6] = out - dl[7]; dl[7] = out;
        // dl[5] += dl[6]; dl[4] += dl[5];
        let d6 = out.wrapping_sub(dl[7]);
        let d5 = d6.wrapping_sub(dl[6]);
        let d4 = d5.wrapping_sub(dl[5]);
        self.dl = [dl[1], dl[2], dl[3], dl[4], d4, d5, d6, out];
        self.error = v;
        out
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

/// The parameter after its sum changed: one down if the sum fell below
/// `shift_16[k]`, one up if it rose above `shift_16[k + 1]`. The table
/// never decreases, so the two cannot both apply.
#[inline(always)]
fn adapt(k: u32, sum: u32) -> u32 {
    let down = u32::from((k > 0) & (sum < shift_16(k)));
    let up = u32::from(sum > shift_16(k + 1));
    k + up - down
}

/// `TTAChannel`
#[derive(Clone, Copy)]
struct Channel {
    predictor: i32,
    filter: Filter,
    rice: Rice,
}

/// A little-endian bit reader over the packet and nothing else.
///
/// The C reader (`get_bits.h`) may load bytes from the padding after the
/// packet, but the decoder checks the number of bits left before every
/// read, so no bit from there ever reaches a sample. This reader has the
/// same results without the padding: it never looks past `data`.
struct Bits<'a> {
    data: &'a [u8],
    /// Index of the first byte that is not yet (fully) in `acc`.
    next: usize,
    /// The low `count` bits are the next unread bits. Any higher bits are
    /// either zero or already the stream's bits that follow.
    acc: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, next: 0, acc: 0, count: 0 }
    }

    /// `get_bits_left`
    fn left(&self) -> usize {
        self.count as usize + 8 * (self.data.len() - self.next)
    }

    /// Top the window up to at least 56 bits, or to all that is left.
    #[inline(always)]
    fn refill(&mut self) {
        match self.data.get(self.next..).and_then(|d| d.first_chunk::<8>()) {
            Some(chunk) => {
                self.acc |= u64::from_le_bytes(*chunk) << self.count;
                self.next += ((63 - self.count) >> 3) as usize;
                self.count |= 56;
            }
            None => self.refill_tail(),
        }
    }

    /// The last seven bytes of the packet, one at a time.
    #[cold]
    fn refill_tail(&mut self) {
        while self.count <= 56 {
            let Some(&b) = self.data.get(self.next) else {
                break;
            };
            self.acc |= u64::from(b) << self.count;
            self.next += 1;
            self.count += 8;
        }
    }

    /// `get_unary(gb, 0, get_bits_left(gb))`: the number of 1 bits before
    /// the next 0 bit, which is consumed too.
    ///
    /// In C a run that reaches the end of the packet stops there and
    /// decoding goes on, but then no bits are left, and a frame is only
    /// output if 32 bits are left after its last sample. So that case is
    /// an error here at once.
    #[inline(always)]
    fn unary(&mut self) -> Result<u32, Error> {
        self.refill();
        let ones = (!self.acc).trailing_zeros();
        if ones < self.count {
            self.acc >>= ones + 1;
            self.count -= ones + 1;
            Ok(ones)
        } else {
            self.unary_long()
        }
    }

    /// A run of ones that fills the window.
    #[cold]
    #[inline(never)]
    fn unary_long(&mut self) -> Result<u32, Error> {
        let mut total = 0u32;
        loop {
            let ones = (!self.acc).trailing_zeros();
            if ones < self.count {
                self.acc >>= ones + 1;
                self.count -= ones + 1;
                return Ok(total.saturating_add(ones));
            }
            total = total.saturating_add(self.count);
            self.acc = 0;
            self.count = 0;
            self.refill();
            if self.count == 0 {
                return Err(Error::InvalidData);
            }
        }
    }

    /// `get_bits(gb, k)` for 0 to 25 bits, after the C decoder's check
    /// that `k` bits are left.
    #[inline(always)]
    fn take(&mut self, k: u32) -> Result<u32, Error> {
        if self.count < k {
            self.refill();
            if self.count < k {
                return Err(Error::InvalidData);
            }
        }
        let v = self.acc as u32 & ((1 << k) - 1);
        self.acc >>= k;
        self.count -= k;
        Ok(v)
    }
}

/// How a decoded 32-bit value is stored in a frame.
trait Sample: Copy {
    fn from_decoded(v: i32) -> Self;
}

impl Sample for u8 {
    #[inline(always)]
    fn from_decoded(v: i32) -> u8 {
        (v as u8).wrapping_add(0x80)
    }
}

impl Sample for i16 {
    #[inline(always)]
    fn from_decoded(v: i32) -> i16 {
        v as i16
    }
}

impl Sample for i32 {
    /// 24-bit samples are stored in the top of 32 bits.
    #[inline(always)]
    fn from_decoded(v: i32) -> i32 {
        v.wrapping_mul(256)
    }
}

/// The frame's interleaved samples, in the format of the stream's bytes
/// per sample.
pub enum Samples<'a> {
    U8(&'a mut [u8]),
    S16(&'a mut [i16]),
    S32(&'a mut [i32]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The output slice does not match the stream (a caller bug).
    BadOutput,
    /// `AVERROR_INVALIDDATA`
    InvalidData,
}

/// `PRED(x, k)` of the C decoder: `x * (2^k - 1) / 2^k`, rounded down.
#[inline(always)]
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
        match (out, self.header.bps) {
            (Samples::U8(o), 1) => self.decode_into(packet, o),
            (Samples::S16(o), 2) => self.decode_into(packet, o),
            (Samples::S32(o), 3) => self.decode_into(packet, o),
            _ => Err(Error::BadOutput),
        }
    }

    fn decode_into<T: Sample>(&mut self, packet: &[u8], out: &mut [T]) -> Result<usize, Error> {
        let h = self.header;
        let channels = usize::from(h.channels);
        let mut framelen = h.frame_length as usize;
        let last_frame_length = h.last_frame_length as usize;
        let total = framelen.checked_mul(channels).ok_or(Error::BadOutput)?;
        let out = out.get_mut(..total).ok_or(Error::BadOutput)?;
        let chans = self.ch.get_mut(..channels).ok_or(Error::BadOutput)?;
        let pred_shift = if h.bps == 1 { 4 } else { 5 };

        // init per channel states
        for c in chans.iter_mut() {
            c.predictor = 0;
            c.filter = Filter::new(FILTER_CONFIGS[usize::from(h.bps) - 1]);
            if h.format == FORMAT_ENCRYPTED {
                for (q, &b) in c.filter.qm.iter_mut().zip(&self.crc_pass) {
                    *q = i32::from(b as i8);
                }
            }
            c.rice = Rice::new(10, 10);
        }

        let mut bits = Bits::new(packet);
        let mut group = [0i32; MAX_CHANNELS];
        let mut i = 0;
        for frame in out.chunks_exact_mut(channels) {
            for (c, g) in chans.iter_mut().zip(group.iter_mut()) {
                let rice = &mut c.rice;

                // A code is a run of ones, a zero, and k bits. Without a
                // run it uses the first parameter, with one the second.
                let unary = bits.unary()?;
                let deep = unary != 0;
                let k = if deep { rice.k1 } else { rice.k0 };
                let unary = unary - u32::from(deep);
                if k > MIN_CACHE_BITS {
                    return Err(Error::InvalidData);
                }
                let low = bits.take(k)?;
                if unary > (i32::MAX as u32) >> k {
                    return Err(Error::InvalidData);
                }
                let mut value = (unary << k) + low;

                // Adapt the parameters; the second only if it was used.
                let sum1 = rice.sum1.wrapping_add(value.wrapping_sub(rice.sum1 >> 4));
                let k1 = adapt(rice.k1, sum1);
                rice.sum1 = if deep { sum1 } else { rice.sum1 };
                rice.k1 = if deep { k1 } else { rice.k1 };
                value = value.wrapping_add(if deep { shift_1(rice.k0) } else { 0 });
                rice.sum0 = rice.sum0.wrapping_add(value.wrapping_sub(rice.sum0 >> 4));
                rice.k0 = adapt(rice.k0, rice.sum0);

                // extract coded value
                let value = value as i32;
                let residual = ((value >> 1) ^ ((value & 1) - 1)).wrapping_add(1);

                // run hybrid filter
                let filtered = c.filter.process(residual);

                // fixed order prediction
                let p = filtered.wrapping_add(pred(c.predictor, pred_shift));
                c.predictor = p;
                *g = p;
            }

            // decorrelate in case of multiple channels
            if channels > 1 {
                let last = channels - 1;
                group[last] = group[last].wrapping_add(group[last - 1] / 2);
                for r in (0..last).rev() {
                    group[r] = group[r + 1].wrapping_sub(group[r]);
                }
            }
            for (o, &g) in frame.iter_mut().zip(&group) {
                *o = T::from_decoded(g);
            }

            i += 1;
            // check for last frame
            if i == last_frame_length && bits.left() / 8 == 4 {
                framelen = last_frame_length;
                break;
            }
        }

        // After aligning to a byte, 32 bits must be left: the frame's
        // CRC, which is checked before decoding if that was asked for.
        if bits.left() < 32 {
            return Err(Error::InvalidData);
        }

        Ok(framelen)
    }
}

#[cfg(test)]
mod tests;
