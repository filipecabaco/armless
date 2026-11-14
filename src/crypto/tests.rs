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
