//! The two checksums of a FLAC frame: CRC-8 (polynomial 0x07) over the
//! header and CRC-16 (polynomial 0x8005) over the whole frame, both fed
//! most significant bit first, starting from zero. A block that ends with
//! its own checksum gives zero, which is all the decoder asks.

const fn crc8_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn crc16_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC8: [u8; 256] = crc8_table();
static CRC16: [u16; 256] = crc16_table();

pub fn crc8(crc: u8, data: &[u8]) -> u8 {
    data.iter().fold(crc, |c, &b| CRC8[usize::from(c ^ b)])
}

pub fn crc16(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |c, &b| (c << 8) ^ CRC16[usize::from((c >> 8) as u8 ^ b)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(crc16(0, b"123456789"), 0xFEE8);
        assert_eq!(crc8(0, b"123456789"), 0xF4);
        // A block followed by its checksum gives zero.
        let mut block = b"123456789".to_vec();
        block.push(0xF4);
        assert_eq!(crc8(0, &block), 0);
        block.pop();
        block.extend_from_slice(&[0xFE, 0xE8]);
        assert_eq!(crc16(0, &block), 0);
    }
}
