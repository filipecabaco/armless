//! The loop of the interpreter over the decoded ops of a page (see `predecode`): `Cpu::run_ops` keeps the pc,
//! the budget and the op it is at in locals, and runs each kind of op with the registers it names, without
//! decoding anything. What a kind does not cover (and what `Gen` and `GenPure` hold) is run by `exec_slow`,
//! the same leaf of the decoder `execute` reaches. Each kind does what the leaf of the decoder does for the
//! instructions it stands for (`a64`, `simd_ls`), and a load or store that the fast tables do not serve, or
//! an SIMD&FP one with the unit off, is handed to `exec_slow` whole, which also raises what it raises.

use super::*;
use crate::a64::{add_with_carry, extend_reg, shift_reg};
use crate::predecode::{decode, entry, fuse_cmp_bcond, Op, BLOCK, K, OUT, PAGE_INSNS};

// Where the flags N, Z, C, V are, in `run_ops`: in `self` (`L_EAGER`); or those of the subtraction `la - lb`
// that the last SUBS did, or of the logical result `la` that the last ANDS did (`L_SUB`, `L_LOG`), which are
// worked out only for what asks for them. The operands of a 32-bit one are shifted to the top of 64 bits,
// which keeps its flags: N is the top bit, Z a zero result, C an unsigned compare, V a signed overflow.
const L_EAGER: u8 = 0;
const L_SUB: u8 = 1;
const L_LOG: u8 = 2;

/// Whether the condition `cond` holds for the lazy flags `lk`, `la`, `lb` (`lk` is not `L_EAGER`).
#[inline(always)]
fn lazy_cond(lk: u8, la: u64, lb: u64, cond: u32) -> bool {
    let r = if lk == L_SUB {
        match cond >> 1 {
            0 => la == lb,
            1 => la >= lb,
            2 => (la.wrapping_sub(lb) as i64) < 0,
            3 => ((la ^ lb) & (la ^ la.wrapping_sub(lb))) >> 63 != 0,
            4 => la > lb,
            5 => (la as i64) >= (lb as i64),
            6 => (la as i64) > (lb as i64),
            _ => true,
        }
    } else {
        // C and V are clear.
        match cond >> 1 {
            0 => la == 0,
            2 => (la as i64) < 0,
            5 => (la as i64) >= 0,
            6 => (la as i64) > 0,
            7 => true,
            _ => false,
        }
    };
    if cond & 1 != 0 && cond != 0xf {
        !r
    } else {
        r
    }
}

impl<H: Host> Cpu<H> {
    /// Decode the instructions of the current code page from index `idx` (not decoded yet) on, into `ops`, the
    /// ops of the page: up to the end of the block (`BLOCK` instructions, a branch that always leaves, an
    /// instruction that `Gen` runs, the end of the page, or the first one that is decoded).
    ///
    /// # Safety
    /// `ops` points at the ops of the current page of the cache, `self.fast[self.el].code_base` is its page.
    #[inline(never)]
    pub(super) unsafe fn fill_ops(&mut self, ops: *mut Op, idx: usize) {
        let base = self.fast[self.el as usize].code_base;
        let mut i = idx;
        let mut n = 0;
        loop {
            let insn = self.ram_load(base + i * 4, 4) as u32;
            let op = decode(insn, i);
            *ops.add(i) = op;
            // A compare and a conditional branch after it run as one op, whichever of them is decoded last.
            if op.k == K::Bcond && i > 0 {
                fuse_cmp_bcond(ops.add(i - 1), ops.add(i));
            } else if i + 1 < PAGE_INSNS {
                fuse_cmp_bcond(ops.add(i), ops.add(i + 1));
            }
            n += 1;
            i += 1;
            if op.k.ends_block() || i == PAGE_INSNS || n == BLOCK || (*ops.add(i)).k != K::Miss {
                break;
            }
        }
        self.jit.pre.blocks += 1;
        self.jit.pre.insns += n as u64;
    }

    /// The `1 << scale` bytes (scale 0..=4) at `va`, if the fast tables serve them.
    #[inline(always)]
    fn vread(&self, va: u64, scale: u32) -> Option<u128> {
        if scale == 4 {
            let (lo, hi) = self.read_pair_hit(va, 8)?;
            Some(lo as u128 | (hi as u128) << 64)
        } else {
            self.read_hit(va, 1 << scale).map(|v| v as u128)
        }
    }

    /// Store the `1 << scale` bytes of `v` at `va`, if the fast tables serve them: true once it is done.
    #[inline(always)]
    fn vwrite(&mut self, va: u64, scale: u32, v: u128) -> bool {
        if scale == 4 {
            self.write_pair_hit(va, 8, v as u64, (v >> 64) as u64)
        } else {
            self.write_hit(va, 1 << scale, v as u64)
        }
    }

    /// Store `v1` at `va` and `v2` right after it (`1 << scale` bytes each), if the fast tables serve both with one
    /// page: true once both are done. Else false, and nothing is done, so that `exec_slow` can run the pair alone.
    #[inline(always)]
    fn vwrite_pair(&mut self, va: u64, scale: u32, v1: u128, v2: u128) -> bool {
        if scale != 4 {
            return self.write_pair_hit(va, 1 << scale, v1 as u64, v2 as u64);
        }
        let off = va & 0xfff;
        if off + 32 <= 0x1000 && self.excl_armed == 0 {
            let f = &self.fast[self.el as usize];
            let i = (va >> 12) as usize & (FAST_ENTRIES - 1);
            if f.write_tag[i] == va >> 12 {
                let a = f.write_base[i] as usize + off as usize;
                let words = [v1 as u64, (v1 >> 64) as u64, v2 as u64, (v2 >> 64) as u64];
                for (k, w) in words.into_iter().enumerate() {
                    self.ram_store(a + 8 * k, 8, w);
                }
                return true;
            }
        }
        false
    }

    /// Run the instruction word of entry `e` at `pc`, which is `exec_slow`, as the loop leaves it: what the
    /// instruction leaves for the pc after it.
    #[inline(never)]
    fn gen_at(&mut self, pc: u64, e: u64) -> Res<u64> {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        self.exec_slow(e).map(|()| self.next_pc)
    }

