//! A minimal WebAssembly binary emitter for JIT-compiled blocks.
//!
//! Each block becomes a module importing the main module's memory and a few
//! helper functions, exporting one function `f: (pc: i64) -> i32`.

/// Value types.
pub const I32: u8 = 0x7f;
pub const I64: u8 = 0x7e;

/// Block types.
pub const EMPTY: u8 = 0x40;

pub fn leb_u(out: &mut Vec<u8>, mut v: u64) {
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

pub fn leb_s(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        if done {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn name(out: &mut Vec<u8>, s: &str) {
    leb_u(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    leb_u(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// A function signature.
#[derive(Clone, PartialEq, Eq)]
pub struct FuncType {
    pub params: Vec<u8>,
    pub results: Vec<u8>,
}

/// An imported helper function.
pub struct Import {
    pub name: &'static str,
    pub ty: FuncType,
}

/// Function body builder.
#[derive(Default)]
pub struct Code {
    pub b: Vec<u8>,
}

#[allow(dead_code)]
impl Code {
    pub fn new() -> Code {
        Code::default()
    }

    // control
    pub fn block(&mut self, ty: u8) {
        self.b.push(0x02);
        self.b.push(ty);
    }
    pub fn loop_(&mut self, ty: u8) {
        self.b.push(0x03);
        self.b.push(ty);
    }
    pub fn if_(&mut self, ty: u8) {
        self.b.push(0x04);
        self.b.push(ty);
    }
    pub fn else_(&mut self) {
        self.b.push(0x05);
    }
    pub fn end(&mut self) {
        self.b.push(0x0b);
    }
    pub fn br(&mut self, depth: u32) {
        self.b.push(0x0c);
        leb_u(&mut self.b, depth as u64);
    }
    pub fn br_if(&mut self, depth: u32) {
        self.b.push(0x0d);
        leb_u(&mut self.b, depth as u64);
    }
    pub fn br_table(&mut self, targets: &[u32], default: u32) {
        self.b.push(0x0e);
        leb_u(&mut self.b, targets.len() as u64);
        for &t in targets {
            leb_u(&mut self.b, t as u64);
        }
        leb_u(&mut self.b, default as u64);
    }
    pub fn ret(&mut self) {
        self.b.push(0x0f);
    }
    pub fn call(&mut self, func: u32) {
        self.b.push(0x10);
        leb_u(&mut self.b, func as u64);
    }
    pub fn drop_(&mut self) {
        self.b.push(0x1a);
    }
    /// atomic.fence (threads proposal)
    pub fn atomic_fence(&mut self) {
        self.b.extend_from_slice(&[0xfe, 0x03, 0x00]);
    }
    pub fn select(&mut self) {
        self.b.push(0x1b);
    }

    // locals
    pub fn get(&mut self, l: u32) {
        self.b.push(0x20);
        leb_u(&mut self.b, l as u64);
    }
    pub fn set(&mut self, l: u32) {
        self.b.push(0x21);
        leb_u(&mut self.b, l as u64);
    }
    pub fn tee(&mut self, l: u32) {
        self.b.push(0x22);
        leb_u(&mut self.b, l as u64);
    }

    // memory: `addr` is the static offset; the dynamic address is on the stack
    fn mem(&mut self, op: u8, align: u32, offset: u32) {
        self.b.push(op);
        leb_u(&mut self.b, align as u64);
        leb_u(&mut self.b, offset as u64);
    }
    pub fn i32_load(&mut self, off: u32) {
        self.mem(0x28, 2, off)
    }
    pub fn i64_load(&mut self, off: u32) {
        self.mem(0x29, 3, off)
    }
    pub fn i32_load8_u(&mut self, off: u32) {
        self.mem(0x2d, 0, off)
    }
    pub fn i64_load8_u(&mut self, off: u32) {
        self.mem(0x31, 0, off)
    }
    pub fn i64_load8_s(&mut self, off: u32) {
        self.mem(0x30, 0, off)
    }
    pub fn i64_load16_u(&mut self, off: u32) {
        self.mem(0x33, 1, off)
    }
    pub fn i64_load16_s(&mut self, off: u32) {
        self.mem(0x32, 1, off)
    }
    pub fn i64_load32_u(&mut self, off: u32) {
        self.mem(0x35, 2, off)
    }
    pub fn i64_load32_s(&mut self, off: u32) {
        self.mem(0x34, 2, off)
    }
    pub fn i32_store(&mut self, off: u32) {
        self.mem(0x36, 2, off)
    }
    pub fn i64_store(&mut self, off: u32) {
        self.mem(0x37, 3, off)
    }
    pub fn i32_clz(&mut self) {
        self.op(0x67)
    }
    pub fn i32_store8(&mut self, off: u32) {
        self.mem(0x3a, 0, off)
    }
    pub fn i64_store8(&mut self, off: u32) {
        self.mem(0x3c, 0, off)
    }
    pub fn i64_store16(&mut self, off: u32) {
        self.mem(0x3d, 1, off)
    }
    pub fn i64_store32(&mut self, off: u32) {
        self.mem(0x3e, 2, off)
    }

    // constants
    pub fn i32c(&mut self, v: i32) {
        self.b.push(0x41);
        leb_s(&mut self.b, v as i64);
    }
    pub fn i64c(&mut self, v: i64) {
        self.b.push(0x42);
        leb_s(&mut self.b, v);
    }

    fn op(&mut self, o: u8) {
        self.b.push(o);
    }

    // i32 ops
    pub fn i32_eqz(&mut self) {
        self.op(0x45)
    }
    pub fn i32_eq(&mut self) {
        self.op(0x46)
    }
    pub fn i32_ne(&mut self) {
        self.op(0x47)
    }
    pub fn i32_lt_u(&mut self) {
        self.op(0x49)
    }
    pub fn i32_le_u(&mut self) {
        self.op(0x4d)
    }
    pub fn i32_add(&mut self) {
        self.op(0x6a)
    }
    pub fn i32_sub(&mut self) {
        self.op(0x6b)
    }
    pub fn i32_and(&mut self) {
        self.op(0x71)
    }
    pub fn i32_or(&mut self) {
        self.op(0x72)
    }
    pub fn i32_xor(&mut self) {
        self.op(0x73)
    }
    pub fn i32_shl(&mut self) {
        self.op(0x74)
    }
    pub fn i32_shr_u(&mut self) {
        self.op(0x76)
    }

    // i64 ops
    pub fn i64_eqz(&mut self) {
        self.op(0x50)
    }
    pub fn i64_eq(&mut self) {
        self.op(0x51)
    }
    pub fn i64_ne(&mut self) {
        self.op(0x52)
    }
    pub fn i64_lt_s(&mut self) {
        self.op(0x53)
    }
    pub fn i64_lt_u(&mut self) {
        self.op(0x54)
    }
    pub fn i64_gt_s(&mut self) {
        self.op(0x55)
    }
    pub fn i64_gt_u(&mut self) {
        self.op(0x56)
    }
    pub fn i64_le_s(&mut self) {
        self.op(0x57)
    }
    pub fn i64_le_u(&mut self) {
        self.op(0x58)
    }
    pub fn i64_ge_s(&mut self) {
        self.op(0x59)
    }
    pub fn i64_ge_u(&mut self) {
        self.op(0x5a)
    }
    pub fn i64_clz(&mut self) {
        self.op(0x79)
    }
    pub fn i64_add(&mut self) {
        self.op(0x7c)
    }
    pub fn i64_sub(&mut self) {
        self.op(0x7d)
    }
    pub fn i64_mul(&mut self) {
        self.op(0x7e)
    }
    pub fn i64_div_s(&mut self) {
        self.op(0x7f)
    }
    pub fn i64_div_u(&mut self) {
        self.op(0x80)
    }
    pub fn i64_and(&mut self) {
        self.op(0x83)
    }
    pub fn i64_or(&mut self) {
        self.op(0x84)
    }
    pub fn i64_xor(&mut self) {
        self.op(0x85)
    }
    pub fn i64_shl(&mut self) {
        self.op(0x86)
    }
    pub fn i64_shr_s(&mut self) {
        self.op(0x87)
    }
    pub fn i64_shr_u(&mut self) {
        self.op(0x88)
    }
    pub fn i64_rotl(&mut self) {
        self.op(0x89)
    }
    pub fn i64_rotr(&mut self) {
        self.op(0x8a)
    }

    // conversions
    pub fn i32_wrap_i64(&mut self) {
        self.op(0xa7)
    }
    pub fn i64_extend_i32_s(&mut self) {
        self.op(0xac)
    }
    pub fn i64_extend_i32_u(&mut self) {
        self.op(0xad)
    }
    pub fn i64_extend8_s(&mut self) {
        self.op(0xc2)
    }
    pub fn i64_extend16_s(&mut self) {
        self.op(0xc3)
    }
    pub fn i64_extend32_s(&mut self) {
        self.op(0xc4)
    }
}

/// Assemble a module: `imports` are functions 0..n, the body is function n,
/// exported as "f" with signature (i64) -> i32. Locals beyond the parameter
/// are given as (count, type) runs. `shared`: the imported memory is the
/// threaded build's shared memory (max 4 GiB).
pub fn module(imports: &[Import], locals: &[(u32, u8)], code: &Code, shared: bool) -> Vec<u8> {
    let mut types: Vec<FuncType> = Vec::new();
    let mut type_of = |t: &FuncType| -> u32 {
        if let Some(i) = types.iter().position(|x| x == t) {
            i as u32
        } else {
            types.push(t.clone());
            (types.len() - 1) as u32
        }
    };
    let import_types: Vec<u32> = imports.iter().map(|i| type_of(&i.ty)).collect();
    let main_type = type_of(&FuncType {
        params: vec![I64],
        results: vec![I32],
    });

    let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    let mut s = Vec::new();
    leb_u(&mut s, types.len() as u64);
    for t in &types {
        s.push(0x60);
        leb_u(&mut s, t.params.len() as u64);
        s.extend_from_slice(&t.params);
        leb_u(&mut s, t.results.len() as u64);
        s.extend_from_slice(&t.results);
    }
    section(&mut out, 1, &s);

    let mut s = Vec::new();
    leb_u(&mut s, imports.len() as u64 + 1);
    name(&mut s, "e");
    name(&mut s, "m");
    s.push(0x02); // memory
    if shared {
        s.push(0x03); // limits: min and max, shared
        leb_u(&mut s, 1);
        leb_u(&mut s, 65536);
    } else {
        s.push(0x00); // limits: min only
        leb_u(&mut s, 1);
    }
    for (imp, &t) in imports.iter().zip(&import_types) {
        name(&mut s, "e");
        name(&mut s, imp.name);
        s.push(0x00); // function
        leb_u(&mut s, t as u64);
    }
    section(&mut out, 2, &s);

    let mut s = Vec::new();
    leb_u(&mut s, 1);
    leb_u(&mut s, main_type as u64);
    section(&mut out, 3, &s);

    let mut s = Vec::new();
    leb_u(&mut s, 1);
    name(&mut s, "f");
    s.push(0x00);
    leb_u(&mut s, imports.len() as u64);
    section(&mut out, 7, &s);

    let mut body = Vec::new();
    leb_u(&mut body, locals.len() as u64);
    for &(n, t) in locals {
        leb_u(&mut body, n as u64);
        body.push(t);
    }
    body.extend_from_slice(&code.b);
    body.push(0x0b);
    let mut s = Vec::new();
    leb_u(&mut s, 1);
    leb_u(&mut s, body.len() as u64);
    s.extend_from_slice(&body);
    section(&mut out, 10, &s);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb() {
        let mut v = Vec::new();
        leb_s(&mut v, -1);
        assert_eq!(v, [0x7f]);
        v.clear();
        leb_s(&mut v, 64);
        assert_eq!(v, [0xc0, 0x00]);
        v.clear();
        leb_u(&mut v, 624485);
        assert_eq!(v, [0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn minimal_module_is_well_formed() {
        let mut c = Code::new();
        c.i32c(7);
        let m = module(&[], &[], &c, false);
        assert_eq!(&m[0..4], b"\0asm");
    }
}
