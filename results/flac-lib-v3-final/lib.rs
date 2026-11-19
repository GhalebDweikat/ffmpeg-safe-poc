//! Safe Rust port of FFmpeg's FLAC (Free Lossless Audio Codec) decoder:
//! libavcodec/flacdec.c with the parts of flac.c, flacdsp.c, get_bits.h and
//! golomb.h it uses, and the behaviour of the x86 assembly in
//! libavcodec/x86/flacdsp.asm.
//!
//! The decoder is bit-exact with the C one, including on damaged input:
//! it accepts and rejects the same packets and keeps the same state after
//! a failure.

#![forbid(unsafe_code)]

mod bits;
mod crc;
mod header;
mod output;
mod subframe;

use bits::BitReader;
use header::{FrameHeader, StreamInfo, MAX_BLOCKSIZE, STREAMINFO_SIZE};
pub use output::{Output, Quirks};
use subframe::LpcRules;

pub const MAX_CHANNELS: usize = 8;
/// Packets shorter than the smallest frame are ignored.
const MIN_FRAME_SIZE: usize = 10;
const METADATA_TYPE_VORBIS_COMMENT: u8 = 4;

/// The input is not something the C decoder accepts (`AVERROR_INVALIDDATA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid;

/// The decoder's four output formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFmt {
    S16,
    S32,
    S16P,
    S32P,
}

/// What the caller asked for (`request_sample_fmt`), as far as the decoder
/// looks at it.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Request {
    /// The requested format has more than two bytes per sample.
    pub want32: bool,
    pub planar: bool,
}

/// Per-packet settings.
#[derive(Clone, Copy, Default, Debug)]
pub struct Options {
    pub request: Request,
    /// Check the frame's CRC-16 (`err_recognition` has `crccheck` or
    /// `compliant`).
    pub check_crc: bool,
    /// The `use_buggy_lpc` option.
    pub buggy_lpc: bool,
}

/// Changes to the codec context that the C decoder makes while it parses.
/// They are made, and stay, even when the packet is rejected afterwards.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Updates {
    /// `ff_flac_set_channel_layout` was called with this channel count.
    pub channels: Option<i32>,
    pub sample_rate: Option<i32>,
    pub bits_per_raw_sample: Option<i32>,
    pub sample_fmt: Option<SampleFmt>,
}

/// A decoded frame, waiting to be written out with [`Decoder::output`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub samples: usize,
    pub channels: usize,
    /// How much of the packet the frame took, by FFmpeg's count: it can be
    /// one more than the packet has. The C decoder then still fetches and
    /// fills an output buffer before it fails.
    pub bytes_read: usize,
    /// The CRC-16 was checked and is wrong.
    pub crc_error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No frame; this many bytes of the packet are used up.
    Skip(usize),
    Frame(Frame),
    Invalid,
}

pub struct Decoder {
    info: StreamInfo,
    got_streaminfo: bool,
    /// Left shift that brings samples to the output format's 16 or 32 bits.
    sample_shift: i32,
    /// Of the last decoded frame.
    blocksize: usize,
    channels: usize,
    ch_mode: u8,
    /// A 33-bit side channel is waiting in `decoded_33`.
    side_33: bool,
    decoded: [Vec<i32>; MAX_CHANNELS],
    decoded_33: Vec<i64>,
}

impl Decoder {
    /// `flac_decode_init`. `extradata` is `None` when the context has
    /// none; then the stream parameters come from the first packets.
    pub fn new(extradata: Option<&[u8]>, request: Request) -> Result<(Decoder, Updates), Invalid> {
        let mut dec = Decoder {
            info: StreamInfo::default(),
            got_streaminfo: false,
            sample_shift: 0,
            blocksize: 0,
            channels: 0,
            ch_mode: 0,
            side_33: false,
            decoded: Default::default(),
            decoded_33: Vec::new(),
        };
        let mut upd = Updates::default();
        let Some(extradata) = extradata else {
            return Ok((dec, upd));
        };
        let block = header::streaminfo_in_extradata(extradata).ok_or(Invalid)?;
        dec.parse_streaminfo(block, request, &mut upd)?;
        Ok((dec, upd))
    }

    /// `ff_flac_parse_streaminfo` and what both of its callers do next.
    fn parse_streaminfo(
        &mut self,
        block: &[u8; STREAMINFO_SIZE],
        request: Request,
        upd: &mut Updates,
    ) -> Result<(), Invalid> {
        self.info.parse(block)?;
        upd.sample_rate = Some(self.info.samplerate);
        upd.bits_per_raw_sample = Some(self.info.bps);
        upd.channels = Some(self.info.channels);
        self.set_bps(request, upd);
        self.got_streaminfo = true;
        Ok(())
    }

