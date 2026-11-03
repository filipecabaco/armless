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
mod tests {
    use super::*;

    struct NoDevices;
    impl Host for NoDevices {
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

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A register value: often an address in the window the data lives in, else the edge cases and noise.
        fn reg(&mut self) -> u64 {
            match self.next() & 7 {
                0 => 0,
                1 => u64::MAX,
                2..=4 => 0x40000 + (self.next() & 0x7ff),
                _ => self.next(),
            }
        }

        /// A register of the few that the programs of `flag_program` use, or SP.
        fn small_reg(&mut self) -> u32 {
            match self.next() % 10 {
                0 => 31,
                v => (v % 8) as u32,
            }
        }

        /// A short program of what sets the flags (SUBS and CMP, ADDS, ANDS and TST, CCMP, ADCS, FCMP, MSR NZCV)
        /// and of what reads them (B.cond, the selects, CCMP, ADC, MRS NZCV), with the branches forward only.
        fn flag_program(&mut self, n: usize) -> Vec<u32> {
            let mut code: Vec<u32> = Vec::new();
            while code.len() < n {
                let room = (n - 1 - code.len()) as u64;
                let (rd, rn, rm) = (self.small_reg(), self.small_reg(), self.small_reg());
                let cond = (self.next() % 16) as u32;
                // Clears the size bit of an encoding of 64 bits, for the 32-bit one.
                let sf = if self.next() & 1 != 0 { !0x8000_0000u32 } else { u32::MAX };
                let imm6 = (self.next() % 32) as u32;
                let imm12 = (self.next() % 0x1000) as u32;
                let sh = (self.next() % 3) as u32;
                let other = (self.next() % 16) as u32;
                let pick = (self.next() % 4) as usize;
                // A branch 1 to 6 instructions on, at most to the last one.
                let off = (1 + self.next() % 6).min(room) as u32;
                let bcond = if room == 0 { 0xd503_201f } else { 0x5400_0000 | off << 5 | cond };
                let imm_form = (0xf100_0000u32 & sf) | (sh & 1) << 22 | imm12 << 10 | rn << 5;
                let reg_form = (0xeb00_0000u32 & sf) | sh << 22 | rm << 16 | imm6 << 10 | rn << 5;
                match self.next() % 32 {
                    // A compare and the branch on it.
                    0..=9 => {
                        code.push(if pick & 1 != 0 { imm_form | 31 } else { reg_form | 31 });
                        code.push(bcond);
                    },
                    // SUBS, ADDS with an immediate or a register (shifted).
                    10..=13 => code.push(match pick {
                        0 => imm_form | rd,
                        1 => reg_form | rd,
                        2 => (imm_form | rd) ^ 0x4000_0000,
                        _ => (reg_form | rd) ^ 0x4000_0000,
                    }),
                    // ANDS (and TST), the register shifted and maybe inverted.
                    14..=15 => code.push(
                        (0xea00_0000u32 & sf)
                            | (other & 3) << 22
                            | (other >> 2 & 1) << 21
                            | rm << 16
                            | imm6 << 10
                            | rn << 5
                            | if other >> 3 != 0 { 31 } else { rd },
                    ),
                    16..=19 => code.push(bcond),
                    // CSEL, CSINC, CSINV, CSNEG.
                    20..=22 => code.push(
                        ([0x9a80_0000u32, 0x9a80_0400, 0xda80_0000, 0xda80_0400][pick] & sf)
                            | rm << 16
                            | cond << 12
                            | rn << 5
                            | rd,
                    ),
                    // CCMP and CCMN of a register or of an immediate (5 bits, in the place of the register).
                    23..=25 => code.push(
                        ([0xfa40_0000u32, 0xba40_0000][pick & 1] & sf)
                            | if pick & 2 != 0 {
                                0x800 | (other | (16 * (imm6 & 1))) << 16
                            } else {
                                rm << 16
                            }
                            | cond << 12
                            | rn << 5
                            | other,
                    ),
                    // ADC, ADCS, SBC, SBCS.
                    26..=27 => code.push(
                        ([0x9a00_0000u32, 0xba00_0000, 0xda00_0000, 0xfa00_0000][pick] & sf)
                            | rm << 16
                            | rn << 5
                            | rd,
                    ),
                    // MRS and MSR of NZCV, which `exec_slow` runs.
                    28 => code.push(0xd53b_4200 | rd),
                    29 => code.push(0xd51b_4200 | (rd & 7)),
                    // FCMP, which writes the flags in `exec_slow` too.
                    30 => code.push(0x1e60_2000 | (rm & 7) << 16 | (rn & 7) << 5),
                    // ADD of an immediate, with SP for either register.
                    _ => code.push(0x9100_0000 | imm6 << 10 | rn << 5 | rd),
                }
            }
            code.truncate(n);
            code
        }

        /// An instruction word of the classes the loop covers, but not the system ones (SVC, MSR and the like)
        /// other than the hints and barriers.
        fn insn(&mut self) -> u32 {
            loop {
                let insn = self.next() as u32;
                let (crm, op2) = ((insn >> 8) & 15, (insn >> 5) & 7);
                match insn >> 27 {
                    // A hint (not WFI, which has the CPU wait) or a barrier.
                    0 if (crm, op2) != (0, 3) => return 0xd503_201f | crm << 8 | op2 << 5,
                    1 if matches!(op2, 2 | 4 | 5 | 6) => {
                        return 0xd503_3000 | crm << 8 | op2 << 5 | 0x1f
                    },
                    _ => {},
                }
                if insn >> 25 != 0b1101010 {
                    return insn;
                }
            }
        }
    }

