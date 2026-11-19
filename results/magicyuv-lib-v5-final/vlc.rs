//! FFmpeg's Huffman table builder (`ff_vlc_init_multi_from_lengths` in
//! `vlc.c`) as MagicYUV uses it.
//!
//! Two tables come out of one set of code lengths. The ordinary table maps
//! the next `bits` bits to a symbol, or to a sub-table for longer codes.
//! The joint table maps the same bits to up to six symbols at once (three
//! for 16-bit samples) where several short codes fit.
//!
//! The decoder accepts any lengths whose codes fit the code space, so
//! tables can have holes, and the joint table is built by a routine with
//! quirks of its own. Both are reproduced as they are; see `multi_gen`.

use alloc::vec::Vec;

const VLC_BITS: u32 = 12;
const VLC_MULTI_MAX_SYMBOLS: usize = 6;

/// `VLCElem`: a symbol and its code length, or (negative length) the
/// position and size of a sub-table. A hole has length 0 and symbol -1.
#[derive(Clone, Copy, Default)]
pub struct VlcElem {
    pub sym: i16,
    pub len: i16,
}

/// `VLCcode`: a code as the builder sees it, first bit in the top bit.
#[derive(Clone, Copy)]
struct VlcCode {
    bits: u8,
    symbol: i16,
    code: u32,
}

/// What `get_vlc_multi` found.
pub enum Multi {
    /// A joint entry and its number of symbols. The decoder stores all
    /// eight bytes of the entry.
    Joint(u64, usize),
    /// One symbol from the ordinary table; false if the bits were not a
    /// code.
    Single(i32, bool),
}

#[derive(Default)]
pub struct Vlc {
    /// Bits looked up in the first level of both tables.
    pub bits: u32,
    pub table: Vec<VlcElem>,
    /// `VLC_MULTI_ELEM` as the eight bytes the C struct has in memory, read
    /// as a little-endian word: six bytes of symbols (8-bit) or three
    /// 16-bit symbols, the total code length, the number of symbols.
    pub multi: Vec<u64>,
}

/// Buffers shared by all planes.
pub struct Scratch {
    /// Code length of every symbol.
    pub len: Vec<u8>,
    /// (length, symbol), longest codes first.
    he: Vec<(u8, u16)>,
    codes: Vec<VlcCode>,
}

impl Scratch {
    pub fn new() -> Self {
        Scratch { len: alloc::vec![0; 1 << 14], he: Vec::new(), codes: Vec::new() }
    }
}

impl Vlc {
    /// `huff_build`: tables for the `nb_elems` code lengths in
    /// `scratch.len`. `codes_pos[n]` is the number of codes of length `n`.
    /// Returns false where the C function fails.
    pub fn build(&mut self, scratch: &mut Scratch, codes_pos: &mut [u16; 33], nb_elems: usize) -> bool {
        // Sort by length, longest first, symbols ascending within a length.
        for i in (1..32).rev() {
            codes_pos[i] += codes_pos[i + 1];
        }
        let he = &mut scratch.he;
        he.clear();
        he.resize(nb_elems, (0, 0));
        for i in (0..nb_elems).rev() {
            let len = scratch.len[i];
            let pos = &mut codes_pos[len as usize];
            *pos -= 1;
            he[*pos as usize] = (len, i as u16);
        }

        self.table.clear();
        self.multi.clear();
        let nb_bits = (he[0].0 as u32).min(VLC_BITS);
        self.bits = nb_bits;

        // Hand out codes in that order, counting upwards.
        let len_max = 32.min(3 * nb_bits);
        let codes = &mut scratch.codes;
        codes.clear();
        let mut code = 0u64;
        for &(len, sym) in he.iter() {
            let len = len as u32;
            codes.push(VlcCode { bits: len as u8, symbol: sym as i16, code: code as u32 });
            if len > len_max || code & ((1u64 << (32 - len)) - 1) != 0 {
                return false;               // "Invalid VLC"
            }
            code += 1 << (32 - len);
            if code > 1 << 32 {
                return false;               // "Overdetermined VLC tree"
            }
        }

        if build_table(&mut self.table, nb_bits, codes).is_none() {
            self.table.clear();
            return false;
        }
        multi_gen(&mut self.multi, &self.table, nb_elems > 256, nb_bits, codes);
        true
    }
}

