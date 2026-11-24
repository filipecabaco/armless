//! RAM above 4 GiB in a browser: each 4 GiB bank of it (`board::HIGH_RAM_BASE` on) is a wasm
//! memory of its own, which this module's code can't address (Rust compiles to memory 0 only).
//! The host instantiates a small module made here (`module`) that imports memory 0 and the banks'
//! memories, and puts its functions in the function table at `SLOT_BASE` on, where `Ram` calls
//! them: loads, stores, copies to and from memory 0, fills and atomics, each on the bank its first
//! argument names (1, 2, ...), at the offset its second names.

/// Where the bank functions are in the function table: after the JIT's slots.
pub const SLOT_BASE: u32 = crate::jit::SLOT_BASE + crate::jit::SLOTS;

/// The functions, by their index past SLOT_BASE (and their export name, "f" and the index).
pub const LOAD: u32 = 0; // + log2(size): (bank, off) -> i64
pub const STORE: u32 = 4; // + log2(size): (bank, off, i64)
pub const COPY_OUT: u32 = 8; // (bank, off, to in memory 0, len)
pub const COPY_IN: u32 = 9; // (bank, off, from in memory 0, len)
pub const FILL_ZERO: u32 = 10; // (bank, off, len)
pub const ATOMIC_LOAD: u32 = 11; // + log2(size): (bank, off) -> i64
pub const CMPXCHG: u32 = 15; // + log2(size): (bank, off, expected, new) -> old
pub const FUNCTIONS: u32 = 19;

const I32: u8 = 0x7f;
const I64: u8 = 0x7e;

