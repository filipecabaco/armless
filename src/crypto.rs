//! The Cryptographic Extension (AES, SHA1, SHA256, SHA512, SHA3, SM3, SM4) and CRC32, as the Arm ARM's pseudocode defines
//! them. Vectors are little-endian u128s: byte i of the register is bits 8i..8i+7.

const fn sbox() -> ([u8; 256], [u8; 256]) {
    let mut fwd = [0u8; 256];
    let mut inv = [0u8; 256];
    // Walk the multiplicative group with generator 3 (p) and its inverse 0xf6 (q).
    let (mut p, mut q) = (1u8, 1u8);
    loop {
        p ^= (p << 1) ^ if p & 0x80 != 0 { 0x1b } else { 0 };
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let x =
            q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4) ^ 0x63;
        fwd[p as usize] = x;
        inv[x as usize] = p;
        if p == 1 {
            break;
        }
    }
    fwd[0] = 0x63;
    inv[0x63] = 0;
    (fwd, inv)
}

const SBOX: ([u8; 256], [u8; 256]) = sbox();

fn bytes(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    r
}

/// AESE: AddRoundKey (with `key`), ShiftRows, SubBytes.
pub fn aese(state: u128, key: u128) -> u128 {
    let s = bytes(state ^ key);
    let mut r = [0u8; 16];
    for c in 0..4 {
        for row in 0..4 {
            r[row + 4 * c] = SBOX.0[s[row + 4 * ((c + row) % 4)] as usize];
        }
    }
    u128::from_le_bytes(r)
}

/// AESD: AddRoundKey (with `key`), InvShiftRows, InvSubBytes.
pub fn aesd(state: u128, key: u128) -> u128 {
    let s = bytes(state ^ key);
    let mut r = [0u8; 16];
    for c in 0..4 {
        for row in 0..4 {
            r[row + 4 * c] = SBOX.1[s[row + 4 * ((c + 4 - row) % 4)] as usize];
        }
    }
    u128::from_le_bytes(r)
}

fn mix(v: u128, m: [u8; 4]) -> u128 {
    let s = bytes(v);
    let mut r = [0u8; 16];
    for c in 0..4 {
        let col = &s[4 * c..4 * c + 4];
        for row in 0..4 {
            r[4 * c + row] = (0..4).fold(0, |acc, k| acc ^ gmul(m[(k + 4 - row) % 4], col[k]));
        }
    }
    u128::from_le_bytes(r)
}

/// AESMC: MixColumns.
pub fn aesmc(v: u128) -> u128 {
    mix(v, [2, 3, 1, 1])
}

/// AESIMC: InvMixColumns.
pub fn aesimc(v: u128) -> u128 {
    mix(v, [14, 11, 13, 9])
}

fn lane(v: u128, e: u32) -> u32 {
    (v >> (32 * e)) as u32
}

fn lanes(l: [u32; 4]) -> u128 {
    l.iter()
        .rev()
        .fold(0u128, |acc, &x| (acc << 32) | x as u128)
}

fn choose(x: u32, y: u32, z: u32) -> u32 {
    ((y ^ z) & x) ^ z
}

fn majority(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | ((x | y) & z)
}

fn parity(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

/// SHA1C (op 0), SHA1P (1), SHA1M (2): four rounds on hash state `x` with `e` and words `w`.
pub fn sha1_rounds(op: u32, x: u128, e: u32, w: u128) -> u128 {
    let f = [choose, parity, majority][op as usize];
    let (mut x, mut y) = (x, e);
    for i in 0..4 {
        let t = f(lane(x, 1), lane(x, 2), lane(x, 3));
        y = y
            .wrapping_add(lane(x, 0).rotate_left(5))
            .wrapping_add(t)
            .wrapping_add(lane(w, i));
        let b = (lane(x, 1).rotate_left(30) as u128) << 32;
        x = (x & !(0xffff_ffffu128 << 32)) | b;
        // <Y, X> = ROL(Y:X, 32)
        let top = lane(x, 3);
        x = (x << 32) | y as u128;
        y = top;
    }
    x
}

/// SHA1H: the fixed rotate.
pub fn sha1h(v: u32) -> u32 {
    v.rotate_left(30)
}

/// SHA1SU0: first half of the message schedule update.
pub fn sha1su0(d: u128, n: u128, m: u128) -> u128 {
    let r = (n << 64) | (d >> 64);
    r ^ d ^ m
}

/// SHA1SU1: second half of the message schedule update.
pub fn sha1su1(d: u128, n: u128) -> u128 {
    let t = d ^ (n >> 32);
    let w0 = lane(t, 0).rotate_left(1);
    let w1 = lane(t, 1).rotate_left(1);
    let w2 = lane(t, 2).rotate_left(1);
    let w3 = lane(t, 3).rotate_left(1) ^ lane(t, 0).rotate_left(2);
    lanes([w0, w1, w2, w3])
}

/// SHA256H (`part1`) and SHA256H2: four rounds; returns the new X or Y half.
pub fn sha256_hash(x: u128, y: u128, w: u128, part1: bool) -> u128 {
    let (mut x, mut y) = (x, y);
    for e in 0..4 {
        let chs = choose(lane(y, 0), lane(y, 1), lane(y, 2));
        let maj = majority(lane(x, 0), lane(x, 1), lane(x, 2));
        let y0 = lane(y, 0);
        let sigma1 = y0.rotate_right(6) ^ y0.rotate_right(11) ^ y0.rotate_right(25);
        let t = lane(y, 3)
            .wrapping_add(sigma1)
            .wrapping_add(chs)
            .wrapping_add(lane(w, e));
        let x3 = t.wrapping_add(lane(x, 3));
        let x0 = lane(x, 0);
        let sigma0 = x0.rotate_right(2) ^ x0.rotate_right(13) ^ x0.rotate_right(22);
        let y3 = t.wrapping_add(sigma0).wrapping_add(maj);
        x = (x & !(0xffff_ffffu128 << 96)) | ((x3 as u128) << 96);
        y = (y & !(0xffff_ffffu128 << 96)) | ((y3 as u128) << 96);
        // <Y, X> = ROL(Y:X, 32)
        let (xt, yt) = (lane(x, 3), lane(y, 3));
        x = (x << 32) | yt as u128;
        y = (y << 32) | xt as u128;
    }
    if part1 {
        x
    } else {
        y
    }
}

/// SHA256SU0.
pub fn sha256su0(d: u128, n: u128) -> u128 {
    let t = ((n as u32 as u128) << 96) | (d >> 32);
    let mut r = [0u32; 4];
    for (e, out) in r.iter_mut().enumerate() {
        let x = lane(t, e as u32);
        let s = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        *out = s.wrapping_add(lane(d, e as u32));
    }
    lanes(r)
}

/// SHA256SU1.
pub fn sha256su1(d: u128, n: u128, m: u128) -> u128 {
    let t0 = ((m as u32 as u128) << 96) | (n >> 32);
    let mut r = [0u32; 4];
    let sigma = |x: u32| x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10);
    for (e, out) in r[..2].iter_mut().enumerate() {
        let x = lane(m, e as u32 + 2);
        *out = sigma(x)
            .wrapping_add(lane(d, e as u32))
            .wrapping_add(lane(t0, e as u32));
    }
    for e in 2..4 {
        let x = r[e - 2];
        r[e] = sigma(x)
            .wrapping_add(lane(d, e as u32))
            .wrapping_add(lane(t0, e as u32));
    }
    lanes(r)
}