    const DATA: std::ops::Range<usize> = 0x3f000..0x42000;

    /// The loop and `execute` (the decoder `exec_slow` reaches) leave the same registers, flags, memory, pc
    /// and exception state after one random instruction.
    #[test]
    fn a_pair_of_simd_stores_is_stored_whole_or_not_at_all() {
        for scale in [2u32, 3, 4] {
            let size = 1usize << scale;
            let mut c = Cpu::new(4 << 20, NoDevices);
            let v1 = 0x1111_2222_3333_4444_5555_6666_7777_8888u128;
            let v2 = !v1;
            // Page 0x40 is served for writes, page 0x41 is not.
            let el = c.el as usize;
            c.fast[el].write_tag[0x40] = 0x40;
            c.fast[el].write_base[0x40] = 0x40000;
            c.mem.write(0x40000, &[0xaa; 0x1000]);
            c.mem.write(0x41000, &[0xaa; 0x1000]);
            // The first component is in page 0x40, the second is not: nothing is stored.
            let va = 0x41000 - size as u64;
            assert!(!c.vwrite_pair(va, scale, v1, v2));
            assert!(c.mem.to_vec()[0x41000 - size..0x42000].iter().all(|&b| b == 0xaa));
            // Both are in page 0x40: both are stored, the low half of each first.
            assert!(c.vwrite_pair(0x40010, scale, v1, v2));
            let got = |c: &Cpu<NoDevices>, a: usize| {
                let mut b = [0u8; 16];
                c.mem.read(a, &mut b[..size]);
                u128::from_le_bytes(b)
            };
            let mask = if scale == 4 { u128::MAX } else { (1u128 << (8 * size)) - 1 };
            assert_eq!(got(&c, 0x10 + 0x40000), v1 & mask);
            assert_eq!(got(&c, 0x10 + 0x40000 + size), v2 & mask);
        }
    }

