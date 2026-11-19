//! STREAMINFO and frame headers: ports of `ff_flac_parse_streaminfo`,
//! `ff_flac_is_extradata_valid` and `ff_flac_decode_frame_header` (flac.c).

use crate::bits::BitReader;
use crate::crc::crc8;
use crate::Invalid;

pub const STREAMINFO_SIZE: usize = 34;
pub const MIN_BLOCKSIZE: i32 = 16;
pub const MAX_BLOCKSIZE: i32 = 65535;

/// What the decoder knows about the stream (`FLACStreaminfo`). Zero means
/// "not known yet" for every field.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub samplerate: i32,
    pub channels: i32,
    pub bps: i32,
    pub max_blocksize: i32,
}

impl StreamInfo {
    /// `ff_flac_parse_streaminfo`. Like the C function it leaves the fields
    /// it got to before an error changed, and two of them replaced.
    pub fn parse(&mut self, block: &[u8; STREAMINFO_SIZE]) -> Result<(), Invalid> {
        self.max_blocksize = i32::from(u16::from_be_bytes([block[2], block[3]]));
        if self.max_blocksize < MIN_BLOCKSIZE {
            self.max_blocksize = 16;
            return Err(Invalid);
        }
        let v = u32::from_be_bytes([block[10], block[11], block[12], block[13]]);
        self.samplerate = (v >> 12) as i32;
        self.channels = ((v >> 9) & 7) as i32 + 1;
        self.bps = ((v >> 4) & 31) as i32 + 1;
        if self.bps < 4 {
            self.bps = 16;
            return Err(Invalid);
        }
        Ok(())
    }
}

/// `ff_flac_is_extradata_valid`: where the STREAMINFO block is in the
/// extradata, which is either the bare block or a file header ("fLaC", a
/// metadata block header, the block).
pub fn streaminfo_in_extradata(extradata: &[u8]) -> Option<&[u8; STREAMINFO_SIZE]> {
    if extradata.len() < STREAMINFO_SIZE {
        return None;
    }
    if extradata[..4] != *b"fLaC" {
        extradata.first_chunk()
    } else {
        extradata.get(8..)?.first_chunk()
    }
}

/// `FLACFrameInfo`, without the frame number, which the decoder does not use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub samplerate: i32,
    pub channels: i32,
    pub bps: i32,
    pub blocksize: i32,
    /// 0 independent, 1 left/side, 2 side/right, 3 mid/side.
    pub ch_mode: u8,
}

const SAMPLE_SIZE_TABLE: [i32; 8] = [0, 8, 12, 0, 16, 20, 24, 32];
const SAMPLE_RATE_TABLE: [i32; 12] =
    [0, 88200, 176400, 192000, 8000, 16000, 22050, 24000, 32000, 44100, 48000, 96000];
const BLOCKSIZE_TABLE: [i32; 16] =
    [0, 192, 576, 1152, 2304, 4608, 0, 0, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768];

/// The frame or sample number, in the extended UTF-8 coding of `GET_UTF8`.
/// Only its length and validity matter.
fn skip_utf8(gb: &mut BitReader) -> Result<(), Invalid> {
    let first = gb.get(8);
    if first & 0xc0 == 0x80 || first >= 0xfe {
        return Err(Invalid);
    }
    // A first byte with its top bit set has one continuation byte for each
    // one bit that follows the top bit.
    let mut bit = (first & 0x80) >> 1;
    while first & bit != 0 {
        let b = gb.get(8);
        if b & 0xc0 != 0x80 {
            return Err(Invalid);
        }
        bit >>= 1;
    }
    Ok(())
}

/// `ff_flac_decode_frame_header`.
pub fn decode_frame_header(gb: &mut BitReader) -> Result<FrameHeader, Invalid> {
    if gb.get(15) != 0x7ffc {
        return Err(Invalid);
    }
    gb.skip(1); // fixed or variable block size
    let bs_code = gb.get(4);
    let sr_code = gb.get(4);

    let ch = gb.get(4);
    let (channels, ch_mode) = match ch {
        0..=7 => (ch as i32 + 1, 0),
        8..=10 => (2, (ch - 7) as u8),
        _ => return Err(Invalid),
    };

    let bps_code = gb.get(3);
    if bps_code == 3 {
        return Err(Invalid);
    }
    let bps = SAMPLE_SIZE_TABLE[bps_code as usize];

    if gb.get1() {
        return Err(Invalid);
    }
    skip_utf8(gb)?;

    let blocksize = match bs_code {
        0 => return Err(Invalid),
        6 => gb.get(8) as i32 + 1,
        7 => gb.get(16) as i32 + 1,
        _ => BLOCKSIZE_TABLE[bs_code as usize],
    };
    let samplerate = match sr_code {
        0..=11 => SAMPLE_RATE_TABLE[sr_code as usize],
        12 => gb.get(8) as i32 * 1000,
        13 => gb.get(16) as i32,
        14 => gb.get(16) as i32 * 10,
        _ => return Err(Invalid),
    };

    gb.skip(8);
    // The checksum covers everything read so far. A header that runs past
    // the end of the packet has taken zeros from the padding, and FFmpeg's
    // position stops one byte past the end: that byte is summed too.
    let len = gb.index() / 8;
    let data = gb.data();
    let mut crc = crc8(0, &data[..len.min(data.len())]);
    if len > data.len() {
        crc = crc8(crc, &[0]);
    }
    if crc != 0 {
        return Err(Invalid);
    }

    Ok(FrameHeader { samplerate, channels, bps, blocksize, ch_mode })
}