/// SM3 P1, the message permutation.
fn sm3_p1(x: u32) -> u32 {
    x ^ x.rotate_left(15) ^ x.rotate_left(23)
}

/// SM3SS1: SS1 = ((A <<< 12) + E + T) <<< 7 into the top lane, the rest cleared. `n` has A, `m` and
/// `a` have E and the rotated round constant (the sum does not care which is which).
pub fn sm3ss1(n: u128, m: u128, a: u128) -> u128 {
    let t = lane(n, 3)
        .rotate_left(12)
        .wrapping_add(lane(m, 3))
        .wrapping_add(lane(a, 3));
    (t.rotate_left(7) as u128) << 96
}

/// SM3TT1A (`op` 0), SM3TT1B (1), SM3TT2A (2), SM3TT2B (3): one round on the state `d` (A, B, C, D or
/// E, F, G, H from the top lane down), with SS1 (SS2 for TT1) from the top lane of `n` and the
/// word W'j (Wj for TT2) from lane `i` of `m`.
pub fn sm3tt(op: u32, d: u128, n: u128, m: u128, i: u32) -> u128 {
    let (a, b, c, e) = (lane(d, 3), lane(d, 2), lane(d, 1), lane(d, 0));
    let w = lane(m, i);
    if op < 2 {
        let ff = if op == 0 { parity(a, b, c) } else { majority(a, b, c) };
        let ss2 = lane(n, 3) ^ a.rotate_left(12);
        let tt1 = ff.wrapping_add(e).wrapping_add(ss2).wrapping_add(w);
        lanes([c, b.rotate_left(9), a, tt1])
    } else {
        let gg = if op == 2 { parity(a, b, c) } else { choose(a, b, c) };
        let tt2 = gg.wrapping_add(e).wrapping_add(lane(n, 3)).wrapping_add(w);
        let p0 = tt2 ^ tt2.rotate_left(9) ^ tt2.rotate_left(17);
        lanes([c, b.rotate_left(19), a, p0])
    }
}

/// SM3PARTW1: the first part of four message words W[j..j+4]. The top lane takes the permuted
/// lane 0, not yet complete: SM3PARTW2 repairs it.
pub fn sm3partw1(d: u128, n: u128, m: u128) -> u128 {
    let mut r = [0u32; 4];
    for (i, out) in r[..3].iter_mut().enumerate() {
        let i = i as u32;
        *out = sm3_p1(lane(d, i) ^ lane(n, i) ^ lane(m, i + 1).rotate_left(15));
    }
    r[3] = sm3_p1(lane(d, 3) ^ lane(n, 3) ^ r[0].rotate_left(15));
    lanes(r)
}

/// SM3PARTW2: the rest of the four message words.
pub fn sm3partw2(d: u128, n: u128, m: u128) -> u128 {
    let x = |i: u32| lane(n, i) ^ lane(m, i).rotate_left(7);
    let t = x(0);
    lanes([
        lane(d, 0) ^ t,
        lane(d, 1) ^ x(1),
        lane(d, 2) ^ x(2),
        lane(d, 3) ^ x(3) ^ sm3_p1(t.rotate_left(15)),
    ])
}

