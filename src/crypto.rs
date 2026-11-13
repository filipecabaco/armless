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
mod tests;
