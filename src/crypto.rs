//! The Cryptographic Extension (AES, SHA1, SHA256) and CRC32, as the Arm ARM's pseudocode defines
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
    for e in 0..2 {
        let x = lane(m, e as u32 + 2);
        r[e] = sigma(x)
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
}