const SM4_SBOX: [u8; 256] = [
    0xd6, 0x90, 0xe9, 0xfe, 0xcc, 0xe1, 0x3d, 0xb7, 0x16, 0xb6, 0x14, 0xc2, 0x28, 0xfb, 0x2c, 0x05,
    0x2b, 0x67, 0x9a, 0x76, 0x2a, 0xbe, 0x04, 0xc3, 0xaa, 0x44, 0x13, 0x26, 0x49, 0x86, 0x06, 0x99,
    0x9c, 0x42, 0x50, 0xf4, 0x91, 0xef, 0x98, 0x7a, 0x33, 0x54, 0x0b, 0x43, 0xed, 0xcf, 0xac, 0x62,
    0xe4, 0xb3, 0x1c, 0xa9, 0xc9, 0x08, 0xe8, 0x95, 0x80, 0xdf, 0x94, 0xfa, 0x75, 0x8f, 0x3f, 0xa6,
    0x47, 0x07, 0xa7, 0xfc, 0xf3, 0x73, 0x17, 0xba, 0x83, 0x59, 0x3c, 0x19, 0xe6, 0x85, 0x4f, 0xa8,
    0x68, 0x6b, 0x81, 0xb2, 0x71, 0x64, 0xda, 0x8b, 0xf8, 0xeb, 0x0f, 0x4b, 0x70, 0x56, 0x9d, 0x35,
    0x1e, 0x24, 0x0e, 0x5e, 0x63, 0x58, 0xd1, 0xa2, 0x25, 0x22, 0x7c, 0x3b, 0x01, 0x21, 0x78, 0x87,
    0xd4, 0x00, 0x46, 0x57, 0x9f, 0xd3, 0x27, 0x52, 0x4c, 0x36, 0x02, 0xe7, 0xa0, 0xc4, 0xc8, 0x9e,
    0xea, 0xbf, 0x8a, 0xd2, 0x40, 0xc7, 0x38, 0xb5, 0xa3, 0xf7, 0xf2, 0xce, 0xf9, 0x61, 0x15, 0xa1,
    0xe0, 0xae, 0x5d, 0xa4, 0x9b, 0x34, 0x1a, 0x55, 0xad, 0x93, 0x32, 0x30, 0xf5, 0x8c, 0xb1, 0xe3,
    0x1d, 0xf6, 0xe2, 0x2e, 0x82, 0x66, 0xca, 0x60, 0xc0, 0x29, 0x23, 0xab, 0x0d, 0x53, 0x4e, 0x6f,
    0xd5, 0xdb, 0x37, 0x45, 0xde, 0xfd, 0x8e, 0x2f, 0x03, 0xff, 0x6a, 0x72, 0x6d, 0x6c, 0x5b, 0x51,
    0x8d, 0x1b, 0xaf, 0x92, 0xbb, 0xdd, 0xbc, 0x7f, 0x11, 0xd9, 0x5c, 0x41, 0x1f, 0x10, 0x5a, 0xd8,
    0x0a, 0xc1, 0x31, 0x88, 0xa5, 0xcd, 0x7b, 0xbd, 0x2d, 0x74, 0xd0, 0x12, 0xb8, 0xe5, 0xb4, 0xb0,
    0x89, 0x69, 0x97, 0x4a, 0x0c, 0x96, 0x77, 0x7e, 0x65, 0xb9, 0xf1, 0x09, 0xc5, 0x6e, 0xc6, 0x84,
    0x18, 0xf0, 0x7d, 0xec, 0x3a, 0xdc, 0x4d, 0x20, 0x79, 0xee, 0x5f, 0x3e, 0xd7, 0xcb, 0x39, 0x48,
];

/// The S-box applied to each byte of `x`.
fn sm4_tau(x: u32) -> u32 {
    u32::from_le_bytes(x.to_le_bytes().map(|b| SM4_SBOX[b as usize]))
}

/// Four rounds over the words of `state` (the oldest in lane 0), the keys coming from `key` and
/// `lin` being the linear transform: each new word enters at the top and the oldest leaves.
fn sm4_rounds(mut state: u128, key: u128, lin: fn(u32) -> u32) -> u128 {
    for i in 0..4 {
        let t = lin(sm4_tau(
            lane(state, 3) ^ lane(state, 2) ^ lane(state, 1) ^ lane(key, i),
        ));
        state = (state >> 32) | (((t ^ lane(state, 0)) as u128) << 96);
    }
    state
}

/// SM4E: four encryption rounds of the block `d` with the round keys `n`.
pub fn sm4e(d: u128, n: u128) -> u128 {
    sm4_rounds(d, n, |t| {
        t ^ t.rotate_left(2) ^ t.rotate_left(10) ^ t.rotate_left(18) ^ t.rotate_left(24)
    })
}

/// SM4EKEY: the four round keys after the four in `n`, from the constants `m`.
pub fn sm4ekey(n: u128, m: u128) -> u128 {
    sm4_rounds(n, m, |t| t ^ t.rotate_left(13) ^ t.rotate_left(23))
}

fn lo(v: u128) -> u64 {
    v as u64
}

fn hi(v: u128) -> u64 {
    (v >> 64) as u64
}

fn pair(lo: u64, hi: u64) -> u128 {
    (hi as u128) << 64 | lo as u128
}

fn choose64(x: u64, y: u64, z: u64) -> u64 {
    (x & y) ^ (!x & z)
}

fn majority64(x: u64, y: u64, z: u64) -> u64 {
    (x & y) ^ (x & z) ^ (y & z)
}