    #[test]
    fn the_loop_agrees_with_execute_on_random_instructions() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut bad, mut seen) = (Vec::new(), Vec::new());
        // The data pages only: a copy of all of RAM after each of the 400 000 instructions took minutes
        let data = |c: &Cpu<NoDevices>| {
            let mut v = vec![0; DATA.len()];
            c.mem.read(DATA.start, &mut v);
            v
        };
        for _ in 0..400 {
            let mut a = Cpu::new(4 << 20, NoDevices);
            let mut b = Cpu::new(4 << 20, NoDevices);
            // SIMD&FP on in most of them, and off in the others (the access traps).
            let fpen = if rng.next() & 3 != 0 { 3 << 20 } else { 0 };
            a.sys.cpacr = fpen;
            b.sys.cpacr = fpen;
            for i in DATA {
                let v = rng.next() as u8;
                a.mem.write(i, &[v]);
                b.mem.write(i, &[v]);
            }
            for j in 0..1000usize {
                let insn = rng.insn();
                let pc = 0x1000 + 4 * j as u64;
                for c in [&mut a, &mut b] {
                    c.mem.write(pc as usize, &insn.to_le_bytes());
                }
                // The word was written behind the cache's back: the block decoded from the instruction
                // before this one has the old (zero) word in its place.
                a.jit.pre.drop_page((pc >> 12) as usize);
                for r in 0..31 {
                    let v = rng.reg();
                    a.x[r] = v;
                    b.x[r] = v;
                }
                let (s0, s1, f) = (rng.reg(), rng.reg(), rng.next());
                for c in [&mut a, &mut b] {
                    c.sp_el0 = s0;
                    c.sp_el1 = s1;
                    c.n = f & 1 != 0;
                    c.z = f & 2 != 0;
                    c.c = f & 4 != 0;
                    c.v = f & 8 != 0;
                    c.pc = pc;
                }
                a.run(1);
                b.next_pc = pc + 4;
                match b.execute(insn).map(|()| b.next_pc) {
                    Ok(next) => b.pc = next,
                    Err(ev) => {
                        let _ = b.step_event(pc, ev);
                    },
                }
                let why = format!("insn {insn:#010x} at {pc:#x}");
                let diff = if a.x[..32] != b.x[..32] {
                    let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
                    Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
                } else if (a.sp_el0, a.sp_el1) != (b.sp_el0, b.sp_el1) {
                    Some("sp".to_string())
                } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
                    Some("flags".to_string())
                } else if a.vreg != b.vreg {
                    Some("vreg".to_string())
                } else if a.pc != b.pc || a.el != b.el {
                    Some(format!("pc {:#x} against {:#x}", a.pc, b.pc))
                } else if (a.sys.esr, a.sys.elr, a.sys.far) != (b.sys.esr, b.sys.elr, b.sys.far) {
                    Some(format!(
                        "esr/elr/far {:#x} {:#x} {:#x} against {:#x} {:#x} {:#x}",
                        a.sys.esr, a.sys.elr, a.sys.far, b.sys.esr, b.sys.elr, b.sys.far
                    ))
                } else if data(&a) != data(&b) {
                    Some("memory".to_string())
                } else {
                    None
                };
                if let Some(d) = diff {
                    // One report per class of instruction, then a fresh pair of machines.
                    if !seen.contains(&(insn >> 21)) && bad.len() < 40 {
                        seen.push(insn >> 21);
                        bad.push(format!("{why}: {d}"));
                    }
                    break;
                }
                assert_eq!(a.x[31], 0, "{why}");
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
    /// Programs of compares, branches and flag readers run by the loop, in rounds (the page is decoded, and
    /// its compares fused with their branches, by the first one), with a budget that may end between a
    /// compare and its branch, leave what `execute` leaves, one instruction at a time.
    #[test]
    fn the_loop_agrees_with_execute_on_flag_sequences() {
        const N: usize = 32;
        const EDGES: [u64; 8] = [
            0,
            1,
            2,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_ffff,
            0x7fff_ffff_ffff_ffff,
            0x8000_0000_0000_0000,
        ];
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let mut bad = Vec::new();
        for prog in 0..300 {
            let code = rng.flag_program(N);
            let mut a = Cpu::new(4 << 20, NoDevices);
            let mut b = Cpu::new(4 << 20, NoDevices);
            a.sys.cpacr = 3 << 20;
            b.sys.cpacr = 3 << 20;
            for (j, w) in code.iter().enumerate() {
                for c in [&mut a, &mut b] {
                    c.mem.write(0x1000 + 4 * j, &w.to_le_bytes());
                }
            }
            for round in 0..4 {
                let budget = if round == 0 { N as u64 } else { 1 + rng.next() % N as u64 };
                for r in 0..31 {
                    let v = if r < 8 { EDGES[(rng.next() % 8) as usize] } else { rng.reg() };
                    a.x[r] = v;
                    b.x[r] = v;
                }
                let (s0, s1, f) = (rng.reg(), rng.reg(), rng.next());
                for i in 0..8 {
                    let v = (EDGES[(rng.next() % 8) as usize] as u128) << 64 | rng.next() as u128;
                    a.vreg[i] = v;
                    b.vreg[i] = v;
                }
                for c in [&mut a, &mut b] {
                    c.sp_el0 = s0;
                    c.sp_el1 = s1;
                    c.n = f & 1 != 0;
                    c.z = f & 2 != 0;
                    c.c = f & 4 != 0;
                    c.v = f & 8 != 0;
                    c.pc = 0x1000;
                }
                a.run(budget);
                for _ in 0..budget {
                    let pc = b.pc;
                    let insn =
                        b.mem.load(pc as usize, 4) as u32;
                    b.next_pc = pc + 4;
                    match b.execute(insn).map(|()| b.next_pc) {
                        Ok(next) => b.pc = next,
                        Err(ev) => {
                            let _ = b.step_event(pc, ev);
                        },
                    }
                }
                let diff = if a.x[..32] != b.x[..32] {
                    let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
                    Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
                } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
                    Some(format!(
                        "flags {}{}{}{} against {}{}{}{}",
                        a.n as u8,
                        a.z as u8,
                        a.c as u8,
                        a.v as u8,
                        b.n as u8,
                        b.z as u8,
                        b.c as u8,
                        b.v as u8
                    ))
                } else if a.pc != b.pc || a.el != b.el {
                    Some(format!("pc {:#x} against {:#x}", a.pc, b.pc))
                } else if (a.sp_el0, a.sp_el1) != (b.sp_el0, b.sp_el1) {
                    Some("sp".to_string())
                } else {
                    None
                };
                if let Some(d) = diff {
                    if bad.len() < 20 {
                        let words: Vec<String> = code.iter().map(|w| format!("{w:08x}")).collect();
                        bad.push(format!(
                            "program {prog} round {round} budget {budget}: {d}: {}",
                            words.join(" ")
                        ));
                    }
                    break;
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// Programs of the system instructions that the loop runs without leaving it (MRS, MSR to DAIFSet), among
    /// those that leave it (MSR of registers, DAIFClr, SPSel, barriers), at EL0 and EL1, leave what `execute`
    /// leaves, one instruction at a time (the ones the loop runs raise the same exceptions at the same pc).
    #[test]
    fn the_loop_agrees_with_execute_on_system_sequences() {
        use crate::sysreg as sr;
        const N: usize = 16;
        const KEYS: [u32; 22] = [
            sr::MIDR_EL1,
            sr::SCTLR_EL1,
            sr::TTBR0_EL1,
            sr::TCR_EL1,
            sr::ELR_EL1,
            sr::SP_EL0,
            sr::SPSEL,
            sr::CURRENT_EL,
            sr::NZCV,
            sr::DAIF,
            sr::FPCR,
            sr::FPSR,
            sr::ESR_EL1,
            sr::VBAR_EL1,
            sr::TPIDR_EL1,
            sr::TPIDR_EL0,
            sr::TPIDRRO_EL0,
            sr::CNTVCT_EL0,
            sr::CNTFRQ_EL0,
            sr::CNTV_CTL_EL0,
            sr::CNTV_TVAL_EL0,
            sr::ID_AA64MMFR0_EL1,
        ];
        let mut rng = Rng(0x1357_9bdf_2468_ace0);
        let mut bad = Vec::new();
        for prog in 0..300 {
            let code: Vec<u32> = (0..N)
                .map(|_| {
                    let rt = (rng.next() % 32) as u32;
                    let key = KEYS[(rng.next() % KEYS.len() as u64) as usize];
                    let crm = ((rng.next() % 16) as u32) << 8;
                    match rng.next() % 9 {
                        0..=3 => 0xd520_0000 | key << 5 | rt,
                        // a write of the registers that move the code (MMU, vector base) would end the program
                        4 => {
                            let key = match key {
                                sr::SCTLR_EL1 | sr::TTBR0_EL1 | sr::TCR_EL1 | sr::VBAR_EL1 => sr::TPIDR_EL1,
                                k => k,
                            };
                            0xd500_0000 | key << 5 | rt
                        },
                        5 => 0xd503_40df | crm,
                        6 => [0xd503_40ff, 0xd500_40bf][(rng.next() % 2) as usize] | crm,
                        7 => 0xd503_3fdf,
                        _ => 0xd503_201f,
                    }
                })
                .collect();
            let mut a = Cpu::new(4 << 20, NoDevices);
            let mut b = Cpu::new(4 << 20, NoDevices);
            for (j, w) in code.iter().enumerate() {
                for c in [&mut a, &mut b] {
                    c.mem.write(0x1000 + 4 * j, &w.to_le_bytes());
                }
            }
            for round in 0..4 {
                let budget = if round == 0 { N as u64 } else { 1 + rng.next() % N as u64 };
                for r in 0..31 {
                    let v = rng.reg();
                    a.x[r] = v;
                    b.x[r] = v;
                }
                let (el, spsel, daif, uma, f) = (
                    (rng.next() & 1) as u8,
                    rng.next() & 1 != 0,
                    (rng.next() as u32 & 15) << 6,
                    rng.next() & 1,
                    rng.next(),
                );
                for c in [&mut a, &mut b] {
                    c.el = el;
                    c.spsel = spsel;
                    c.daif = daif;
                    c.sys.sctlr = uma << 9;
                    c.n = f & 1 != 0;
                    c.z = f & 2 != 0;
                    c.c = f & 4 != 0;
                    c.v = f & 8 != 0;
                    c.pc = 0x1000;
                }
                a.run(budget);
                for _ in 0..budget {
                    let pc = b.pc;
                    let insn =
                        b.mem.load(pc as usize, 4) as u32;
                    b.next_pc = pc + 4;
                    match b.execute(insn).map(|()| b.next_pc) {
                        Ok(next) => b.pc = next,
                        Err(ev) => {
                            let _ = b.step_event(pc, ev);
                        },
                    }
                }
                let diff = if a.x[..32] != b.x[..32] {
                    let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
                    Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
                } else if (a.pc, a.el, a.daif, a.spsel) != (b.pc, b.el, b.daif, b.spsel) {
                    Some(format!(
                        "pc/el/daif/spsel {:#x} {} {:#x} {} against {:#x} {} {:#x} {}",
                        a.pc, a.el, a.daif, a.spsel, b.pc, b.el, b.daif, b.spsel
                    ))
                } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
                    Some("flags".to_string())
                } else if (
                    a.sp_el0,
                    a.sp_el1,
                    a.sys.tpidr_el0,
                    a.sys.tpidr_el1,
                    a.sys.fpcr,
                    a.sys.fpsr,
                ) != (
                    b.sp_el0,
                    b.sp_el1,
                    b.sys.tpidr_el0,
                    b.sys.tpidr_el1,
                    b.sys.fpcr,
                    b.sys.fpsr,
                ) {
                    Some("registers".to_string())
                } else if (a.sys.esr, a.sys.elr, a.sys.far) != (b.sys.esr, b.sys.elr, b.sys.far) {
                    Some("esr/elr/far".to_string())
                } else {
                    None
                };
                if let Some(d) = diff {
                    if bad.len() < 20 {
                        let words: Vec<String> = code.iter().map(|w| format!("{w:08x}")).collect();
                        bad.push(format!(
                            "program {prog} round {round} budget {budget} el {el}: {d}: {}",
                            words.join(" ")
                        ));
                    }
                    break;
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    // ------------------------------------------------------------ programs of several pages

    /// A host with a JIT that takes the compile requests and never reports one ready, as a compile that is
    /// slow does: the entries it was asked for stay in flight, the interpreter goes on running the code, and
    /// `jit_step` is refused the next compile of them.
    struct Takes;
    impl Host for Takes {
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
        fn jit_compile(&mut self, _: u32, _: &[u8]) -> bool {
            true
        }
    }

    /// What the machine under test has for a JIT.
    #[derive(Clone, Copy, PartialEq)]
    enum Tier {
        /// None: the loop only.
        Off,
        /// The one of `Takes`: it counts the entries and has compiles asked for, which never get ready.
        Stub,
        /// The native one, which compiles the hot entries, so that the loop finds modules to run.
        Native,
    }

    /// The code of the programs: `PAGES` pages from `CODE`, the machine's memory being the address space.
    const CODE: u64 = 0x10000;
    const PAGES: usize = 4;

    fn put<H: Host>(c: &mut Cpu<H>, at: u64, word: u32) {
        c.mem.write(at as usize, &word.to_le_bytes());
    }

    /// `B to` at address `from`.
    fn branch(from: u64, to: u64) -> u32 {
        0x1400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x3ff_ffff)
    }

    /// The instruction at the pc of `b`, run by `execute`, which is how the loop is held to what it does: the
    /// MMU is off in the machine, so the pc is the address in memory.
    fn step_ref<H: Host>(b: &mut Cpu<H>) {
        let pc = b.pc;
        let insn = b.mem.load(pc as usize, 4) as u32;
        b.next_pc = pc + 4;
        match b.execute(insn).map(|()| b.next_pc) {
            Ok(next) => b.pc = next,
            Err(ev) => {
                let _ = b.step_event(pc, ev);
            },
        }
    }

    /// What differs between two machines that ran the same instructions, if anything.
    fn differ<H: Host, G: Host>(a: &Cpu<H>, b: &Cpu<G>) -> Option<String> {
        if a.x[..32] != b.x[..32] {
            let r = (0..32).find(|&r| a.x[r] != b.x[r]).unwrap();
            Some(format!("x{r}: {:#x} against {:#x}", a.x[r], b.x[r]))
        } else if (a.sp_el0, a.sp_el1) != (b.sp_el0, b.sp_el1) {
            Some("sp".to_string())
        } else if (a.n, a.z, a.c, a.v) != (b.n, b.z, b.c, b.v) {
            Some("flags".to_string())
        } else if a.pc != b.pc || a.el != b.el {
            Some(format!("pc {:#x} against {:#x}", a.pc, b.pc))
        } else if (a.sys.esr, a.sys.elr, a.sys.far) != (b.sys.esr, b.sys.elr, b.sys.far) {
            Some(format!(
                "esr/elr/far {:#x} {:#x} {:#x} against {:#x} {:#x} {:#x}",
                a.sys.esr, a.sys.elr, a.sys.far, b.sys.esr, b.sys.elr, b.sys.far
            ))
        } else if a.vreg != b.vreg {
            Some("vreg".to_string())
        } else {
            None
        }
    }

    impl Rng {
        /// A word of a program of `n` words for the branch at word `j` to go to: near it half of the time,
        /// else anywhere in the program (so, mostly, in another page).
        fn target(&mut self, j: usize, n: usize) -> usize {
            if self.next() & 1 != 0 {
                let t = j as i64 + (self.next() % 129) as i64 - 64;
                t.clamp(0, n as i64 - 1) as usize
            } else {
                (self.next() % n as u64) as usize
            }
        }

        /// A program of `PAGES` pages: what `flag_program` writes, with branches of every kind in the place of
        /// some of its instructions, to words anywhere in the program, backwards too: B, BL, B.cond, CBZ and
        /// CBNZ, TBZ and TBNZ, RET (x30), and BR, BLR and RET (x17) behind the MOVZ and MOVK that give x17 the
        /// address of a word. The last word goes back to the first.
        fn paged_program(&mut self) -> Vec<u32> {
            let n = PAGES * 1024;
            let mut code = self.flag_program(n);
            let mut j = 0usize;
            loop {
                j += 1 + (self.next() % 10) as usize;
                if j + 3 >= n - 1 {
                    break;
                }
                let t = self.target(j, n);
                let off = t as i64 - j as i64;
                let imm26 = (off as u32) & 0x3ff_ffff;
                let imm19 = ((off as u32) & 0x7_ffff) << 5;
                let imm14 = ((off as u32) & 0x3fff) << 5;
                let rt = self.small_reg();
                match self.next() % 14 {
                    0..=2 => code[j] = 0x1400_0000 | imm26,
                    3 => code[j] = 0x9400_0000 | imm26,
                    4 => code[j] = 0x5400_0000 | imm19 | (self.next() % 16) as u32,
                    5..=6 => {
                        let op = [0xb400_0000u32, 0xb500_0000, 0x3400_0000, 0x3500_0000];
                        code[j] = op[(self.next() % 4) as usize] | imm19 | rt;
                    },
                    7 => {
                        let bit = (self.next() % 64) as u32;
                        let op = [0x3600_0000u32, 0x3700_0000][(self.next() % 2) as usize];
                        code[j] = op | ((bit >> 5) << 31) | ((bit & 31) << 19) | imm14 | rt;
                    },
                    8 => code[j] = 0xd65f_03c0,
                    9..=11 => {
                        let va = CODE + 4 * t as u64;
                        code[j] = 0xd280_0000 | (((va & 0xffff) as u32) << 5) | 17;
                        code[j + 1] = 0xf2a0_0000 | ((((va >> 16) & 0xffff) as u32) << 5) | 17;
                        let op = [0xd61f_0220u32, 0xd63f_0220, 0xd65f_0220];
                        code[j + 2] = op[(self.next() % 3) as usize];
                        j += 2;
                    },
                    _ => {},
                }
            }
            code[n - 1] = 0x1400_0000 | ((1 - n as i64) as u32 & 0x3ff_ffff);
            code
        }
    }

    /// Programs of several pages, run by the loop in rounds from random words, with budgets around the length
    /// of a run (`PAGE_RUN`) and short ones, leave what `execute` leaves, one instruction at a time: registers,
    /// flags and pc, wherever the run ends (in the middle of a page, between a compare and its branch, in the
    /// first instruction of a page that a branch led to), and the budget is the exact one.
    fn paged_vs_execute(tier: Tier, seed: u64, programs: usize) {
        const EDGES: [u64; 8] = [
            0,
            1,
            2,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_ffff,
            0x7fff_ffff_ffff_ffff,
            0x8000_0000_0000_0000,
        ];
        let n = (PAGES * 1024) as u64;
        let mut rng = Rng(seed);
        let (mut bad, mut changes) = (Vec::new(), 0u64);
        'programs: for prog in 0..programs {
            let code = rng.paged_program();
            let mut a = Cpu::new(4 << 20, Takes);
            let mut b = Cpu::new(4 << 20, Takes);
            a.sys.cpacr = 3 << 20;
            b.sys.cpacr = 3 << 20;
            match tier {
                Tier::Off => {},
                Tier::Stub => {
                    a.jit.enabled = true;
                    a.jit.threshold = 16;
                },
                Tier::Native => {
                    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
                    {
                        if !a.enable_native_jit() {
                            return; // no Cranelift backend for this host
                        }
                        a.jit.threshold = 4;
                    }
                },
            }
            for (j, w) in code.iter().enumerate() {
                put(&mut a, CODE + 4 * j as u64, *w);
                put(&mut b, CODE + 4 * j as u64, *w);
            }
            for round in 0..5 {
                let budget = match round {
                    0 => 255,
                    1 => 256,
                    2 => 257,
                    3 => 1 + rng.next() % 40,
                    _ => 600 + rng.next() % 900,
                };
                let start = CODE + 4 * (rng.next() % n);
                let (r17, r30) = (CODE + 4 * (rng.next() % n), CODE + 4 * (rng.next() % n));
                let (s0, s1, f) = (rng.reg(), rng.reg(), rng.next());
                for r in 0..8 {
                    let v = match rng.next() & 1 {
                        0 => EDGES[(rng.next() % 8) as usize],
                        _ => rng.reg(),
                    };
                    a.x[r] = v;
                    b.x[r] = v;
                }
                for c in [&mut a, &mut b] {
                    c.x[17] = r17;
                    c.x[30] = r30;
                    c.sp_el0 = s0;
                    c.sp_el1 = s1;
                    c.n = f & 1 != 0;
                    c.z = f & 2 != 0;
                    c.c = f & 4 != 0;
                    c.v = f & 8 != 0;
                    c.pc = start;
                }
                let before = a.insn_count;
                let exit = a.run(budget);
                // The modules of the native JIT that the run asked for are there for the next one.
                #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
                a.wait_native_jit();
                let ran = a.insn_count - before;
                for _ in 0..ran {
                    let was = b.pc;
                    step_ref(&mut b);
                    changes += ((was ^ b.pc) >> 12 != 0) as u64;
                }
                // With no vectors installed an exception stops the machine, which leaves the pc where the
                // run began (nothing goes on from there): what it raised is what is compared.
                let stopped = matches!(exit, Exit::Stop(_));
                if stopped {
                    a.pc = b.pc;
                }
                // A module may end a run past the budget, by what is left of its block.
                let diff = if tier != Tier::Native && ran != budget && !stopped {
                    Some(format!("ran {ran} of the budget: {exit:?}, esr {:#x}", a.sys.esr))
                } else {
                    differ(&a, &b)
                };
                if let Some(d) = diff {
                    if bad.len() < 20 {
                        bad.push(format!(
                            "tier {} program {prog} round {round} budget {budget} at {start:#x}: {d}",
                            tier as u8
                        ));
                    }
                    continue 'programs;
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
        assert!(changes > programs as u64 * 10, "only {changes} branches to another page");
    }

    #[test]
    fn the_loop_agrees_with_execute_on_programs_of_several_pages() {
        paged_vs_execute(Tier::Off, 0x2f6e_2b1c_90a3_5d47, 60);
    }

    #[test]
    fn the_loop_agrees_with_execute_on_programs_of_several_pages_with_compiles_in_flight() {
        paged_vs_execute(Tier::Stub, 0x51ed_270a_c4b3_91f5, 40);
    }

    #[test]
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    fn the_loop_agrees_with_execute_on_programs_of_several_pages_with_the_native_jit() {
        paged_vs_execute(Tier::Native, 0x7c3a_9e15_b2d8_4f61, 25);
    }

    /// Two pages that branch to each other, run with every budget up to a few laps: the budget is exact, and
    /// leaves what `execute` leaves, wherever it ends in a page. With both pages held and decoded, one call of
    /// `step_page` goes through all of them (and not out of the page that holds its first instruction).
    #[test]
    fn a_run_goes_on_in_the_page_a_taken_branch_leads_to() {
        for tier in [Tier::Off, Tier::Stub] {
            let mut a = Cpu::new(4 << 20, Takes);
            let mut b = Cpu::new(4 << 20, Takes);
            if tier == Tier::Stub {
                a.jit.enabled = true;
                a.jit.threshold = 16;
            }
            let (p0, p1) = (CODE, CODE + 0x1000);
            for c in [&mut a, &mut b] {
                for k in 0..6u64 {
                    put(c, p0 + 4 * k, 0x9100_0400);
                }
                put(c, p0 + 24, branch(p0 + 24, p1));
                for k in 0..5u64 {
                    put(c, p1 + 4 * k, 0x9100_0c21);
                }
                put(c, p1 + 20, branch(p1 + 20, p0));
                c.pc = p0;
            }
            for budget in 1..=40u64 {
                let before = a.insn_count;
                a.run(budget);
                assert_eq!(a.insn_count - before, budget);
                for _ in 0..budget {
                    step_ref(&mut b);
                }
                assert_eq!(differ(&a, &b), None, "budget {budget}");
            }
            if tier == Tier::Off {
                for c in [&mut a, &mut b] {
                    c.pc = p0;
                }
                let (r, n) = a.step_page(p0, 100);
                assert_eq!(n, 100);
                assert!(matches!(r, Ok(pc) if pc == a.pc));
                for _ in 0..100 {
                    step_ref(&mut b);
                }
                assert_eq!(differ(&a, &b), None);
            }
        }
    }

    /// A store into the code of the page the run goes on in, which the run has gone through before, changes
    /// what it runs there: the store drops the decoded page, and a run that went on with the ops it had would
    /// run the instruction that is not there any more.
    #[test]
    fn a_store_into_the_next_page_changes_what_the_run_does_there() {
        for tier in [Tier::Off, Tier::Stub] {
            let mut a = Cpu::new(4 << 20, Takes);
            let mut b = Cpu::new(4 << 20, Takes);
            if tier == Tier::Stub {
                a.jit.enabled = true;
                a.jit.threshold = 16;
            }
            let (p0, p1) = (CODE, CODE + 0x1000);
            for c in [&mut a, &mut b] {
                // str w1, [x2]; add w1, w1, w4; add x5, x5, #1; b p1
                put(c, p0, 0xb900_0041);
                put(c, p0 + 4, 0x0b04_0021);
                put(c, p0 + 8, 0x9100_04a5);
                put(c, p0 + 12, branch(p0 + 12, p1));
                // add x0, x0, #1 nine times, the fourth one the instruction that is stored over; b p0
                for k in 0..9u64 {
                    put(c, p1 + 4 * k, 0x9100_0400);
                }
                put(c, p1 + 36, branch(p1 + 36, p0));
                // The stored word is add x0, x0, #imm with imm one more each time.
                c.x[1] = 0x9100_0800;
                c.x[2] = p1 + 12;
                c.x[4] = 0x400;
                c.pc = p0;
            }
            for round in 0..4 {
                a.run(700);
                for _ in 0..700 {
                    step_ref(&mut b);
                }
                assert_eq!(differ(&a, &b), None, "round {round}");
                let same = a.mem.to_vec()[0x10000..0x12000] == b.mem.to_vec()[0x10000..0x12000];
                assert!(same, "round {round}: memory");
                assert!(a.x[5] >= 40 * (round + 1), "round {round}: only {} laps", a.x[5]);
            }
        }
    }

    /// A machine with the MMU on: a space of 39 bits, 4 KiB pages, one table for each of the three levels (at
    /// 0x80000, 0x81000, 0x82000), the first 0x20 pages mapped to themselves, EL1, the vectors in page 0 where
    /// `b .` stands, so that a fault leaves the machine spinning.
    fn mapped() -> Cpu<Takes> {
        let mut c = Cpu::new(4 << 20, Takes);
        c.sys.cpacr = 3 << 20;
        c.sys.sctlr |= 1;
        c.sys.tcr = 25;
        c.sys.ttbr0 = 0x80000;
        c.mem.write(0x80000, &(0x81000u64 | 3).to_le_bytes());
        c.mem.write(0x81000, &(0x82000u64 | 3).to_le_bytes());
        for page in 0..0x20u64 {
            map(&mut c, page, (page << 12) | 0x403);
        }
        put(&mut c, 0x0, 0x1400_0000);
        put(&mut c, 0x200, 0x1400_0000);
        c
    }

    /// The descriptor of the level 3 table for virtual page `page`.
    fn map(c: &mut Cpu<Takes>, page: u64, desc: u64) {
        let at = 0x82000 + 8 * page as usize;
        c.mem.write(at, &desc.to_le_bytes());
    }

    /// `a` runs `n` instructions in one call, `b` one at a time (so that no run of it goes from a page to
    /// another): they leave the same state, and the same count.
    fn lockstep(a: &mut Cpu<Takes>, b: &mut Cpu<Takes>, n: u64, why: &str) {
        a.run(n);
        for _ in 0..n {
            let _ = b.run(1);
        }
        assert_eq!(a.insn_count, b.insn_count, "{why}");
        assert_eq!(differ(a, b), None, "{why}");
    }

    /// A taken branch to a page whose fetch faults raises what that fetch raises, at the target: the page was
    /// unmapped or not executable, or the translation of it changed, and the TLB was maintained for the page,
    /// or all of it (when it was not, the stale translation is what the machine still uses, as it is one
    /// instruction at a time). A target that is not aligned raises the alignment fault of the fetch.
    #[test]
    fn a_branch_to_a_page_that_the_fetch_faults_on_raises_that_fault() {
        const T: u64 = 0x11000;
        // The branch to T that follows `add x0, x0, #1` at the start of page 0x10: x1 is 7, x17 and x30 are T.
        let kinds: [u32; 7] = [
            0x1400_0000 | 0x3ff,                    // b
            0x5400_0001 | (0x3ff << 5),             // b.ne
            0xb500_0001 | (0x3ff << 5),             // cbnz x1
            0x3700_0001 | (1 << 19) | (0x3ff << 5), // tbnz x1, #1
            0xd61f_0220,                            // br x17
            0xd63f_0220,                            // blr x17
            0xd65f_03c0,                            // ret
        ];
        for (ki, &kind) in kinds.iter().enumerate() {
            for mode in 0..3 {
                for maint in 0..3 {
                    let why = format!("branch {ki} mode {mode} maintenance {maint}");
                    let mut a = mapped();
                    let mut b = mapped();
                    for c in [&mut a, &mut b] {
                        put(c, 0x10000, 0x9100_0400);
                        put(c, 0x10004, kind);
                        put(c, 0x10008, branch(0x10008, 0x10000));
                        // The page the branches lead to, and the one it is mapped to after.
                        put(c, 0x11000, 0x9100_0442); // add x2, x2, #1
                        put(c, 0x11004, branch(0x11004, 0x10000));
                        put(c, 0x13000, 0x9100_0463); // add x3, x3, #1
                        // (the page runs at 0x11000, mapped to this: its branch is the one of address 0x11004)
                        put(c, 0x13004, branch(0x11004, 0x10000));
                        c.x[1] = 7;
                        c.x[17] = T;
                        c.x[30] = T;
                        c.pc = 0x10000;
                    }
                    lockstep(&mut a, &mut b, 60, &why);
                    let laps = a.x[2];
                    assert!(laps > 5, "{why}: the page ran {laps} times");
                    for c in [&mut a, &mut b] {
                        match mode {
                            0 => map(c, 0x11, 0),
                            1 => map(c, 0x11, 0x11000 | 0x403 | (1 << 53)),
                            _ => map(c, 0x11, 0x13000 | 0x403),
                        }
                        match maint {
                            0 => c.tlb_flush_page(0x11),
                            1 => c.tlb_flush(),
                            _ => {},
                        }
                        c.x[17] = T;
                        c.x[30] = T;
                        c.pc = 0x10000;
                    }
                    lockstep(&mut a, &mut b, 60, &why);
                    if maint == 2 {
                        continue;
                    }
                    match mode {
                        0 | 1 => {
                            assert_eq!(a.sys.esr >> 26, 0x21, "{why}");
                            assert_eq!((a.sys.far, a.sys.elr), (T, T), "{why}");
                            assert_eq!(a.x[2], laps, "{why}: ran the page that faults");
                        },
                        _ => {
                            assert!(a.x[3] > 5, "{why}: the new page did not run");
                            assert_eq!(a.x[2], laps, "{why}: ran the old page");
                        },
                    }
                }
            }
        }
        for (ki, &kind) in kinds[4..].iter().enumerate() {
            let why = format!("misaligned branch {ki}");
            let mut a = mapped();
            let mut b = mapped();
            for c in [&mut a, &mut b] {
                put(c, 0x10000, 0x9100_0400);
                put(c, 0x10004, kind);
                put(c, 0x11000, 0x9100_0442);
                c.x[17] = T + 2;
                c.x[30] = T + 2;
                c.pc = 0x10000;
            }
            lockstep(&mut a, &mut b, 20, &why);
            assert_eq!(a.sys.esr >> 26, 0x22, "{why}");
            assert_eq!((a.sys.far, a.sys.elr), (T + 2, T + 2), "{why}");
            assert_eq!(a.x[2], 0, "{why}: ran the page");
        }
    }

    /// The entry of a loop that the JIT refused (a compile that has it is in flight) is not sent back to
    /// `jit_step` by each execution of it: its count is left below the threshold, from where it gets there
    /// again after some executions. The loop is run as `execute` runs it.
    #[test]
    fn an_entry_that_the_jit_refused_is_counted_again_from_below_the_threshold() {
        let mut a = Cpu::new(4 << 20, Takes);
        let mut b = Cpu::new(4 << 20, Takes);
        a.jit.enabled = true;
        a.jit.threshold = 64;
        // add x1, x1, #1; subs x0, x0, #1; b.ne the add; b .
        let code = [0x9100_0421u32, 0xf100_0400, 0x54ff_ffc1, 0x1400_0000];
        for c in [&mut a, &mut b] {
            for (k, w) in code.iter().enumerate() {
                put(c, CODE + 4 * k as u64, *w);
            }
            c.x[0] = 3000;
            c.pc = CODE;
        }
        let steps = 3 * 3000 + 1;
        a.run(steps);
        for _ in 0..steps {
            step_ref(&mut b);
        }
        assert_eq!(a.insn_count, steps);
        assert_eq!(differ(&a, &b), None);
        assert_eq!(a.x[1], 3000);
        assert!(a.jit.compiled >= 1, "the loop was never compiled");
        let ops = a.jit.pre.enter(CODE as usize);
        assert!(!ops.is_null());
        // SAFETY: `ops` is the ops of the page at CODE, a live slot of 1024 ops or more.
        let hottest = (0..1024).map(|i| unsafe { (*ops.add(i)).cnt }).max().unwrap();
        assert!(hottest < 64, "the count of the entry is {hottest}: each execution exits");
    }
}
