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

/// `t[k][x]` is the checksum of byte `x` followed by `k` zero bytes, which
/// lets the CRC-16 take eight bytes in one step.
const fn crc16_tables() -> [[u16; 256]; 8] {
    let mut t = [[0u16; 256]; 8];
    t[0] = crc16_table();
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let c = t[k - 1][i];
            t[k][i] = (c << 8) ^ t[0][(c >> 8) as usize];
            i += 1;
        }
        k += 1;
    }
    t
}

static CRC8: [u8; 256] = crc8_table();
static CRC16: [[u16; 256]; 8] = crc16_tables();

pub fn crc8(crc: u8, data: &[u8]) -> u8 {
    data.iter().fold(crc, |c, &b| CRC8[usize::from(c ^ b)])
}

pub fn crc16(crc: u16, data: &[u8]) -> u16 {
    let (chunks, tail) = data.as_chunks::<8>();
    let mut c = crc;
    for b in chunks {
        let hi = (c >> 8) as u8 ^ b[0];
        let lo = c as u8 ^ b[1];
        c = CRC16[7][usize::from(hi)]
            ^ CRC16[6][usize::from(lo)]
            ^ CRC16[5][usize::from(b[2])]
            ^ CRC16[4][usize::from(b[3])]
            ^ CRC16[3][usize::from(b[4])]
            ^ CRC16[2][usize::from(b[5])]
            ^ CRC16[1][usize::from(b[6])]
            ^ CRC16[0][usize::from(b[7])];
    }
    tail.iter().fold(c, |c, &b| (c << 8) ^ CRC16[0][usize::from((c >> 8) as u8 ^ b)])
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
        // Eight bytes at a time and one at a time agree.
        let long: Vec<u8> = (0..1000u32).map(|i| (i * 37 + i / 7) as u8).collect();
        for cut in [0, 1, 7, 8, 9, 500, 999] {
            let (a, b) = long.split_at(cut);
            let bytewise = b.iter().fold(crc16(0, a), |c, &x| crc16(c, &[x]));
            assert_eq!(crc16(0, &long), bytewise, "cut {cut}");
        }
    }
}