/// SHA512H: two rounds on (G, H) in `d` (the round constants and words already added), with (F, G)
/// in `n` and (D, E) in `m`; returns the two T1, the second in the low half.
pub fn sha512h(d: u128, n: u128, m: u128) -> u128 {
    let s1 = |x: u64| x.rotate_right(14) ^ x.rotate_right(18) ^ x.rotate_right(41);
    let t1 = hi(d)
        .wrapping_add(s1(hi(m)))
        .wrapping_add(choose64(hi(m), lo(n), hi(n)));
    let e = t1.wrapping_add(lo(m));
    let t2 = lo(d)
        .wrapping_add(s1(e))
        .wrapping_add(choose64(e, hi(m), lo(n)));
    pair(t2, t1)
}

/// SHA512H2: the two T1 in `d` become the two new A (the second in the low half), with C in `n`
/// and (A, B) in `m`.
pub fn sha512h2(d: u128, n: u128, m: u128) -> u128 {
    let s0 = |x: u64| x.rotate_right(28) ^ x.rotate_right(34) ^ x.rotate_right(39);
    let a1 = hi(d)
        .wrapping_add(s0(lo(m)))
        .wrapping_add(majority64(lo(n), hi(m), lo(m)));
    let a2 = lo(d)
        .wrapping_add(s0(a1))
        .wrapping_add(majority64(a1, lo(m), hi(m)));
    pair(a2, a1)
}

/// SHA512SU0: the first part of two message words.
pub fn sha512su0(d: u128, n: u128) -> u128 {
    let s0 = |x: u64| x.rotate_right(1) ^ x.rotate_right(8) ^ (x >> 7);
    pair(lo(d).wrapping_add(s0(hi(d))), hi(d).wrapping_add(s0(lo(n))))
}

/// SHA512SU1: the rest of two message words.
pub fn sha512su1(d: u128, n: u128, m: u128) -> u128 {
    let s1 = |x: u64| x.rotate_right(19) ^ x.rotate_right(61) ^ (x >> 6);
    pair(
        lo(d).wrapping_add(s1(lo(n))).wrapping_add(lo(m)),
        hi(d).wrapping_add(s1(hi(n))).wrapping_add(hi(m)),
    )
}

/// EOR3.
pub fn eor3(n: u128, m: u128, a: u128) -> u128 {
    n ^ m ^ a
}

/// BCAX.
pub fn bcax(n: u128, m: u128, a: u128) -> u128 {
    n ^ (m & !a)
}

/// RAX1: `n` xor `m` with each 64-bit lane rotated left by one.
pub fn rax1(n: u128, m: u128) -> u128 {
    pair(lo(n) ^ lo(m).rotate_left(1), hi(n) ^ hi(m).rotate_left(1))
}

/// XAR: each 64-bit lane of `n` xor `m`, rotated right by `imm`.
pub fn xar(n: u128, m: u128, imm: u32) -> u128 {
    let x = n ^ m;
    pair(lo(x).rotate_right(imm), hi(x).rotate_right(imm))
}