/// `build_table`: one level of the ordinary table, appended to `table`.
/// Returns its position, or `None` where the C function fails.
///
/// Like the original, this rewrites the codes that go into a sub-table:
/// their length and bits are cut down to what the sub-table looks at.
/// `multi_gen` later sees them in that state.
fn build_table(table: &mut Vec<VlcElem>, table_nb_bits: u32, codes: &mut [VlcCode]) -> Option<usize> {
    let table_size = 1usize << table_nb_bits;
    let table_index = table.len();
    table.resize(table_index + table_size, VlcElem::default());

    let mut i = 0;
    while i < codes.len() {
        let n = codes[i].bits as u32;
        let code = codes[i].code;
        let symbol = codes[i].symbol;
        if n <= table_nb_bits {
            // No need to add another table.
            let j = (code >> (32 - table_nb_bits)) as usize;
            let nb = 1usize << (table_nb_bits - n);
            for e in table[table_index..].get_mut(j..j + nb)? {
                if (e.len != 0 || e.sym != 0) && (e.len != n as i16 || e.sym != symbol) {
                    return None;            // "incorrect codes"
                }
                *e = VlcElem { sym: symbol, len: n as i16 };
            }
        } else {
            // Fill an auxiliary table with every code that shares the prefix.
            let mut n = n - table_nb_bits;
            let code_prefix = code >> (32 - table_nb_bits);
            let mut subtable_bits = n;
            codes[i].bits = n as u8;
            codes[i].code = code << table_nb_bits;
            let mut k = i + 1;
            while k < codes.len() {
                if codes[k].bits as u32 <= table_nb_bits {
                    break;
                }
                n = codes[k].bits as u32 - table_nb_bits;
                let code = codes[k].code;
                if code >> (32 - table_nb_bits) != code_prefix {
                    break;
                }
                codes[k].bits = n as u8;
                codes[k].code = code << table_nb_bits;
                subtable_bits = subtable_bits.max(n);
                k += 1;
            }
            subtable_bits = subtable_bits.min(table_nb_bits);
            let j = table_index + code_prefix as usize;
            table[j].len = -(subtable_bits as i16);
            let index = build_table(table, subtable_bits, &mut codes[i..k])?;
            // The position has to fit the 16-bit symbol field.
            table[j].sym = i16::try_from(index).ok()?;      // "strange codes"
            i = k - 1;
        }
        i += 1;
    }

    for e in &mut table[table_index..table_index + table_size] {
        if e.len == 0 {
            e.sym = -1;
        }
    }
    Some(table_index)
}

/// Where a joint entry keeps symbol number `level`.
fn set_symbol(info: u64, is16bit: bool, level: usize, sym: i16) -> u64 {
    if is16bit {
        let shift = 16 * level;
        info & !(0xffff << shift) | (sym as u16 as u64) << shift
    } else {
        let shift = 8 * level;
        info & !(0xff << shift) | (sym as u8 as u64) << shift
    }
}

struct MultiGen<'a> {
    table: &'a mut [u64],
    is16bit: bool,
    numbits: u32,
    buf: &'a [VlcCode],
    minlen: u32,
    max: usize,
}

impl MultiGen<'_> {
    /// `add_level`: extend the code `curcode` (`curlen` bits, `curlevel`
    /// symbols, held in `info`) by one more symbol in every way that still
    /// fits in `curlimit` bits, and enter the results into the table.
    ///
    /// Kept exactly as in C, including that every code is tried twice and
    /// that the code just below the usable ones (`max - 1`) is tried too.
    /// When all short codes are usable, that one is a long code which
    /// `build_table` has cut down to its tail, and no code at all. It is
    /// never reached then: the longest short code comes before it and
    /// ends the loop (`l >= curlimit`). Checked against the C decoder
    /// with a code made for the purpose.
    fn add_level(&mut self, curcode: u32, curlen: u32, curlimit: u32, curlevel: usize, mut info: u64) {
        let max_symbols = VLC_MULTI_MAX_SYMBOLS >> self.is16bit as usize;
        for i in (self.max..self.buf.len()).rev() {
            for j in 0..2 {
                let t = if j == 1 { i - 1 } else { i };
                let mut l = self.buf[t].bits as u32;
                let sym = self.buf[t].symbol;
                if l >= curlimit {
                    return;
                }
                let code = curcode.wrapping_add(self.buf[t].code >> curlen);
                let newlimit = curlimit - l;
                l += curlen;
                info = set_symbol(info, self.is16bit, curlevel, sym);

                if curlevel != 0 {
                    // Let's not add single entries.
                    let val = (code >> (32 - self.numbits)) as usize;
                    let nb = val + (1usize << (self.numbits - l));
                    info = info & 0x0000_ffff_ffff_ffff | (l as u64) << 48 | (curlevel as u64 + 1) << 56;
                    let nb = nb.min(self.table.len());
                    if val < nb {
                        self.table[val..nb].fill(info);
                    }
                }

                if curlevel + 1 < max_symbols && newlimit >= self.minlen {
                    self.add_level(code, l, newlimit, curlevel + 1, info);
                }
            }
        }
    }
}

/// `vlc_multi_gen`: the joint table for the codes in `buf`, which
/// `build_table` has already been through.
fn multi_gen(multi: &mut Vec<u64>, single: &[VlcElem], is16bit: bool, numbits: u32, buf: &[VlcCode]) {
    let nb_codes = buf.len();
    let size = 1usize << numbits;

    // Codes short enough for the first level. They are at the end of `buf`.
    let mut count0 = 0;
    let mut j = 0;
    while j < size {
        let len = single[j].len;
        if len > 0 {
            count0 += 1;
            j += (1 << (numbits - len as u32)) - 1;
        }
        j += 1;
    }
    let short = &buf[nb_codes - count0..];
    let minbits = short.iter().map(|c| c.bits as u32).min().unwrap_or(32);
    let maxbits = short.iter().map(|c| c.bits as u32).max().unwrap_or(0);

    // Only codes that fit into the table together with the shortest code
    // can start a joint entry.
    let mut max = nb_codes;
    while max > nb_codes - count0 {
        if buf[max - 1].bits as u32 + minbits > numbits {
            break;
        }
        max -= 1;
    }

    multi.extend(single[..size].iter().map(|e| {
        let num = (e.len > 0) as u64;
        let val = if is16bit { e.sym as u16 as u64 } else { e.sym as u8 as u64 };
        val | (e.len as u8 as u64) << 48 | num << 56
    }));

    let mut gen = MultiGen { table: multi, is16bit, numbits, buf, minlen: minbits, max };
    gen.add_level(0, 0, maxbits.min(numbits), 0, 0);
}
