//! Small bit-manipulation helpers shared by the decoder and the MMU.

/// Extract `len` bits of `v` starting at bit `lo`.
#[inline(always)]
pub fn field(v: u32, lo: u32, len: u32) -> u32 {
    (v >> lo) & ((1u32 << len) - 1)
}

/// Single bit of `v`, as a bool.
#[inline(always)]
pub fn bit(v: u32, n: u32) -> bool {
    (v >> n) & 1 != 0
}

/// Sign-extend the low `bits` bits of `v`.
#[inline(always)]
pub fn sext(v: u64, bits: u32) -> i64 {
    let s = 64 - bits;
    ((v << s) as i64) >> s
}

/// Mask with the low `n` bits set (n may be 64).
#[inline(always)]
pub fn ones(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Truncate a result to the operand size (32 or 64 bits).
#[inline(always)]
pub fn trunc(v: u64, sf: bool) -> u64 {
    if sf {
        v
    } else {
        v & 0xffff_ffff
    }
}

/// ARM DecodeBitMasks for logical immediates. Returns `None` for reserved encodings.
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32, sf: bool) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let esize = 1u32 << len;
    if !sf && esize > 32 {
        return None;
    }
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    if s == levels {
        return None;
    }
    let welem = ones(s + 1);
    let emask = ones(esize);
    let rot = if r == 0 {
        welem
    } else {
        ((welem >> r) | (welem << (esize - r))) & emask
    };
    let mut out = 0u64;
    let mut i = 0;
    while i < 64 {
        out |= rot << i;
        i += esize;
    }
    Some(trunc(out, sf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmasks() {
        // and x0, x1, #0xff  -> N=1 immr=0 imms=7
        assert_eq!(decode_bit_masks(1, 7, 0, true), Some(0xff));
        // orr w0, wzr, #0x55555555 -> N=0 immr=0 imms=0b111100
        assert_eq!(decode_bit_masks(0, 0b111100, 0, false), Some(0x5555_5555));
        // and x0, x0, #0xfffffffffffffff0 -> N=1 immr=60 imms=59
        assert_eq!(decode_bit_masks(1, 59, 60, true), Some(0xffff_ffff_ffff_fff0));
        // reserved: all ones
        assert_eq!(decode_bit_masks(1, 63, 0, true), None);
    }

    #[test]
    fn sign_extension() {
        assert_eq!(sext(0x3ffffff, 26), -1);
        assert_eq!(sext(0x1, 26), 1);
        assert_eq!(sext(0x40, 7), -64);
    }
}