const fn crc_table(poly: u32) -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = (c >> 1) ^ if c & 1 != 0 { poly } else { 0 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const CRC32: [u32; 256] = crc_table(0xedb8_8320);
const CRC32C: [u32; 256] = crc_table(0x82f6_3b78);

/// CRC32{B,H,W,X} and CRC32C*: `bytes` low bytes of `data` into `acc` (bit-reflected, with no
/// inversion before or after, as the instructions define it).
pub fn crc32(acc: u32, data: u64, bytes: u32, castagnoli: bool) -> u32 {
    let t = if castagnoli { &CRC32C } else { &CRC32 };
    let mut c = acc;
    for i in 0..bytes {
        let b = (data >> (8 * i)) as u8;
        c = (c >> 8) ^ t[((c as u8) ^ b) as usize];
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(b: [u8; 16]) -> u128 {
        u128::from_le_bytes(b)
    }

    #[test]
    fn sbox_values() {
        assert_eq!(SBOX.0[0x00], 0x63);
        assert_eq!(SBOX.0[0x53], 0xed);
        assert_eq!(SBOX.0[0xff], 0x16);
        for i in 0..256 {
            assert_eq!(SBOX.1[SBOX.0[i] as usize] as usize, i);
        }
    }

    /// FIPS-197 appendix C.1 (AES-128), with the round structure ARMv8 code uses:
    /// AESE+AESMC for rounds 1..9, AESE and a final XOR for round 10.
    #[test]
    fn aes128_fips197() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let pt: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
        let expect = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        // Key expansion
        let mut w = [[0u8; 4]; 44];
        for i in 0..4 {
            w[i].copy_from_slice(&key[4 * i..4 * i + 4]);
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                t = [
                    SBOX.0[t[1] as usize] ^ rcon,
                    SBOX.0[t[2] as usize],
                    SBOX.0[t[3] as usize],
                    SBOX.0[t[0] as usize],
                ];
                rcon = xtime(rcon);
            }
            for k in 0..4 {
                w[i][k] = w[i - 4][k] ^ t[k];
            }
        }
        let rk = |r: usize| {
            let mut b = [0u8; 16];
            for c in 0..4 {
                b[4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
            block(b)
        };
        let mut s = block(pt);
        for r in 0..9 {
            s = aesmc(aese(s, rk(r)));
        }
        s = aese(s, rk(9)) ^ rk(10);
        assert_eq!(s, block(expect));
        // And back: AESD+AESIMC with InvMixColumns'd round keys.
        let mut s = s ^ rk(10);
        s = aesd(s, 0);
        for r in (1..10).rev() {
            s = aesimc(s ^ rk(r));
            s = aesd(s, 0);
        }
        assert_eq!(s ^ rk(0), block(pt));
    }

    #[test]
    fn crc_check_values() {
        let crc = |c: bool, s: &[u8]| !s.iter().fold(!0u32, |a, &b| crc32(a, b as u64, 1, c));
        assert_eq!(crc(false, b"123456789"), 0xcbf4_3926);
        assert_eq!(crc(true, b"123456789"), 0xe306_9283);
        let w = u64::from_le_bytes(*b"12345678");
        assert_eq!(
            crc32(!0, w, 8, false),
            (0..8).fold(!0, |a, i| crc32(a, w >> (8 * i), 1, false))
        );
    }

    /// One SHA-256 block of "abc", driven the way ARMv8 code drives the instructions.
    #[test]
    fn sha256_abc() {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut msg = [0u8; 64];
        msg[..3].copy_from_slice(b"abc");
        msg[3] = 0x80;
        msg[63] = 24;
        let mut m: [u128; 4] = core::array::from_fn(|i| {
            lanes(core::array::from_fn(|j| {
                u32::from_be_bytes(msg[16 * i + 4 * j..16 * i + 4 * j + 4].try_into().unwrap())
            }))
        });
        let abcd0 = lanes([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a]);
        let efgh0 = lanes([0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
        let (mut abcd, mut efgh) = (abcd0, efgh0);
        for i in 0..16 {
            let k = lanes(core::array::from_fn(|j| K[4 * i + j]));
            let wk = lanes(core::array::from_fn(|j| {
                lane(m[i % 4], j as u32).wrapping_add(lane(k, j as u32))
            }));
            let prev = abcd;
            abcd = sha256_hash(abcd, efgh, wk, true);
            efgh = sha256_hash(prev, efgh, wk, false);
            if i < 12 {
                let a = sha256su0(m[i % 4], m[(i + 1) % 4]);
                m[i % 4] = sha256su1(a, m[(i + 2) % 4], m[(i + 3) % 4]);
            }
        }
        let add = |a: u128, b: u128| {
            lanes(core::array::from_fn(|j| {
                lane(a, j as u32).wrapping_add(lane(b, j as u32))
            }))
        };
        let (abcd, efgh) = (add(abcd, abcd0), add(efgh, efgh0));
        let digest: Vec<u32> = (0..4)
            .map(|j| lane(abcd, j))
            .chain((0..4).map(|j| lane(efgh, j)))
            .collect();
        assert_eq!(
            digest,
            [
                0xba7816bf, 0x8f01cfea, 0x414140de, 0x5dae2223, 0xb00361a3, 0x96177a9c, 0xb410ff61,
                0xf20015ad
            ]
        );
    }

    /// One SHA-1 block of "abc".
    #[test]
    fn sha1_abc() {
        let mut msg = [0u8; 64];
        msg[..3].copy_from_slice(b"abc");
        msg[3] = 0x80;
        msg[63] = 24;
        let mut m: [u128; 4] = core::array::from_fn(|i| {
            lanes(core::array::from_fn(|j| {
                u32::from_be_bytes(msg[16 * i + 4 * j..16 * i + 4 * j + 4].try_into().unwrap())
            }))
        });
        let k = [0x5a827999u32, 0x6ed9eba1, 0x8f1bbcdc, 0xca62c1d6];
        let abcd0 = lanes([0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476]);
        let e0 = 0xc3d2e1f0u32;
        let (mut abcd, mut e) = (abcd0, e0);
        for i in 0..20 {
            let kk = k[i / 5];
            let wk = lanes(core::array::from_fn(|j| {
                lane(m[i % 4], j as u32).wrapping_add(kk)
            }));
            let next_e = sha1h(lane(abcd, 0));
            let op = match i / 5 {
                0 => 0,
                2 => 2,
                _ => 1,
            };
            abcd = sha1_rounds(op, abcd, e, wk);
            e = next_e;
            if i < 16 {
                let t = sha1su0(m[i % 4], m[(i + 1) % 4], m[(i + 2) % 4]);
                m[i % 4] = sha1su1(t, m[(i + 3) % 4]);
            }
        }
        let h: Vec<u32> = (0..4)
            .map(|j| lane(abcd, j).wrapping_add(lane(abcd0, j)))
            .chain([e.wrapping_add(e0)])
            .collect();
        assert_eq!(
            h,
            [0xa9993e36, 0x4706816a, 0xba3e2571, 0x7850c26c, 0x9cd0d89d]
        );
    }

    /// `a` bytes then `b` bytes from `byte` of `a` (EXT).
    fn ext(a: u128, b: u128, byte: u32) -> u128 {
        (a >> (8 * byte)) | (b << (128 - 8 * byte))
    }

    /// One SM3 block of "abc", driven the way the SM3 code in Linux does.
    #[test]
    fn sm3_abc() {
        let mut msg = [0u8; 64];
        msg[..3].copy_from_slice(b"abc");
        msg[3] = 0x80;
        msg[63] = 24;
        let mut ws: Vec<u128> = (0..4)
            .map(|i| {
                lanes(core::array::from_fn(|j| {
                    u32::from_be_bytes(msg[16 * i + 4 * j..16 * i + 4 * j + 4].try_into().unwrap())
                }))
            })
            .collect();
        for g in 0..13 {
            let (s0, s1, s2, s3) = (ws[g], ws[g + 1], ws[g + 2], ws[g + 3]);
            let t = sm3partw1(ext(s1, s2, 12), s0, s3);
            ws.push(sm3partw2(t, ext(s2, s3, 8), ext(s0, s1, 12)));
        }
        // A, B, C, D and E, F, G, H from the top lane down.
        let iv = [
            0x7380166fu32,
            0x4914b2b9,
            0x172442d7,
            0xda8a0600,
            0xa96f30bc,
            0x163138aa,
            0xe38dee4d,
            0xb0fb0e4e,
        ];
        let abcd = lanes([iv[3], iv[2], iv[1], iv[0]]);
        let efgh = lanes([iv[7], iv[6], iv[5], iv[4]]);
        let (mut x, mut y) = (abcd, efgh);
        for j in 0..64u32 {
            let t = if j < 16 { 0x79cc4519u32 } else { 0x7a879d8a };
            let tj = t.rotate_left(j % 32);
            let ss1 = sm3ss1(x, (tj as u128) << 96, y);
            let (s0, s1) = (ws[j as usize / 4], ws[j as usize / 4 + 1]);
            let b = (j >= 16) as u32;
            x = sm3tt(b, x, ss1, s0 ^ s1, j % 4);
            y = sm3tt(2 + b, y, ss1, s0, j % 4);
        }
        let out: Vec<u8> = [x ^ abcd, y ^ efgh]
            .iter()
            .flat_map(|v| (0..4).rev().flat_map(move |i| lane(*v, i).to_be_bytes()))
            .collect();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0"
        );
    }

    /// The block of the SM4 standard (key and plaintext 0123456789abcdeffedcba9876543210), with the key
    /// schedule from SM4EKEY and the rounds from SM4E.
    #[test]
    fn sm4_block() {
        let words = |b: [u8; 16]| -> [u32; 4] {
            core::array::from_fn(|i| u32::from_be_bytes(b[4 * i..4 * i + 4].try_into().unwrap()))
        };
        let block = [
            0x01u8, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let fk = [0xa3b1bac6u32, 0x56aa3350, 0x677d9197, 0xb27022dc];
        let key = words(block);
        let mut k = lanes(core::array::from_fn(|i| key[i] ^ fk[i]));
        let mut rk = Vec::new();
        for i in 0..8u32 {
            // CK[j] has the bytes (7 * j + 7 * k) for k in 0..4 from the top
            let ck = lanes(core::array::from_fn(|j| {
                u32::from_be_bytes(core::array::from_fn(|b| {
                    ((4 * i + j as u32) * 4 + b as u32).wrapping_mul(7) as u8
                }))
            }));
            k = sm4ekey(k, ck);
            rk.push(k);
        }
        let mut x = lanes(words(block));
        for r in &rk {
            x = sm4e(x, *r);
        }
        // Y = X35, X34, X33, X32
        let out: Vec<u8> = (0..4)
            .rev()
            .flat_map(|i| lane(x, i).to_be_bytes())
            .collect();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "681edf34d206965e86b3e94f536e4246");
    }

    /// One SHA-512 block of "abc": the state in four registers (A, B), (C, D), (E, F), (G, H), as
    /// the SHA-512 code in Linux holds it, and the schedule two words to a register.
    #[test]
    fn sha512_abc() {
        const K: [u64; 80] = [
            0x428a2f98d728ae22,
            0x7137449123ef65cd,
            0xb5c0fbcfec4d3b2f,
            0xe9b5dba58189dbbc,
            0x3956c25bf348b538,
            0x59f111f1b605d019,
            0x923f82a4af194f9b,
            0xab1c5ed5da6d8118,
            0xd807aa98a3030242,
            0x12835b0145706fbe,
            0x243185be4ee4b28c,
            0x550c7dc3d5ffb4e2,
            0x72be5d74f27b896f,
            0x80deb1fe3b1696b1,
            0x9bdc06a725c71235,
            0xc19bf174cf692694,
            0xe49b69c19ef14ad2,
            0xefbe4786384f25e3,
            0x0fc19dc68b8cd5b5,
            0x240ca1cc77ac9c65,
            0x2de92c6f592b0275,
            0x4a7484aa6ea6e483,
            0x5cb0a9dcbd41fbd4,
            0x76f988da831153b5,
            0x983e5152ee66dfab,
            0xa831c66d2db43210,
            0xb00327c898fb213f,
            0xbf597fc7beef0ee4,
            0xc6e00bf33da88fc2,
            0xd5a79147930aa725,
            0x06ca6351e003826f,
            0x142929670a0e6e70,
            0x27b70a8546d22ffc,
            0x2e1b21385c26c926,
            0x4d2c6dfc5ac42aed,
            0x53380d139d95b3df,
            0x650a73548baf63de,
            0x766a0abb3c77b2a8,
            0x81c2c92e47edaee6,
            0x92722c851482353b,
            0xa2bfe8a14cf10364,
            0xa81a664bbc423001,
            0xc24b8b70d0f89791,
            0xc76c51a30654be30,
            0xd192e819d6ef5218,
            0xd69906245565a910,
            0xf40e35855771202a,
            0x106aa07032bbd1b8,
            0x19a4c116b8d2d0c8,
            0x1e376c085141ab53,
            0x2748774cdf8eeb99,
            0x34b0bcb5e19b48a8,
            0x391c0cb3c5c95a63,
            0x4ed8aa4ae3418acb,
            0x5b9cca4f7763e373,
            0x682e6ff3d6b2b8a3,
            0x748f82ee5defb2fc,
            0x78a5636f43172f60,
            0x84c87814a1f0ab72,
            0x8cc702081a6439ec,
            0x90befffa23631e28,
            0xa4506cebde82bde9,
            0xbef9a3f7b2c67915,
            0xc67178f2e372532b,
            0xca273eceea26619c,
            0xd186b8c721c0c207,
            0xeada7dd6cde0eb1e,
            0xf57d4f7fee6ed178,
            0x06f067aa72176fba,
            0x0a637dc5a2c898a6,
            0x113f9804bef90dae,
            0x1b710b35131c471b,
            0x28db77f523047d84,
            0x32caab7b40c72493,
            0x3c9ebe0a15c9bebc,
            0x431d67c49c100d4c,
            0x4cc5d4becb3e42b6,
            0x597f299cfc657e2a,
            0x5fcb6fab3ad6faec,
            0x6c44198c4a475817,
        ];
        let mut msg = [0u8; 128];
        msg[..3].copy_from_slice(b"abc");
        msg[3] = 0x80;
        msg[127] = 24;
        let mut w: Vec<u128> = (0..8)
            .map(|i| {
                let word = |j: usize| {
                    u64::from_be_bytes(msg[16 * i + 8 * j..16 * i + 8 * j + 8].try_into().unwrap())
                };
                pair(word(0), word(1))
            })
            .collect();
        for k in 0..32 {
            let t = sha512su0(w[k], w[k + 1]);
            w.push(sha512su1(t, w[k + 7], ext(w[k + 4], w[k + 5], 8)));
        }
        let iv = [
            0x6a09e667f3bcc908u64,
            0xbb67ae8584caa73b,
            0x3c6ef372fe94f82b,
            0xa54ff53a5f1d36f1,
            0x510e527fade682d1,
            0x9b05688c2b3e6c1f,
            0x1f83d9abfb41bd6b,
            0x5be0cd19137e2179,
        ];
        let init = [0, 2, 4, 6].map(|i| pair(iv[i], iv[i + 1]));
        let [mut ab, mut cd, mut ef, mut gh] = init;
        for t in 0..40 {
            let kw0 = K[2 * t].wrapping_add(lo(w[t]));
            let kw1 = K[2 * t + 1].wrapping_add(hi(w[t]));
            let gh_kw = pair(lo(gh).wrapping_add(kw1), hi(gh).wrapping_add(kw0));
            let t1 = sha512h(gh_kw, ext(ef, gh, 8), ext(cd, ef, 8));
            let new_ef = pair(lo(cd).wrapping_add(lo(t1)), hi(cd).wrapping_add(hi(t1)));
            // The new A and B; (A, B) and (E, F) move down.
            let new_ab = sha512h2(t1, cd, ab);
            (ab, cd, ef, gh) = (new_ab, ab, new_ef, ef);
        }
        let out: Vec<u8> = [ab, cd, ef, gh]
            .iter()
            .zip(init)
            .flat_map(|(s, i)| {
                let (a, b) = (lo(*s).wrapping_add(lo(i)), hi(*s).wrapping_add(hi(i)));
                [a.to_be_bytes(), b.to_be_bytes()]
            })
            .flatten()
            .collect();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    /// SHA3-256 of "abc": Keccak-f[1600] with the theta column parities from EOR3, the D values from
    /// RAX1, rho from XAR and chi from BCAX (the lane in the low half, as 64-bit Keccak code does).
    #[test]
    fn sha3_256_abc() {
        const RHO: [u32; 25] = [
            0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61,
            56, 14,
        ];
        const RC: [u64; 24] = [
            0x1,
            0x8082,
            0x800000000000808a,
            0x8000000080008000,
            0x808b,
            0x80000001,
            0x8000000080008081,
            0x8000000000008009,
            0x8a,
            0x88,
            0x80008009,
            0x8000000a,
            0x8000808b,
            0x800000000000008b,
            0x8000000000008089,
            0x8000000000008003,
            0x8000000000008002,
            0x8000000000000080,
            0x800a,
            0x800000008000000a,
            0x8000000080008081,
            0x8000000000008080,
            0x80000001,
            0x8000000080008008,
        ];
        let mut msg = [0u8; 136];
        msg[..3].copy_from_slice(b"abc");
        msg[3] = 0x06;
        msg[135] |= 0x80;
        let mut a: [u128; 25] = [0; 25];
        for (i, l) in a.iter_mut().take(17).enumerate() {
            *l = u64::from_le_bytes(msg[8 * i..8 * i + 8].try_into().unwrap()) as u128;
        }
        for rc in RC {
            let c: [u128; 5] = core::array::from_fn(|x| {
                eor3(eor3(a[x], a[x + 5], a[x + 10]), a[x + 15], a[x + 20])
            });
            let d: [u128; 5] = core::array::from_fn(|x| rax1(c[(x + 4) % 5], c[(x + 1) % 5]));
            let mut b = [0u128; 25];
            for x in 0..5 {
                for y in 0..5 {
                    b[y + 5 * ((2 * x + 3 * y) % 5)] =
                        xar(a[x + 5 * y], d[x], (64 - RHO[x + 5 * y]) % 64);
                }
            }
            for x in 0..5 {
                for y in 0..5 {
                    a[x + 5 * y] =
                        bcax(b[x + 5 * y], b[(x + 2) % 5 + 5 * y], b[(x + 1) % 5 + 5 * y]);
                }
            }
            a[0] ^= rc as u128;
        }
        let hex: String = a[..4]
            .iter()
            .flat_map(|l| (*l as u64).to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
    }

    /// The two vector lanes are independent for RAX1 and XAR, and the three-operand forms are what
    /// their names say.
    #[test]
    fn sha3_operations() {
        let (n, m, a) = (
            0x0123456789abcdeffedcba9876543210u128,
            0xdeadbeefcafef00d0f1e2d3c4b5a6978u128,
            0x00ff00ff00ff00ff5a5a5a5aa5a5a5a5u128,
        );
        assert_eq!(eor3(n, m, a), n ^ m ^ a);
        assert_eq!(bcax(n, m, a), n ^ (m & !a));
        assert_eq!(
            rax1(n, m),
            pair(lo(n) ^ lo(m).rotate_left(1), hi(n) ^ hi(m).rotate_left(1))
        );
        assert_eq!(
            xar(n, m, 17),
            pair(
                (lo(n) ^ lo(m)).rotate_right(17),
                (hi(n) ^ hi(m)).rotate_right(17)
            )
        );
        assert_eq!(xar(n, m, 0), n ^ m);
    }

    struct NoDevices;
    impl crate::board::Host for NoDevices {
        fn mmio_read(&mut self, _: u64, _: u32) -> Option<u64> {
            None
        }
        fn mmio_write(&mut self, _: u64, _: u32, _: u64) -> bool {
            false
        }
        fn now_ns(&mut self) -> u64 {
            0
        }
        fn wall_clock_secs(&mut self) -> u64 {
            0
        }
    }

    /// Each instruction word, as the interpreter decodes it, does what its function does with the
    /// registers the word names; the words next to them stay undefined.
    #[test]
    fn the_instruction_words_reach_their_functions() {
        let mut c = crate::cpu::Cpu::new(1 << 20, NoDevices);
        c.sys.cpacr = 3 << 20;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for r in 0..32 {
            c.vreg[r] = (next() as u128) << 64 | next() as u128;
        }
        const RN: u32 = 5 << 5;
        const RM: u32 = 6 << 16;
        const RA: u32 = 7 << 10;
        // The word without its registers, the register fields it has (Rd is v4, Rn v5, Rm v6, Ra v7) and
        // its result from (Vd, Vn, Vm, Va).
        type Case = (u32, u32, fn(u128, u128, u128, u128) -> u128);
        let cases: [Case; 19] = [
            (0xce00_0000, RN | RM | RA, |_, n, m, a| eor3(n, m, a)),
            (0xce20_0000, RN | RM | RA, |_, n, m, a| bcax(n, m, a)),
            (0xce40_0000, RN | RM | RA, |_, n, m, a| sm3ss1(n, m, a)),
            (0xce40_8000, RN | RM, |d, n, m, _| sm3tt(0, d, n, m, 0)),
            (0xce40_9400, RN | RM, |d, n, m, _| sm3tt(1, d, n, m, 1)),
            (0xce40_a800, RN | RM, |d, n, m, _| sm3tt(2, d, n, m, 2)),
            (0xce40_bc00, RN | RM, |d, n, m, _| sm3tt(3, d, n, m, 3)),
            (0xce60_8000, RN | RM, |d, n, m, _| sha512h(d, n, m)),
            (0xce60_8400, RN | RM, |d, n, m, _| sha512h2(d, n, m)),
            (0xce60_8800, RN | RM, |d, n, m, _| sha512su1(d, n, m)),
            (0xce60_8c00, RN | RM, |_, n, m, _| rax1(n, m)),
            (0xce60_c000, RN | RM, |d, n, m, _| sm3partw1(d, n, m)),
            (0xce60_c400, RN | RM, |d, n, m, _| sm3partw2(d, n, m)),
            (0xce60_c800, RN | RM, |_, n, m, _| sm4ekey(n, m)),
            (0xce80_0000, RN | RM, |_, n, m, _| xar(n, m, 0)),
            (0xce80_fc00, RN | RM, |_, n, m, _| xar(n, m, 63)),
            (0xce80_5400, RN | RM, |_, n, m, _| xar(n, m, 21)),
            (0xcec0_8000, RN, |d, n, _, _| sha512su0(d, n)),
            (0xcec0_8400, RN, |d, n, _, _| sm4e(d, n)),
        ];
        for (word, fields, f) in cases {
            let w = word | 4 | fields;
            let want = f(c.vreg[4], c.vreg[5], c.vreg[6], c.vreg[7]);
            c.execute(w)
                .unwrap_or_else(|_| panic!("{w:#010x} is undefined"));
            assert_eq!(c.vreg[4], want, "{w:#010x}");
            c.vreg[4] = (next() as u128) << 64 | next() as u128;
        }
        // The instruction a node program stopped on: sm3partw1 v4.4s, v0.4s, v3.4s
        let want = sm3partw1(c.vreg[4], c.vreg[0], c.vreg[3]);
        c.execute(0xce63_c004).unwrap();
        assert_eq!(c.vreg[4], want);
        // Unallocated: bit 15 set in a four-register form, the fourth SM3/SM4 opcode, a SM4E with an Rm.
        let before = c.vreg[4];
        for w in [
            0xce00_8004u32,
            0xce60_cc04,
            0xcec1_8404,
            0xcec0_8804,
            0xce60_0004,
        ] {
            assert!(c.execute(w).is_err(), "{w:#010x} is allocated");
            assert_eq!(c.vreg[4], before);
        }
    }
}