    /// `flac_set_bps`: choose the output format for the stream's sample size.
    fn set_bps(&mut self, request: Request, upd: &mut Updates) {
        let need32 = self.info.bps > 16;
        if need32 || request.want32 {
            upd.sample_fmt = Some(if request.planar { SampleFmt::S32P } else { SampleFmt::S32 });
            self.sample_shift = 32 - self.info.bps;
        } else {
            upd.sample_fmt = Some(if request.planar { SampleFmt::S16P } else { SampleFmt::S16 });
            self.sample_shift = 16 - self.info.bps;
        }
    }

    /// `flac_decode_frame` up to the point where it asks for an output
    /// buffer. After `Outcome::Frame` the caller applies the updates, gets
    /// a buffer of that shape and calls `output`.
    #[inline(never)]
    pub fn decode(&mut self, buf: &[u8], opts: &Options) -> (Outcome, Updates) {
        self.decode_impl(buf, opts)
    }

    /// Inlined, with everything below it, into `decode` and `decode_avx2`.
    #[inline(always)]
    fn decode_impl(&mut self, buf: &[u8], opts: &Options) -> (Outcome, Updates) {
        let mut upd = Updates::default();
        let outcome = self.decode_packet(buf, opts, &mut upd).unwrap_or(Outcome::Invalid);
        (outcome, upd)
    }

    #[inline(always)]
    fn decode_packet(
        &mut self,
        buf: &[u8],
        opts: &Options,
        upd: &mut Updates,
    ) -> Result<Outcome, Invalid> {
        // An Ogg FLAC header packet.
        if buf.len() > 5 && buf[..5] == *b"\x7fFLAC" {
            return Ok(Outcome::Skip(buf.len()));
        }
        // A Vorbis comment metadata block.
        if !buf.is_empty() && buf[0] & 0x7f == METADATA_TYPE_VORBIS_COMMENT {
            return Ok(Outcome::Skip(buf.len()));
        }
        if buf.len() < MIN_FRAME_SIZE {
            return Ok(Outcome::Skip(buf.len()));
        }

        // The header of a FLAC file, passed as packet data.
        if buf[..4] == *b"fLaC" {
            if !self.got_streaminfo {
                self.parse_inline_streaminfo(buf, opts.request, upd)?;
            }
            return Ok(Outcome::Skip(metadata_size(buf)?));
        }

        // init_get_bits8 refuses a buffer whose size in bits overflows.
        if buf.len() > i32::MAX as usize / 8 {
            return Err(Invalid);
        }
        let mut gb = BitReader::new(buf);
        self.decode_frame(&mut gb, opts, upd)?;
        let bytes_read = gb.index() / 8;

        let mut crc_error = false;
        if opts.check_crc {
            // An overread by one byte took a zero from the packet's padding.
            let mut crc = crc::crc16(0, &buf[..bytes_read.min(buf.len())]);
            if bytes_read > buf.len() {
                crc = crc::crc16(crc, &[0]);
            }
            crc_error = crc != 0;
        }

        Ok(Outcome::Frame(Frame {
            samples: self.blocksize,
            channels: self.channels,
            bytes_read,
            crc_error,
        }))
    }

    /// `parse_streaminfo` of flacdec.c: the STREAMINFO block of an inline
    /// file header. A header too short to hold one is passed over.
    fn parse_inline_streaminfo(
        &mut self,
        buf: &[u8],
        request: Request,
        upd: &mut Updates,
    ) -> Result<(), Invalid> {
        let Some(block) = buf.get(8..).and_then(|b| b.first_chunk::<STREAMINFO_SIZE>()) else {
            return Ok(());
        };
        let metadata_type = buf[4] & 0x7f;
        let metadata_size = u32::from_be_bytes([0, buf[5], buf[6], buf[7]]);
        if metadata_type != 0 || metadata_size != STREAMINFO_SIZE as u32 {
            return Err(Invalid);
        }
        self.parse_streaminfo(block, request, upd)
    }