    /// The flags that `run_ops` has not stored (see `L_SUB`), in `self`.
    #[inline(never)]
    fn flush_flags(&mut self, lk: u8, la: u64, lb: u64) {
        if lk == L_SUB {
            let r = la.wrapping_sub(lb);
            self.n = r >> 63 != 0;
            self.z = la == lb;
            self.c = la >= lb;
            self.v = ((la ^ lb) & (la ^ r)) >> 63 != 0;
        } else {
            self.n = la >> 63 != 0;
            self.z = la == 0;
            self.c = false;
            self.v = false;
        }
    }

    /// Run the ops of the page `ops` from index `start` for as long as they go on in it: at most `max` (at
    /// least 1) in all. Returns what the last of them leaves for the pc, how many were run (a faulting one
    /// counts), and the pc of the last one. `self.pc`, `next_pc` and `insn_count` are for `step_page` to set.
    ///
    /// The loop ends at a budget that is used up, at the end of the page, at a taken branch that `jit_step`
    /// counts (see `jump`) or whose target is in another page that the fetch path does not hold or the cache
    /// has no ops for (a taken branch to a page that it holds goes on there, see `cross`), at an op that `Gen`
    /// or a load or store that the fast tables do not serve runs, and at an exception. Nothing that is not
    /// inlined is called in the loop for the kinds it has, so the loop keeps what it carries in registers;
    /// `Cache::drop_page` and `evict` run in calls that end the run (a store to a page, an instruction-cache
    /// invalidation, a page decoded), so the ops that it runs are never dropped under it. Across pages, `ops0`
    /// and `page_pc` are those of the page the loop is in, the current page of the fetch path and the cache.
    ///
    /// # Safety
    /// `ops` points at the `SLOT_OPS` ops of a live slot, whose page is the one `page_pc` is in; `start` is an
    /// index in it.
    pub(super) unsafe fn run_ops(
        &mut self,
        mut ops0: *mut Op,
        mut page_pc: u64,
        start: usize,
        max: u64,
    ) -> (Res<u64>, u64, u64) {
        let mut op = ops0.add(start);
        // Where the run, or its stretch since the last taken branch, began, how much of the budget was left
        // there, and so the op the budget ends at.
        let mut blk = op;
        let mut rem = max;
        let mut end = blk.wrapping_add(max as usize);
        // What `jit_step` does with an entry of this page, worked out at the first taken branch (`warm_at` 0: not
        // yet): the count at which it warms the entry (`warm_at`) and the one at which it compiles it (`hot_at`),
        // and whether the page has modules (`mods`), which may hold the entry (`mkey` is the key of the page).
        let mut warm_at = 0u32;
        let mut hot_at = 0u32;
        let mut mods = false;
        let mut mkey = 0u64;
        // The flags that are not in `self` yet (`L_SUB`, `L_LOG`): the loop stores them to `self` before
        // anything else can read them (`exec_slow`, which an exception also reaches) and when it ends.
        let mut lk = L_EAGER;
        let mut la = 0u64;
        let mut lb = 0u64;

        // A field of the op.
        macro_rules! o {
            ($f:ident) => {
                (*op).$f
            };
        }
        // The pc of the op.
        macro_rules! pc {
            () => {
                page_pc.wrapping_add(((op as usize - ops0 as usize) >> 2) as u64)
            };
        }
        // How many were run when the run ends with the op, `$extra` more if it is run too.
        macro_rules! done {
            ($extra:expr) => {
                (max - rem) + op.offset_from(blk) as u64 + $extra
            };
        }
        macro_rules! reg {
            ($i:expr) => {
                *self.x.get_unchecked($i as usize)
            };
        }
        macro_rules! put {
            ($i:expr, $v:expr) => {{
                let v: u64 = $v;
                *self.x.get_unchecked_mut($i as usize) = v;
            }};
        }
        // The flags (N, Z, C, V) of an addition, which are worked out and stored at once.
        macro_rules! flags {
            ($t:expr) => {{
                let (r, n, z, c, v) = $t;
                lk = L_EAGER;
                self.n = n;
                self.z = z;
                self.c = c;
                self.v = v;
                r
            }};
        }
        // The flags of a subtraction at the size 64 or 32 (`$sf`): lazy. Returns the difference.
        macro_rules! lsub {
            ($x:expr, $y:expr, $sf:expr) => {{
                let (x, y): (u64, u64) = ($x, $y);
                lk = L_SUB;
                if $sf {
                    la = x;
                    lb = y;
                    x.wrapping_sub(y)
                } else {
                    la = x << 32;
                    lb = y << 32;
                    (x as u32).wrapping_sub(y as u32) as u64
                }
            }};
        }
        // The flags of a logical result (of the size 64 or 32, `$sf`; the 32-bit one zero-extended): lazy.
        macro_rules! llog {
            ($r:expr, $sf:expr) => {{
                let r: u64 = $r;
                lk = L_LOG;
                la = if $sf { r } else { r << 32 };
                r
            }};
        }
        // Whether the condition `$c` holds.
        macro_rules! cond {
            ($c:expr) => {{
                let c = $c as u32;
                if lk == L_EAGER {
                    self.cond_holds(c)
                } else {
                    lazy_cond(lk, la, lb, c)
                }
            }};
        }
        // The carry flag.
        macro_rules! carry {
            () => {
                match lk {
                    L_EAGER => self.c,
                    L_SUB => la >= lb,
                    _ => false,
                }
            };
        }
        // The flags of the loop stored to `self`.
        macro_rules! flush {
            () => {
                if lk != L_EAGER {
                    self.flush_flags(lk, la, lb);
                    lk = L_EAGER;
                }
            };
        }
        // The op that continues at `$t`, a target in the page, after a taken branch whose pc afterwards is
        // `$next`: the run goes on unless the budget is used up or `jit_step` has more to do with the branch than
        // to count it (JIT on and the target not decoded, or the count that warms it or the one that compiles it
        // reached, or a module ready for it). The counting is done here, in the op.
        macro_rules! jump {
            ($t:expr, $next:expr) => {{
                let t: *mut Op = $t;
                let left = rem - (op.offset_from(blk) as u64 + 1);
                if left == 0 {
                    break (Ok($next), max, pc!());
                }
                if self.jit.enabled {
                    if warm_at == 0 {
                        let f = &self.fast[self.el as usize];
                        (warm_at, hot_at, mods) = self.page_counts(f.code_base);
                        mkey = crate::jit::Jit::key(f.code_base as u64, self.el, self.spsel);
                    }
                    let c = (*t).cnt as u32 + 1;
                    if (*t).k == K::Miss
                        || c == warm_at
                        || c >= hot_at
                        || (mods
                            && self
                                .jit
                                .has_module(mkey + ((t.offset_from(ops0) as u64) << 4)))
                    {
                        break (Ok($next), done!(1), pc!());
                    }
                    (*t).cnt = c as u16;
                }
                rem = left;
                blk = t;
                end = t.wrapping_add(left as usize);
                op = t;
                continue;
            }};
        }
        // The op that continues at `$next`, an aligned target in another page, after a taken branch: `jump` in that
        // page, when the fetch path holds the page and the cache has its ops (`enter_page`), and it is the page
        // that the loop runs from then on (`ops0` and `page_pc`, the current page of the fetch path and the
        // cache). Else the run ends, and `step_page` does what it does for a branch out of the page: fetch and
        // translate it (and raise the fault of that), decode it. A `jit_step` that has more to do than count the
        // entry, as in `jump`, ends it too, before the loop leaves the page it is in. `rem` and `blk` do not
        // depend on the page, so the budget is the same one all through.
        macro_rules! cross {
            ($next:expr) => {{
                let next: u64 = $next;
                let left = rem - (op.offset_from(blk) as u64 + 1);
                if left == 0 {
                    break (Ok(next), max, pc!());
                }
                let nops = self.enter_page(next);
                if nops.is_null() {
                    break (Ok(next), done!(1), pc!());
                }
                let idx = ((next & 0xfff) >> 2) as usize;
                let t = nops.add(idx);
                if self.jit.enabled {
                    let base = self.fast[self.el as usize].code_base;
                    let (w, h, m) = self.page_counts(base);
                    let key = crate::jit::Jit::key(base as u64, self.el, self.spsel);
                    let c = (*t).cnt as u32 + 1;
                    if (*t).k == K::Miss
                        || c == w
                        || c >= h
                        || (m && self.jit.has_module(key + ((idx as u64) << 4)))
                    {
                        break (Ok(next), done!(1), pc!());
                    }
                    (*t).cnt = c as u16;
                    warm_at = w;
                    hot_at = h;
                    mods = m;
                    mkey = key;
                }
                ops0 = nops;
                page_pc = next & !0xfff;
                rem = left;
                blk = t;
                end = t.wrapping_add(left as usize);
                op = t;
                continue;
            }};
        }
        // A taken direct branch.
        macro_rules! take {
            () => {{
                let next = page_pc.wrapping_add(o!(imm));
                if o!(x) == OUT {
                    cross!(next)
                }
                jump!(ops0.add(o!(x) as usize), next)
            }};
        }
        // A taken indirect branch to `$t`.
        macro_rules! take_reg {
            ($t:expr) => {{
                let t: u64 = $t;
                if t & 3 != 0 {
                    break (Ok(t), done!(1), pc!());
                }
                if (t ^ page_pc) >> 12 != 0 {
                    cross!(t)
                }
                jump!(ops0.add(((t & 0xfff) >> 2) as usize), t)
            }};
        }
        // Not a branch: the next op.
        macro_rules! cbr {
            ($taken:expr) => {{
                if $taken {
                    take!()
                }
            }};
        }
        // An instruction that leaves the loop, run by `exec_slow`: the entry `$e`.
        macro_rules! leave {
            ($e:expr) => {{
                let pc = pc!();
                flush!();
                let r = self.gen_at(pc, $e);
                break (r, done!(1), pc);
            }};
        }
        // The load or store of the op, which the fast tables did not serve: `exec_slow` runs it.
        macro_rules! slow_mem {
            () => {{
                leave!(entry((o!(imm) >> 32) as u32))
            }};
        }
        // The register that a memory op addresses from: SP, or an X register.
        macro_rules! base {
            () => {
                if o!(b) == 31 {
                    self.sp()
                } else {
                    reg!(o!(b))
                }
            };
        }
        macro_rules! offset {
            () => {
                o!(imm) as u32 as i32 as i64 as u64
            };
        }
        // The address of an access in mode `$m` from the base `$base`, and the base register's new value.
        macro_rules! addr {
            (Off, $base:expr) => {
                $base.wrapping_add(offset!())
            };
            (Pre, $base:expr) => {
                $base.wrapping_add(offset!())
            };
            (Post, $base:expr) => {
                $base
            };
            (Reg, $base:expr) => {
                $base.wrapping_add(extend_reg(reg!(o!(c)), o!(x) as u32, o!(imm) as u32))
            };
            // The extension of an SIMD&FP register offset is above the size in `x`.
            (VReg, $base:expr) => {
                $base.wrapping_add(extend_reg(reg!(o!(c)), (o!(x) >> 4) as u32, o!(imm) as u32))
            };
        }
        macro_rules! writeback {
            (Off, $base:expr, $addr:expr) => {};
            (Reg, $base:expr, $addr:expr) => {};
            (VReg, $base:expr, $addr:expr) => {};
            (Pre, $base:expr, $addr:expr) => {
                self.ws(o!(b) as u32, $addr)
            };
            (Post, $base:expr, $addr:expr) => {
                self.ws(o!(b) as u32, $base.wrapping_add(offset!()))
            };
        }
        macro_rules! st {
            ($sz:expr, $m:ident) => {{
                let base = base!();
                let addr = addr!($m, base);
                if !self.write_hit(addr, $sz, reg!(o!(a))) {
                    slow_mem!()
                }
                writeback!($m, base, addr);
            }};
        }
        macro_rules! ld {
            ($sz:expr, $m:ident, $conv:expr) => {{
                let base = base!();
                let addr = addr!($m, base);
                let Some(v) = self.read_hit(addr, $sz) else {
                    slow_mem!()
                };
                put!(o!(a), ($conv)(v));
                writeback!($m, base, addr);
            }};
        }
        macro_rules! stp {
            ($sz:expr, $m:ident) => {{
                let base = base!();
                let addr = addr!($m, base);
                if !self.write_pair_hit(addr, $sz, reg!(o!(a)), reg!(o!(c))) {
                    slow_mem!()
                }
                writeback!($m, base, addr);
            }};
        }
        macro_rules! ldp {
            ($sz:expr, $m:ident) => {{
                let base = base!();
                let addr = addr!($m, base);
                let Some((v1, v2)) = self.read_pair_hit(addr, $sz) else {
                    slow_mem!()
                };
                put!(o!(a), v1);
                put!(o!(c), v2);
                writeback!($m, base, addr);
            }};
        }
        // A load of the literal at the address in the low half of the immediate.
        macro_rules! lit {
            ($sz:expr, $conv:expr) => {{
                let addr = page_pc.wrapping_add(offset!());
                let Some(v) = self.read_hit(addr, $sz) else {
                    slow_mem!()
                };
                put!(o!(a), ($conv)(v));
            }};
        }

        // SIMD&FP loads and stores of one register: the size is in the low bits of `x`.
        macro_rules! vst {
            ($m:ident) => {{
                if !self.fp_enabled() {
                    slow_mem!()
                }
                let base = base!();
                let addr = addr!($m, base);
                let v = *self.vreg.get_unchecked(o!(a) as usize);
                if !self.vwrite(addr, (o!(x) & 15) as u32, v) {
                    slow_mem!()
                }
                writeback!($m, base, addr);
            }};
        }
        macro_rules! vld {
            ($m:ident) => {{
                if !self.fp_enabled() {
                    slow_mem!()
                }
                let base = base!();
                let addr = addr!($m, base);
                let Some(v) = self.vread(addr, (o!(x) & 15) as u32) else {
                    slow_mem!()
                };
                *self.vreg.get_unchecked_mut(o!(a) as usize) = v;
                writeback!($m, base, addr);
            }};
        }
        // The flags of CCMP/CCMN: those of the comparison if the condition holds, else the immediate nzcv in `a`.
        macro_rules! ccmp {
            ($y:expr, $sub:expr, $sf:expr) => {{
                let holds = cond!(o!(x));
                lk = L_EAGER;
                if holds {
                    let y: u64 = $y;
                    let n = reg!(o!(b));
                    let (_, n, z, c, v) = if $sub {
                        add_with_carry(n, !y, true, $sf)
                    } else {
                        add_with_carry(n, y, false, $sf)
                    };
                    self.n = n;
                    self.z = z;
                    self.c = c;
                    self.v = v;
                } else {
                    let f = o!(a);
                    self.n = f & 8 != 0;
                    self.z = f & 4 != 0;
                    self.c = f & 2 != 0;
                    self.v = f & 1 != 0;
                }
            }};
        }
        // A compare (SUBS) fused with the B.cond after it (see `fuse_cmp_bcond`): the compare, with lazy flags,
        // and then the branch, which is the op after it, unless the budget ends between the two. `$pred` is the
        // condition class on `la` and `lb`, which bit 8 of `x` inverts.
        macro_rules! fsub {
            ($second:expr, $pred:expr) => {{
                let sh = (o!(x) & 63) as u32;
                let inv = o!(x) >> 8 != 0;
                let x = reg!(o!(b)) << sh;
                let y: u64 = ($second) << sh;
                put!(o!(a), x.wrapping_sub(y) >> sh);
                lk = L_SUB;
                la = x;
                lb = y;
                if op.add(1) != end {
                    op = op.add(1);
                    if ($pred) != inv {
                        take!()
                    }
                }
            }};
        }
        // The second operand of a logical instruction with a shifted (and maybe inverted) register.
        macro_rules! shifted {
            ($sf:expr) => {{
                let m = shift_reg(
                    reg!(o!(c)),
                    (o!(x) & 3) as u32,
                    ((o!(x) >> 2) & 63) as u32,
                    $sf,
                );
                if o!(x) >> 8 != 0 {
                    !m
                } else {
                    m
                }
            }};
        }
        // An extended register operand, 64-bit, and the first (SP or an X register).
        macro_rules! extended {
            () => {
                extend_reg(reg!(o!(c)), (o!(x) & 7) as u32, (o!(x) >> 3) as u32)
            };
        }
        macro_rules! sp_or_reg {
            ($i:expr) => {
                if $i == 31 {
                    self.sp()
                } else {
                    reg!($i)
                }
            };
        }
        // A write to a register that is SP for 31.
        macro_rules! put_sp {
            ($i:expr, $v:expr) => {{
                let v: u64 = $v;
                if $i == 31 {
                    self.set_sp(v)
                } else {
                    put!($i, v)
                }
            }};
        }

        let r = loop {
            match o!(k) {
                K::Miss => {
                    // Decode from here (which the loop does not do for itself), and run the op again.
                    self.fill_ops(ops0, op.offset_from(ops0) as usize);
                    continue;
                },
                K::End => {
                    let next = pc!();
                    break (Ok(next), done!(0), next.wrapping_sub(4));
                },
                K::Gen => leave!(o!(imm)),
                K::GenPure => {
                    let pc = pc!();
                    flush!();
                    self.pc = pc;
                    self.next_pc = pc.wrapping_add(4);
                    if let Err(ev) = self.exec_slow(o!(imm)) {
                        break (Err(ev), done!(1), pc);
                    }
                },
                K::Nop => {},
                K::B => take!(),
                K::Bl => {
                    put!(30, pc!().wrapping_add(4));
                    take!()
                },
                K::Bcond => cbr!(cond!(o!(a))),
                K::Cbz64 => cbr!(reg!(o!(b)) == 0),
                K::Cbnz64 => cbr!(reg!(o!(b)) != 0),
                K::Cbz32 => cbr!(reg!(o!(b)) as u32 == 0),
                K::Cbnz32 => cbr!(reg!(o!(b)) as u32 != 0),
                K::Tbz => cbr!((reg!(o!(b)) >> o!(a)) & 1 == 0),
                K::Tbnz => cbr!((reg!(o!(b)) >> o!(a)) & 1 != 0),
                K::Br => take_reg!(reg!(o!(b))),
                K::Blr => {
                    let t = reg!(o!(b));
                    put!(30, pc!().wrapping_add(4));
                    take_reg!(t)
                },
                K::Adr => put!(o!(a), page_pc.wrapping_add(o!(imm))),
                K::MovI => put!(o!(a), o!(imm)),
                K::Movk64 => {
                    let sh = o!(b) as u32;
                    put!(
                        o!(a),
                        (reg!(o!(a)) & !(0xffffu64 << sh)) | ((o!(x) as u64) << sh)
                    )
                },
                K::Movk32 => {
                    let sh = o!(b) as u32;
                    put!(
                        o!(a),
                        ((reg!(o!(a)) & !(0xffffu64 << sh)) | ((o!(x) as u64) << sh)) as u32 as u64
                    )
                },
                K::MovR64 => put!(o!(a), reg!(o!(c))),
                K::MovR32 => put!(o!(a), reg!(o!(c)) as u32 as u64),
                K::AddI64 => put!(o!(a), reg!(o!(b)).wrapping_add(o!(imm))),
                K::AddI32 => put!(
                    o!(a),
                    (reg!(o!(b)) as u32).wrapping_add(o!(imm) as u32) as u64
                ),
                K::AddsI64 => put!(
                    o!(a),
                    flags!(add_with_carry(reg!(o!(b)), o!(imm), false, true))
                ),
                K::AddsI32 => put!(
                    o!(a),
                    flags!(add_with_carry(reg!(o!(b)), o!(imm), false, false))
                ),
                K::SubsI64 => put!(o!(a), lsub!(reg!(o!(b)), o!(imm), true)),
                K::SubsI32 => put!(o!(a), lsub!(reg!(o!(b)), o!(imm), false)),
                K::AndI64 => put!(o!(a), reg!(o!(b)) & o!(imm)),
                K::OrrI64 => put!(o!(a), reg!(o!(b)) | o!(imm)),
                K::EorI64 => put!(o!(a), reg!(o!(b)) ^ o!(imm)),
                K::AndsI64 => put!(o!(a), llog!(reg!(o!(b)) & o!(imm), true)),
                K::AndI32 => put!(o!(a), (reg!(o!(b)) & o!(imm)) as u32 as u64),
                K::OrrI32 => put!(o!(a), (reg!(o!(b)) | o!(imm)) as u32 as u64),
                K::EorI32 => put!(o!(a), (reg!(o!(b)) ^ o!(imm)) as u32 as u64),
                K::AndsI32 => put!(o!(a), llog!((reg!(o!(b)) & o!(imm)) as u32 as u64, false)),
                K::Ubfx => put!(o!(a), (reg!(o!(b)) >> o!(c)) & o!(imm)),
                K::Ubfiz => put!(o!(a), (reg!(o!(b)) & o!(imm)) << o!(c)),
                K::Sbfx64 => {
                    let r = self.bitfield(0, reg!(o!(b)), 0, o!(c) as u32, o!(x) as u32, 64);
                    put!(o!(a), r)
                },
                K::Sbfx32 => {
                    let r = self.bitfield(
                        0,
                        reg!(o!(b)) as u32 as u64,
                        0,
                        o!(c) as u32,
                        o!(x) as u32,
                        32,
                    );
                    put!(o!(a), r as u32 as u64)
                },
                K::Bfm => {
                    let sf = o!(x) >> 8 != 0;
                    let (src, size) =
                        if sf { (reg!(o!(b)), 64) } else { (reg!(o!(b)) as u32 as u64, 32) };
                    let r = self.bitfield(
                        1,
                        src,
                        reg!(o!(a)),
                        o!(c) as u32,
                        (o!(x) & 0xff) as u32,
                        size,
                    );
                    put!(o!(a), if sf { r } else { r as u32 as u64 })
                },
                K::AddR64 => put!(o!(a), reg!(o!(b)).wrapping_add(reg!(o!(c)))),
                K::AddR32 => put!(
                    o!(a),
                    (reg!(o!(b)) as u32).wrapping_add(reg!(o!(c)) as u32) as u64
                ),
                K::SubR64 => put!(o!(a), reg!(o!(b)).wrapping_sub(reg!(o!(c)))),
                K::SubR32 => put!(
                    o!(a),
                    (reg!(o!(b)) as u32).wrapping_sub(reg!(o!(c)) as u32) as u64
                ),
                K::AddsR64 => put!(
                    o!(a),
                    flags!(add_with_carry(reg!(o!(b)), reg!(o!(c)), false, true))
                ),
                K::AddsR32 => put!(
                    o!(a),
                    flags!(add_with_carry(reg!(o!(b)), reg!(o!(c)), false, false))
                ),
                K::SubsR64 => put!(o!(a), lsub!(reg!(o!(b)), reg!(o!(c)), true)),
                K::SubsR32 => put!(o!(a), lsub!(reg!(o!(b)), reg!(o!(c)), false)),
                K::AddRs64 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, true);
                    put!(o!(a), reg!(o!(b)).wrapping_add(m))
                },
                K::AddRs32 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, false);
                    put!(o!(a), (reg!(o!(b)) as u32).wrapping_add(m as u32) as u64)
                },
                K::SubRs64 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, true);
                    put!(o!(a), reg!(o!(b)).wrapping_sub(m))
                },
                K::SubRs32 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, false);
                    put!(o!(a), (reg!(o!(b)) as u32).wrapping_sub(m as u32) as u64)
                },
                K::AddsRs64 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, true);
                    put!(o!(a), flags!(add_with_carry(reg!(o!(b)), m, false, true)))
                },
                K::AddsRs32 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, false);
                    put!(o!(a), flags!(add_with_carry(reg!(o!(b)), m, false, false)))
                },
                K::SubsRs64 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, true);
                    put!(o!(a), lsub!(reg!(o!(b)), m, true))
                },
                K::SubsRs32 => {
                    let m = shift_reg(reg!(o!(c)), (o!(x) & 3) as u32, (o!(x) >> 2) as u32, false);
                    put!(o!(a), lsub!(reg!(o!(b)), m, false))
                },
                K::AndR64 => put!(o!(a), reg!(o!(b)) & reg!(o!(c))),
                K::OrrR64 => put!(o!(a), reg!(o!(b)) | reg!(o!(c))),
                K::EorR64 => put!(o!(a), reg!(o!(b)) ^ reg!(o!(c))),
                K::AndsR64 => put!(o!(a), llog!(reg!(o!(b)) & reg!(o!(c)), true)),
                K::AndR32 => put!(o!(a), (reg!(o!(b)) & reg!(o!(c))) as u32 as u64),
                K::OrrR32 => put!(o!(a), (reg!(o!(b)) | reg!(o!(c))) as u32 as u64),
                K::EorR32 => put!(o!(a), (reg!(o!(b)) ^ reg!(o!(c))) as u32 as u64),
                K::AndsR32 => put!(
                    o!(a),
                    llog!((reg!(o!(b)) & reg!(o!(c))) as u32 as u64, false)
                ),
                K::Csel64 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { reg!(o!(c)) };
                    put!(o!(a), v)
                },
                K::Csinc64 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { reg!(o!(c)).wrapping_add(1) };
                    put!(o!(a), v)
                },
                K::Csinv64 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { !reg!(o!(c)) };
                    put!(o!(a), v)
                },
                K::Csneg64 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { reg!(o!(c)).wrapping_neg() };
                    put!(o!(a), v)
                },
                K::Csel32 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { reg!(o!(c)) };
                    put!(o!(a), v as u32 as u64)
                },
                K::Csinc32 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { reg!(o!(c)).wrapping_add(1) };
                    put!(o!(a), v as u32 as u64)
                },
                K::Csinv32 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { !reg!(o!(c)) };
                    put!(o!(a), v as u32 as u64)
                },
                K::Csneg32 => {
                    let v = if cond!(o!(x)) { reg!(o!(b)) } else { reg!(o!(c)).wrapping_neg() };
                    put!(o!(a), v as u32 as u64)
                },
                K::Shiftv64 => put!(
                    o!(a),
                    shift_reg(reg!(o!(b)), o!(x) as u32, reg!(o!(c)) as u32, true)
                ),
                K::Shiftv32 => put!(
                    o!(a),
                    shift_reg(reg!(o!(b)), o!(x) as u32, reg!(o!(c)) as u32, false)
                ),
                K::Madd64 => put!(
                    o!(a),
                    reg!(o!(x)).wrapping_add(reg!(o!(b)).wrapping_mul(reg!(o!(c))))
                ),
                K::Msub64 => put!(
                    o!(a),
                    reg!(o!(x)).wrapping_sub(reg!(o!(b)).wrapping_mul(reg!(o!(c))))
                ),
                K::Madd32 => put!(
                    o!(a),
                    reg!(o!(x)).wrapping_add(reg!(o!(b)).wrapping_mul(reg!(o!(c)))) as u32 as u64
                ),
                K::Msub32 => put!(
                    o!(a),
                    reg!(o!(x)).wrapping_sub(reg!(o!(b)).wrapping_mul(reg!(o!(c)))) as u32 as u64
                ),
                K::LdrLit32 => lit!(4, |v: u64| v),
                K::LdrLit64 => lit!(8, |v: u64| v),
                K::LdrLitSw => lit!(4, |v: u64| v as u32 as i32 as i64 as u64),
                K::St1Off => st!(1, Off),
                K::St2Off => st!(2, Off),
                K::St4Off => st!(4, Off),
                K::St8Off => st!(8, Off),
                K::Ld1Off => ld!(1, Off, |v: u64| v),
                K::Ld2Off => ld!(2, Off, |v: u64| v),
                K::Ld4Off => ld!(4, Off, |v: u64| v),
                K::Ld8Off => ld!(8, Off, |v: u64| v),
                K::St1Pre => st!(1, Pre),
                K::St2Pre => st!(2, Pre),
                K::St4Pre => st!(4, Pre),
                K::St8Pre => st!(8, Pre),
                K::Ld1Pre => ld!(1, Pre, |v: u64| v),
                K::Ld2Pre => ld!(2, Pre, |v: u64| v),
                K::Ld4Pre => ld!(4, Pre, |v: u64| v),
                K::Ld8Pre => ld!(8, Pre, |v: u64| v),
                K::St1Post => st!(1, Post),
                K::St2Post => st!(2, Post),
                K::St4Post => st!(4, Post),
                K::St8Post => st!(8, Post),
                K::Ld1Post => ld!(1, Post, |v: u64| v),
                K::Ld2Post => ld!(2, Post, |v: u64| v),
                K::Ld4Post => ld!(4, Post, |v: u64| v),
                K::Ld8Post => ld!(8, Post, |v: u64| v),
                K::St1Reg => st!(1, Reg),
                K::St2Reg => st!(2, Reg),
                K::St4Reg => st!(4, Reg),
                K::St8Reg => st!(8, Reg),
                K::Ld1Reg => ld!(1, Reg, |v: u64| v),
                K::Ld2Reg => ld!(2, Reg, |v: u64| v),
                K::Ld4Reg => ld!(4, Reg, |v: u64| v),
                K::Ld8Reg => ld!(8, Reg, |v: u64| v),
                K::Ls1Off => ld!(1, Off, |v: u64| v as u8 as i8 as i64 as u64),
                K::Ls2Off => ld!(2, Off, |v: u64| v as u16 as i16 as i64 as u64),
                K::Ls4Off => ld!(4, Off, |v: u64| v as u32 as i32 as i64 as u64),
                K::Lw1Off => ld!(1, Off, |v: u64| v as u8 as i8 as i32 as u32 as u64),
                K::Lw2Off => ld!(2, Off, |v: u64| v as u16 as i16 as i32 as u32 as u64),
                K::Stp4Off => stp!(4, Off),
                K::Stp8Off => stp!(8, Off),
                K::Ldp4Off => ldp!(4, Off),
                K::Ldp8Off => ldp!(8, Off),
                K::Stp4Pre => stp!(4, Pre),
                K::Stp8Pre => stp!(8, Pre),
                K::Ldp4Pre => ldp!(4, Pre),
                K::Ldp8Pre => ldp!(8, Pre),
                K::Stp4Post => stp!(4, Post),
                K::Stp8Post => stp!(8, Post),
                K::Ldp4Post => ldp!(4, Post),
                K::Ldp8Post => ldp!(8, Post),
                K::AddSp64 => {
                    let n = sp_or_reg!(o!(b));
                    put_sp!(o!(a), n.wrapping_add(o!(imm)))
                },
                K::AddExt64 => {
                    let n = sp_or_reg!(o!(b));
                    put_sp!(o!(a), n.wrapping_add(extended!()))
                },
                K::SubExt64 => {
                    let n = sp_or_reg!(o!(b));
                    put_sp!(o!(a), n.wrapping_sub(extended!()))
                },
                K::AddsExt64 => {
                    let n = sp_or_reg!(o!(b));
                    put!(o!(a), flags!(add_with_carry(n, extended!(), false, true)))
                },
                K::SubsExt64 => {
                    let n = sp_or_reg!(o!(b));
                    put!(o!(a), flags!(add_with_carry(n, !extended!(), true, true)))
                },
                K::AndRs64 => put!(o!(a), reg!(o!(b)) & shifted!(true)),
                K::OrrRs64 => put!(o!(a), reg!(o!(b)) | shifted!(true)),
                K::EorRs64 => put!(o!(a), reg!(o!(b)) ^ shifted!(true)),
                K::AndsRs64 => put!(o!(a), llog!(reg!(o!(b)) & shifted!(true), true)),
                K::AndRs32 => put!(o!(a), (reg!(o!(b)) & shifted!(false)) as u32 as u64),
                K::OrrRs32 => put!(o!(a), (reg!(o!(b)) | shifted!(false)) as u32 as u64),
                K::EorRs32 => put!(o!(a), (reg!(o!(b)) ^ shifted!(false)) as u32 as u64),
                K::AndsRs32 => put!(
                    o!(a),
                    llog!((reg!(o!(b)) & shifted!(false)) as u32 as u64, false)
                ),
                K::Ccmp64 => ccmp!(reg!(o!(c)), true, true),
                K::Ccmn64 => ccmp!(reg!(o!(c)), false, true),
                K::Ccmp32 => ccmp!(reg!(o!(c)), true, false),
                K::Ccmn32 => ccmp!(reg!(o!(c)), false, false),
                K::CcmpI64 => ccmp!(o!(c) as u64, true, true),
                K::CcmnI64 => ccmp!(o!(c) as u64, false, true),
                K::CcmpI32 => ccmp!(o!(c) as u64, true, false),
                K::CcmnI32 => ccmp!(o!(c) as u64, false, false),
                K::AdcSbc => {
                    let x = o!(x);
                    let m = reg!(o!(c));
                    let m = if x & 1 != 0 { !m } else { m };
                    let (r, n, z, c, v) = add_with_carry(reg!(o!(b)), m, carry!(), x & 4 != 0);
                    if x & 2 != 0 {
                        lk = L_EAGER;
                        self.n = n;
                        self.z = z;
                        self.c = c;
                        self.v = v;
                    }
                    put!(o!(a), r)
                },
                K::Dp1 => {
                    let a = reg!(o!(b));
                    let sf = o!(x) >> 8 != 0;
                    let r = match (o!(x) & 0xff, sf) {
                        (0, true) => a.reverse_bits(),
                        (0, false) => (a as u32).reverse_bits() as u64,
                        (1, _) => {
                            let m = 0x00ff_00ff_00ff_00ffu64;
                            ((a >> 8) & m) | ((a & m) << 8)
                        },
                        (2, false) => (a as u32).swap_bytes() as u64,
                        (2, true) => {
                            let lo = (a as u32).swap_bytes() as u64;
                            let hi = ((a >> 32) as u32).swap_bytes() as u64;
                            (hi << 32) | lo
                        },
                        (3, _) => a.swap_bytes(),
                        (4, true) => a.leading_zeros() as u64,
                        (4, false) => (a as u32).leading_zeros() as u64,
                        (_, true) => {
                            let s = a ^ ((a as i64 >> 1) as u64);
                            (s.leading_zeros() as u64).saturating_sub(1)
                        },
                        (_, false) => {
                            let w = a as u32;
                            let s = w ^ ((w as i32 >> 1) as u32);
                            (s.leading_zeros() as u64).saturating_sub(1)
                        },
                    };
                    put!(o!(a), if sf { r } else { r as u32 as u64 })
                },
                K::Div => {
                    let (n, m) = (reg!(o!(b)), reg!(o!(c)));
                    let r = match (o!(x) & 1 != 0, o!(x) >> 8 != 0) {
                        (false, true) => n.checked_div(m).unwrap_or(0),
                        (false, false) => (n as u32).checked_div(m as u32).unwrap_or(0) as u64,
                        (true, true) => {
                            if m == 0 {
                                0
                            } else {
                                (n as i64).wrapping_div(m as i64) as u64
                            }
                        },
                        (true, false) => {
                            if m as u32 == 0 {
                                0
                            } else {
                                (n as i32).wrapping_div(m as i32) as u32 as u64
                            }
                        },
                    };
                    put!(o!(a), r)
                },
                K::Smaddl => {
                    let p =
                        (reg!(o!(b)) as i32 as i64).wrapping_mul(reg!(o!(c)) as i32 as i64) as u64;
                    put!(o!(a), reg!(o!(x)).wrapping_add(p))
                },
                K::Smsubl => {
                    let p =
                        (reg!(o!(b)) as i32 as i64).wrapping_mul(reg!(o!(c)) as i32 as i64) as u64;
                    put!(o!(a), reg!(o!(x)).wrapping_sub(p))
                },
                K::Umaddl => {
                    let p = (reg!(o!(b)) as u32 as u64).wrapping_mul(reg!(o!(c)) as u32 as u64);
                    put!(o!(a), reg!(o!(x)).wrapping_add(p))
                },
                K::Umsubl => {
                    let p = (reg!(o!(b)) as u32 as u64).wrapping_mul(reg!(o!(c)) as u32 as u64);
                    put!(o!(a), reg!(o!(x)).wrapping_sub(p))
                },
                K::Smulh => put!(
                    o!(a),
                    ((reg!(o!(b)) as i64 as i128 * reg!(o!(c)) as i64 as i128) >> 64) as u64
                ),
                K::Umulh => put!(
                    o!(a),
                    ((reg!(o!(b)) as u128 * reg!(o!(c)) as u128) >> 64) as u64
                ),
                K::Extr64 => {
                    let (hi, lo, lsb) = (reg!(o!(b)), reg!(o!(c)), o!(x) as u32);
                    put!(
                        o!(a),
                        if lsb == 0 { lo } else { (lo >> lsb) | (hi << (64 - lsb)) }
                    )
                },
                K::Extr32 => {
                    let (hi, lo, lsb) = (reg!(o!(b)) as u32, reg!(o!(c)) as u32, o!(x) as u32);
                    put!(
                        o!(a),
                        (if lsb == 0 { lo } else { (lo >> lsb) | (hi << (32 - lsb)) }) as u64
                    )
                },
                K::Dmb => {
                    if self.machine.ncpus > 1 {
                        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                    }
                },
                K::VLdLit => {
                    if !self.fp_enabled() {
                        slow_mem!()
                    }
                    let addr = page_pc.wrapping_add(offset!());
                    let Some(v) = self.vread(addr, o!(x) as u32) else {
                        slow_mem!()
                    };
                    *self.vreg.get_unchecked_mut(o!(a) as usize) = v;
                },
                K::VLdp => {
                    if !self.fp_enabled() {
                        slow_mem!()
                    }
                    let scale = (o!(x) & 0xff) as u32;
                    let wb = o!(x) >> 8;
                    let base = base!();
                    let addr = if wb == 1 { base } else { base.wrapping_add(offset!()) };
                    let Some(v1) = self.vread(addr, scale) else {
                        slow_mem!()
                    };
                    let Some(v2) = self.vread(addr.wrapping_add(1 << scale), scale) else {
                        slow_mem!()
                    };
                    *self.vreg.get_unchecked_mut(o!(a) as usize) = v1;
                    *self.vreg.get_unchecked_mut(o!(c) as usize) = v2;
                    if wb != 0 {
                        self.ws(o!(b) as u32, base.wrapping_add(offset!()));
                    }
                },
                K::VStp => {
                    if !self.fp_enabled() {
                        slow_mem!()
                    }
                    let scale = (o!(x) & 0xff) as u32;
                    let wb = o!(x) >> 8;
                    let base = base!();
                    let addr = if wb == 1 { base } else { base.wrapping_add(offset!()) };
                    let (v1, v2) = (
                        *self.vreg.get_unchecked(o!(a) as usize),
                        *self.vreg.get_unchecked(o!(c) as usize),
                    );
                    if !self.vwrite_pair(addr, scale, v1, v2) {
                        slow_mem!()
                    }
                    if wb != 0 {
                        self.ws(o!(b) as u32, base.wrapping_add(offset!()));
                    }
                },
                K::VLdOff => vld!(Off),
                K::VStOff => vst!(Off),
                K::VLdPre => vld!(Pre),
                K::VStPre => vst!(Pre),
                K::VLdPost => vld!(Post),
                K::VStPost => vst!(Post),
                K::VLdReg => vld!(VReg),
                K::VStReg => vst!(VReg),
                K::FSubIEq => fsub!(o!(imm), la == lb),
                K::FSubICs => fsub!(o!(imm), la >= lb),
                K::FSubIMi => fsub!(o!(imm), (la.wrapping_sub(lb) as i64) < 0),
                K::FSubIVs => fsub!(o!(imm), ((la ^ lb) & (la ^ la.wrapping_sub(lb))) >> 63 != 0),
                K::FSubIHi => fsub!(o!(imm), la > lb),
                K::FSubIGe => fsub!(o!(imm), (la as i64) >= (lb as i64)),
                K::FSubIGt => fsub!(o!(imm), (la as i64) > (lb as i64)),
                K::FSubREq => fsub!(reg!(o!(c)), la == lb),
                K::FSubRCs => fsub!(reg!(o!(c)), la >= lb),
                K::FSubRMi => fsub!(reg!(o!(c)), (la.wrapping_sub(lb) as i64) < 0),
                K::FSubRVs => fsub!(
                    reg!(o!(c)),
                    ((la ^ lb) & (la ^ la.wrapping_sub(lb))) >> 63 != 0
                ),
                K::FSubRHi => fsub!(reg!(o!(c)), la > lb),
                K::FSubRGe => fsub!(reg!(o!(c)), (la as i64) >= (lb as i64)),
                K::FSubRGt => fsub!(reg!(o!(c)), (la as i64) > (lb as i64)),
            }
            op = op.add(1);
            if op == end {
                let next = pc!();
                break (Ok(next), done!(0), next.wrapping_sub(4));
            }
        };
        if lk != L_EAGER {
            self.flush_flags(lk, la, lb);
        }
        r
    }
}

#[cfg(test)]
mod tests;