fn leb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    leb(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn name(out: &mut Vec<u8>, s: &str) {
    leb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// A memory access's immediate: alignment (log2), memory and offset 0.
fn memarg(out: &mut Vec<u8>, align: u32, mem: u32) {
    if mem == 0 {
        leb(out, align as u64);
    } else {
        leb(out, (align | 0x40) as u64);
        leb(out, mem as u64);
    }
    out.push(0);
}

/// The function types, by index: (params, results).
const TYPES: [(&[u8], &[u8]); 5] = [
    (&[I32, I32], &[I64]),           // 0: load
    (&[I32, I32, I64], &[]),         // 1: store
    (&[I32, I32, I32, I32], &[]),    // 2: copies
    (&[I32, I32, I32], &[]),         // 3: fill
    (&[I32, I32, I64, I64], &[I64]), // 4: compare-and-swap
];

/// The type of function `f`, and its code on memory `mem`.
fn function(f: u32, mem: u32, code: &mut Vec<u8>) -> u32 {
    let get = |code: &mut Vec<u8>, i: u8| code.extend_from_slice(&[0x20, i]);
    match f {
        LOAD..=3 => {
            let size = f - LOAD;
            get(code, 1);
            code.push([0x31, 0x33, 0x35, 0x29][size as usize]); // i64.load8_u .. i64.load
            memarg(code, size, mem);
            0
        },
        STORE..=7 => {
            let size = f - STORE;
            get(code, 1);
            get(code, 2);
            code.push([0x3c, 0x3d, 0x3e, 0x37][size as usize]); // i64.store8 .. i64.store
            memarg(code, size, mem);
            1
        },
        COPY_OUT | COPY_IN => {
            // memory.copy: to, from, len; then the memories, to first
            let (to, from, (dst, src)) = if f == COPY_OUT { (2, 1, (0, mem)) } else { (1, 2, (mem, 0)) };
            get(code, to);
            get(code, from);
            get(code, 3);
            code.extend_from_slice(&[0xfc, 0x0a]);
            leb(code, dst as u64);
            leb(code, src as u64);
            2
        },
        FILL_ZERO => {
            get(code, 1);
            code.extend_from_slice(&[0x41, 0]); // i32.const 0
            get(code, 2);
            code.extend_from_slice(&[0xfc, 0x0b]);
            leb(code, mem as u64);
            3
        },
        ATOMIC_LOAD..=14 => {
            let size = f - ATOMIC_LOAD;
            get(code, 1);
            code.extend_from_slice(&[0xfe, [0x14, 0x15, 0x16, 0x11][size as usize]]);
            memarg(code, size, mem);
            0
        },
        _ => {
            let size = f - CMPXCHG;
            get(code, 1);
            get(code, 2);
            get(code, 3);
            code.extend_from_slice(&[0xfe, [0x4c, 0x4d, 0x4e, 0x49][size as usize]]);
            memarg(code, size, mem);
            4
        },
    }
}

/// The module for `banks` banks, importing memory 0 as "e"."m0" and bank k's as "e"."m{k}", all
/// shared (the threaded build) or none, and exporting function i as "f{i}".
pub fn module(banks: u32, shared: bool) -> Vec<u8> {
    assert!(banks >= 1);
    let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    let mut s = Vec::new();
    leb(&mut s, TYPES.len() as u64);
    for (params, results) in TYPES {
        s.push(0x60);
        leb(&mut s, params.len() as u64);
        s.extend_from_slice(params);
        leb(&mut s, results.len() as u64);
        s.extend_from_slice(results);
    }
    section(&mut out, 1, &s);

    let mut s = Vec::new();
    leb(&mut s, banks as u64 + 1);
    for m in 0..=banks {
        name(&mut s, "e");
        name(&mut s, &format!("m{m}"));
        s.push(0x02);
        // A shared memory has a maximum (4 GiB here, as the machine's); the single-threaded
        // build's own has none, so none is asked of any
        if shared {
            s.push(0x03);
            leb(&mut s, 0);
            leb(&mut s, 65536);
        } else {
            s.push(0x00);
            leb(&mut s, 0);
        }
    }
    section(&mut out, 2, &s);

    // Each function: a block per bank, a br_table on the bank to the end of its block, and there
    // the access on that bank's memory
    let mut types = Vec::new();
    let mut bodies = Vec::new();
    for f in 0..FUNCTIONS {
        let mut code = vec![0]; // no locals
        code.extend(std::iter::repeat_n([0x02, 0x40], banks as usize).flatten());
        code.extend_from_slice(&[0x20, 0, 0x41, 1, 0x6b]); // bank - 1
        code.push(0x0e);
        leb(&mut code, banks as u64 - 1);
        (0..banks).for_each(|d| leb(&mut code, d as u64));
        leb(&mut code, banks as u64 - 1);
        let mut ty = 0;
        for bank in 1..=banks {
            code.push(0x0b);
            ty = function(f, bank, &mut code);
            code.push(0x0f); // return
        }
        code.push(0x0b);
        types.push(ty);
        bodies.push(code);
    }
    let mut s = Vec::new();
    leb(&mut s, FUNCTIONS as u64);
    types.iter().for_each(|&t| leb(&mut s, t as u64));
    section(&mut out, 3, &s);

    let mut s = Vec::new();
    leb(&mut s, FUNCTIONS as u64);
    for f in 0..FUNCTIONS {
        name(&mut s, &format!("f{f}"));
        s.push(0x00);
        leb(&mut s, f as u64);
    }
    section(&mut out, 7, &s);

    let mut s = Vec::new();
    leb(&mut s, FUNCTIONS as u64);
    for b in &bodies {
        leb(&mut s, b.len() as u64);
        s.extend_from_slice(b);
    }
    section(&mut out, 10, &s);
    out
}

/// Banks of 4 GiB RAM of `len` bytes needs above 4 GiB.
pub fn needed(len: u64) -> u32 {
    let high = crate::board::ram_end(len).saturating_sub(crate::board::HIGH_RAM_BASE);
    high.div_ceil(1 << 32) as u32
}

/// In the browser: the bank functions, called through the function table.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
pub mod call {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// How many banks the host made (`armless_set_banks`).
    pub static BANKS: AtomicU32 = AtomicU32::new(0);

    pub fn banks() -> u32 {
        BANKS.load(Ordering::Relaxed)
    }

    /// The function at `i` past SLOT_BASE, as a pointer of type `F` (a function pointer is its
    /// table index in wasm).
    ///
    /// # Safety
    ///
    /// `F` must be the function's type, and the host must have put it in the table (`banks()` is
    /// more than 0).
    #[inline(always)]
    unsafe fn f<F: Copy>(i: u32) -> F {
        debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
        let index = (super::SLOT_BASE + i) as usize;
        // SAFETY: a function pointer is a table index here, and the caller vouches for its type
        unsafe { std::mem::transmute_copy(&index) }
    }

    #[inline(always)]
    pub fn load(bank: u32, off: u32, size: u32) -> u64 {
        // SAFETY: LOAD + log2(size) is (bank, off) -> i64, put there before any RAM above 4 GiB
        unsafe { f::<extern "C" fn(u32, u32) -> u64>(super::LOAD + size.trailing_zeros())(bank, off) }
    }

    #[inline(always)]
    pub fn store(bank: u32, off: u32, size: u32, value: u64) {
        // SAFETY: as for `load`
        unsafe {
            f::<extern "C" fn(u32, u32, u64)>(super::STORE + size.trailing_zeros())(bank, off, value)
        }
    }

    pub fn copy_out(bank: u32, off: u32, to: *mut u8, len: u32) {
        // SAFETY: as for `load`
        unsafe { f::<extern "C" fn(u32, u32, *mut u8, u32)>(super::COPY_OUT)(bank, off, to, len) }
    }

    pub fn copy_in(bank: u32, off: u32, from: *const u8, len: u32) {
        // SAFETY: as for `load`
        unsafe { f::<extern "C" fn(u32, u32, *const u8, u32)>(super::COPY_IN)(bank, off, from, len) }
    }

    pub fn fill_zero(bank: u32, off: u32, len: u32) {
        // SAFETY: as for `load`
        unsafe { f::<extern "C" fn(u32, u32, u32)>(super::FILL_ZERO)(bank, off, len) }
    }

    pub fn atomic_load(bank: u32, off: u32, size: u32) -> u64 {
        // SAFETY: as for `load`
        unsafe {
            f::<extern "C" fn(u32, u32) -> u64>(super::ATOMIC_LOAD + size.trailing_zeros())(bank, off)
        }
    }

    /// The old value.
    pub fn cmpxchg(bank: u32, off: u32, size: u32, expected: u64, new: u64) -> u64 {
        // SAFETY: as for `load`
        unsafe {
            f::<extern "C" fn(u32, u32, u64, u64) -> u64>(super::CMPXCHG + size.trailing_zeros())(
                bank, off, expected, new,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banks_needed_count_4_gib_each_above_the_hole() {
        assert_eq!(needed(512 << 20), 0);
        assert_eq!(needed(3 << 30), 0);
        assert_eq!(needed((3 << 30) + (1 << 20)), 1);
        assert_eq!(needed(7 << 30), 1);
        assert_eq!(needed((7 << 30) + (1 << 20)), 2);
        assert_eq!(needed(crate::board::RAM_MAX), 15);
    }

    #[test]
    fn the_module_has_its_functions_and_imports() {
        let m = module(2, true);
        assert_eq!(&m[..8], b"\0asm\x01\0\0\0");
        let s = String::from_utf8_lossy(&m);
        assert!(s.contains("m0") && s.contains("m2") && !s.contains("m3"));
        assert!(s.contains("f0") && s.contains("f18") && !s.contains("f19"));
    }
}