    /// `decode_frame`. The order of the checks and of the changes to the
    /// stream parameters is the C function's: an early failure leaves the
    /// changes made before it.
    #[inline(always)]
    fn decode_frame(
        &mut self,
        gb: &mut BitReader,
        opts: &Options,
        upd: &mut Updates,
    ) -> Result<(), Invalid> {
        let FrameHeader { samplerate, channels, bps, blocksize, ch_mode } =
            header::decode_frame_header(gb)?;

        self.info.channels = channels;
        upd.channels = Some(channels);
        self.ch_mode = ch_mode;

        if self.info.bps == 0 && bps == 0 {
            return Err(Invalid);
        }
        if bps != 0 && self.info.bps != 0 && bps != self.info.bps {
            return Err(Invalid); // the sample size cannot change mid-stream
        }
        if self.info.bps == 0 {
            self.info.bps = bps;
            upd.bits_per_raw_sample = Some(bps);
            self.set_bps(opts.request, upd);
        }

        if self.info.max_blocksize == 0 {
            self.info.max_blocksize = MAX_BLOCKSIZE;
        }
        if blocksize > self.info.max_blocksize {
            return Err(Invalid);
        }
        let blocksize = blocksize as usize;
        self.blocksize = blocksize;

        if self.info.samplerate == 0 && samplerate == 0 {
            return Err(Invalid);
        }
        if samplerate != 0 {
            self.info.samplerate = samplerate;
        }
        upd.sample_rate = Some(self.info.samplerate);
        self.got_streaminfo = true;

        let channels = channels as usize;
        self.channels = channels;
        let side_channel = match ch_mode {
            0 => None,
            2 => Some(0),
            _ => Some(1),
        };
        self.side_33 = self.info.bps == 32 && side_channel.is_some();
        if self.side_33 && self.decoded_33.len() < blocksize {
            self.decoded_33.resize(blocksize, 0);
        }
        let rules = LpcRules { stream_bps: self.info.bps, buggy_lpc: opts.buggy_lpc };

        for ch in 0..channels {
            let decoded = &mut self.decoded[ch];
            if decoded.len() < blocksize {
                decoded.resize(blocksize, 0);
            }
            let is_side = side_channel == Some(ch);
            let wide = if is_side && self.side_33 { &mut self.decoded_33[..blocksize] } else { &mut [][..] };
            let bps = self.info.bps + i32::from(is_side);
            subframe::decode_subframe(gb, &mut decoded[..blocksize], wide, bps, rules)?;
        }

        gb.align();
        gb.skip(16); // the frame's CRC-16
        Ok(())
    }

    /// The rest of `flac_decode_frame`: undo the stereo decorrelation and
    /// write the frame that the last `decode` returned. `shift` aside, the
    /// output format is the caller's: the C decoder writes whatever format
    /// its context holds at this point.
    #[inline(never)]
    pub fn output(&mut self, out: Output, quirks: Quirks) -> Option<()> {
        self.output_impl(out, quirks)
    }

    #[inline(always)]
    fn output_impl(&mut self, out: Output, quirks: Quirks) -> Option<()> {
        let n = self.blocksize;
        let mut ch_mode = self.ch_mode;
        if self.side_33 {
            let (ch0, ch1) = self.decoded.split_at_mut(1);
            output::decorrelate_33bps(
                ch_mode,
                ch0[0].get_mut(..n)?,
                ch1[0].get_mut(..n)?,
                self.decoded_33.get(..n)?,
            );
            self.side_33 = false;
            ch_mode = 0;
        }
        let mut channels: [&[i32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (c, d) in channels.iter_mut().zip(&self.decoded).take(self.channels) {
            *c = d.get(..n)?;
        }
        let shift = u32::try_from(self.sample_shift).ok()?;
        output::write(&channels[..self.channels], ch_mode, shift, quirks, out)
    }
}

/// The entry points for a CPU with AVX2, BMI1, BMI2 and LZCNT: the same
/// safe code as `decode` and `output`, compiled for those extensions.
/// Calling them is only sound on a CPU that has them, which is why Rust
/// makes the caller say `unsafe`; that one call is in the FFI crate,
/// behind a CPU check.
#[cfg(target_arch = "x86_64")]
impl Decoder {
    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt")]
    pub fn decode_avx2(&mut self, buf: &[u8], opts: &Options) -> (Outcome, Updates) {
        self.decode_impl(buf, opts)
    }

    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt")]
    pub fn output_avx2(&mut self, out: Output, quirks: Quirks) -> Option<()> {
        self.output_impl(out, quirks)
    }
}

/// `get_metadata_size`: the length of an inline file header, which is the
/// "fLaC" marker and metadata blocks up to the one flagged as last.
fn metadata_size(buf: &[u8]) -> Result<usize, Invalid> {
    let mut pos = 4;
    loop {
        let head = buf.get(pos..).and_then(|b| b.first_chunk::<4>()).ok_or(Invalid)?;
        let last = head[0] & 0x80 != 0;
        let size = u32::from_be_bytes([0, head[1], head[2], head[3]]) as usize;
        pos += 4;
        if buf.len() - pos < size {
            return Err(Invalid);
        }
        pos += size;
        if last {
            return Ok(pos);
        }
    }
}

#[cfg(test)]
mod tests;
