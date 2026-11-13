//! Memory: an address space for each process, made of frames from one pool.
//!
//! A process's space is a set of areas (`Vma`: a range, its protection, and the file its pages come
//! from if any) and the page tables the CPU walks to reach them (4 KiB pages, one root per space,
//! tagged with an ASID of its own so a switch between processes flushes nothing). The tables live
//! in frames of the pool like the pages do, and no page of any process maps them. VA is not PA:
//! a page has a frame when something first touches it, so what a program maps costs nothing until
//! it uses it: a reservation (PROT_NONE, or MAP_NORESERVE) of far more than the RAM there is
//! (a JavaScript engine's heap cage, a WebAssembly memory's guard region) costs the tables of the
//! pages touched and those pages, and munmap or madvise(MADV_DONTNEED) gives them back. There is
//! no overcommit check: a program that touches more than the RAM has is stopped when the pool
//! runs dry, not when it maps. The touch is a translation fault, which
//! stops the CPU (`stop_on_exceptions`): the area says what the page is (zeros, or a part of a
//! file, read now, or its thread waits for the blobs that needs and touches it again once they
//! arrived), a frame is taken and mapped, and the instruction runs again. A snapshot therefore
//! carries only the pages a program used, and the filesystem fetches only the blobs of those.
//!
//! Frames are counted. A fork copies the tables, not the pages: both sides map the same frames
//! read-only (a writable page of an area that isn't MAP_SHARED), and the first write by either is
//! a permission fault, answered by taking a frame of its own for the page (or, when the other side
//! is gone, by making it writable again). A frame whose count falls to zero is zeroed, the host
//! given its memory back, and the code the CPU decoded from it dropped.
//!
//! What the host reads or writes of the program's memory for a system call (a write(2) from a
//! page) goes through the same tables, in software (`read`, `write`): it faults pages in, and
//! breaks sharing, as a CPU access would, and refuses (EFAULT) what the areas don't allow.
//!
//! The CPU's TLB is told of what a call changes after it (`sync_mmu`): the whole of it when pages
//! went away or lost permissions, the pages of a break of sharing otherwise.

use std::collections::{BTreeMap, HashSet};

use armless::cpu::Cpu;
use armless::dev::fs::Ino;
use armless::{Host, Ram};

use crate::elf::Seg;
use crate::files::{got, Desc, Got, Kind};
use crate::ser::{R, W};
use crate::signal::SIGILL;
use crate::{page_up, ExecError, Process, Ret, Wait, PAGE};

pub(crate) const PROT_READ: u8 = 1;
pub(crate) const PROT_WRITE: u8 = 2;
pub(crate) const PROT_EXEC: u8 = 4;

/// Descriptor bits. A page (at level 3) or a table: valid; for memory: accessed, inner shareable,
/// attributes 0 of MAIR (normal, write-back), not global (the ASID tags it); AP[1] lets EL0 in,
/// AP[2] makes it read-only; UXN keeps EL0 from executing it.
const VALID: u64 = 3;
const MEM: u64 = (1 << 10) | (3 << 8) | (1 << 11);
const AP_EL0: u64 = 1 << 6;
const AP_RO: u64 = 1 << 7;
const UXN: u64 = 1 << 54;
const ADDR: u64 = 0x0000_ffff_ffff_f000;
/// TCR_EL1: a 39-bit space from level 1 (T0SZ 25), write-back walks, inner shareable, 4 KiB
/// granule, 16-bit ASIDs, no walks from TTBR1 (EPD1), 40-bit physical addresses.
const TCR: u64 = 25 | (1 << 8) | (1 << 10) | (3 << 12) | (1 << 23) | (1 << 36) | (2 << 32);
const MAIR: u64 = 0xff;
/// The end of the space a 39-bit table reaches: 512 GiB of address space for each process, far
/// more than there is RAM, since a page has a frame only when something touches it.
pub(crate) const VA_END: u64 = 1 << 39;
/// A futex key's mark for one that is a frame's (`Process::futex_key`)
pub(crate) const SHARED_FUTEX: u64 = 1 << 63;

const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EEXIST: i64 = 17;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;

fn bits(prot: u8, writable: bool) -> u64 {
    let mut d = VALID | MEM;
    if prot & (PROT_READ | PROT_WRITE) != 0 && writable && prot & PROT_WRITE != 0 {
        d |= AP_EL0;
    } else if prot & (PROT_READ | PROT_WRITE) != 0 {
        d |= AP_EL0 | AP_RO;
    } else {
        d |= AP_RO;
    }
    if prot & PROT_EXEC == 0 {
        d |= UXN;
    }
    d
}

fn writable(d: u64) -> bool {
    d & (3 << 6) == AP_EL0
}

fn idx(va: u64, level: u32) -> u64 {
    (va >> (12 + 9 * (3 - level))) & 511
}

fn eager() -> bool {
    std::env::var("ARMLESS_USER_EAGER").is_ok_and(|v| v == "1")
}

// ---------------------------------------------------------------- frames

/// The pool of frames (pages of RAM): counted, and zero while free.
pub(crate) struct Frames {
    ram: Ram,
    refs: Vec<u32>,
    /// Frames below this have been handed out at some point; none above ever was
    next: u64,
    free: Vec<u64>,
    /// Frames whose count fell to zero, not yet zeroed (`reap`)
    dying: Vec<u64>,
    /// Drops what the CPUs decoded or compiled from a physical page
    purge: Box<dyn Fn(u64) + Send>,
    pub(crate) live: u64,
}

impl Frames {
    pub(crate) fn new(ram: Ram, purge: Box<dyn Fn(u64) + Send>) -> Frames {
        let n = ram.len() / PAGE as usize;
        Frames {
            ram,
            refs: vec![0; n],
            next: 0,
            free: Vec::new(),
            dying: Vec::new(),
            purge,
            live: 0,
        }
    }

    /// A zeroed frame (its address), counted once.
    pub(crate) fn alloc(&mut self) -> Option<u64> {
        let f = match self.free.pop() {
            Some(f) => f,
            None if self.next < self.refs.len() as u64 => {
                self.next += 1;
                self.next - 1
            },
            None => {
                self.reap();
                self.free.pop()?
            },
        };
        self.refs[f as usize] = 1;
        self.live += 1;
        Some(f * PAGE)
    }

    pub(crate) fn incref(&mut self, pa: u64) {
        self.refs[(pa / PAGE) as usize] += 1;
    }

    pub(crate) fn decref(&mut self, pa: u64) {
        let r = &mut self.refs[(pa / PAGE) as usize];
        *r -= 1;
        if *r == 0 {
            self.dying.push(pa / PAGE);
            self.live -= 1;
        }
    }

    pub(crate) fn count(&self, pa: u64) -> u32 {
        self.refs[(pa / PAGE) as usize]
    }

    /// The frames freed so far zeroed, their memory given back to the host, and what was decoded
    /// from them dropped: free for `alloc`.
    pub(crate) fn reap(&mut self) {
        if self.dying.is_empty() {
            return;
        }
        self.dying.sort_unstable();
        let mut i = 0;
        while i < self.dying.len() {
            let start = self.dying[i];
            let mut n = 1;
            while i + (n as usize) < self.dying.len() && self.dying[i + n as usize] == start + n {
                n += 1;
            }
            self.ram
                .discard((start * PAGE) as usize, (n * PAGE) as usize);
            for p in start..start + n {
                (self.purge)(p);
            }
            i += n as usize;
        }
        let dead = std::mem::take(&mut self.dying);
        self.free.extend(dead.into_iter().rev());
    }

    /// Drops the code decoded from the frame at `pa`, after the host wrote it.
    pub(crate) fn purge(&self, pa: u64) {
        (self.purge)(pa / PAGE);
    }

    fn get(&self, at: u64) -> u64 {
        self.ram.load(at as usize, 8)
    }

    fn put(&self, at: u64, v: u64) {
        self.ram.store(at as usize, 8, v);
    }

    fn copy(&self, from: u64, to: u64) {
        let mut b = vec![0u8; PAGE as usize];
        self.ram.read(from as usize, &mut b);
        self.ram.write(to as usize, &b);
    }

    pub(crate) fn save(&mut self, w: &mut W) {
        self.reap();
        w.u(self.next);
        let used: Vec<(usize, u32)> = self.refs[..self.next as usize]
            .iter()
            .enumerate()
            .filter(|(_, &r)| r > 0)
            .map(|(i, &r)| (i, r))
            .collect();
        w.u(used.len() as u64);
        for (f, r) in used {
            w.u(f as u64);
            w.u(r as u64);
        }
    }

    pub(crate) fn load(&mut self, r: &mut R) -> Result<(), String> {
        self.next = r.u()?;
        if self.next > self.refs.len() as u64 {
            return Err("bad frame pool".into());
        }
        self.refs.fill(0);
        self.dying.clear();
        self.live = 0;
        for _ in 0..r.u()? {
            let (f, c) = (r.u()? as usize, r.u()? as u32);
            if f >= self.next as usize || c == 0 {
                return Err("bad frame pool".into());
            }
            self.refs[f] = c;
            self.live += 1;
        }
        self.free = (0..self.next)
            .filter(|&f| self.refs[f as usize] == 0)
            .rev()
            .collect();
        Ok(())
    }
}

// ---------------------------------------------------------------- address spaces

/// The file a range's pages come from: the page at `p` holds the file's bytes from
/// `off + (p - start)`, up to the address `data_end` (zeros past it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Back {
    pub ino: Ino,
    pub off: u64,
    pub data_end: u64,
}

/// An area `start..end` (the start is its key), page aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Vma {
    pub end: u64,
    pub prot: u8,
    /// Shared with the processes forked from this one (MAP_SHARED, anonymous): its pages are not
    /// copied on write
    pub shared: bool,
    pub file: Option<Back>,
}

fn joins(a: &Vma, b: &Vma) -> bool {
    a.file.is_none() && b.file.is_none() && a.prot == b.prot && a.shared == b.shared
}

pub(crate) struct Mm {
    /// The root table's frame
    pub root: u64,
    pub asid: u16,
    /// The generation of ASIDs `asid` is from (`sync_mmu`)
    pub gen: u64,
    /// The CPU's translations of this space are to be flushed: all of them, or these pages
    stale: bool,
    flush: Vec<u64>,
    pub vmas: BTreeMap<u64, Vma>,
    pub brk_start: u64,
    pub brk: u64,
    pub mmap_floor: u64,
    pub mmap_top: u64,
    /// Free ranges of the mmap area, start -> end, coalesced
    pub free: BTreeMap<u64, u64>,
    /// Pages with file contents not read yet, per file: the space holds a reference to a file while
    /// it has some, so a mapped file that is unlinked keeps its contents.
    per_file: BTreeMap<Ino, u64>,
    /// References to take (true) or give back, for the filesystem (`Process::settle`)
    refs: Vec<(Ino, bool)>,
}

impl Mm {
    pub(crate) fn new(
        f: &mut Frames,
        brk_start: u64,
        mmap_floor: u64,
        mmap_top: u64,
    ) -> Option<Mm> {
        Some(Mm {
            root: f.alloc()?,
            asid: 0,
            gen: u64::MAX,
            stale: true,
            flush: Vec::new(),
            vmas: BTreeMap::new(),
            brk_start,
            brk: brk_start,
            mmap_floor,
            mmap_top,
            free: BTreeMap::from([(mmap_floor, mmap_top)]),
            per_file: BTreeMap::new(),
            refs: Vec::new(),
        })
    }

    /// An address space with nothing in it, for a process that has ended
    pub(crate) fn husk() -> Mm {
        Mm {
            root: 0,
            asid: 0,
            gen: u64::MAX,
            stale: false,
            flush: Vec::new(),
            vmas: BTreeMap::new(),
            brk_start: 0,
            brk: 0,
            mmap_floor: 0,
            mmap_top: 0,
            free: BTreeMap::new(),
            per_file: BTreeMap::new(),
            refs: Vec::new(),
        }
    }

    // ------------------------------------------------------------ page tables

    /// The address of the level-3 descriptor of `va`, if its tables exist.
    fn slot(&self, f: &Frames, va: u64) -> Option<u64> {
        let mut t = self.root;
        for level in 1..=2 {
            let d = f.get(t + 8 * idx(va, level));
            if d & 1 == 0 {
                return None;
            }
            t = d & ADDR;
        }
        Some(t + 8 * idx(va, 3))
    }

    pub(crate) fn pte(&self, f: &Frames, va: u64) -> u64 {
        self.slot(f, va).map_or(0, |s| f.get(s))
    }

    /// Maps `va` with the descriptor `d`, making the tables it needs. False when there is no frame
    /// for one.
    fn set(&self, f: &mut Frames, va: u64, d: u64) -> bool {
        let mut t = self.root;
        for level in 1..=2 {
            let at = t + 8 * idx(va, level);
            let mut e = f.get(at);
            if e & 1 == 0 {
                let Some(n) = f.alloc() else {
                    return false;
                };
                e = n | VALID;
                f.put(at, e);
            }
            t = e & ADDR;
        }
        f.put(t + 8 * idx(va, 3), d);
        true
    }

    /// The frames `frames` mapped from `a` on, page after page, as `prot` (a shared area's: System
    /// V shared memory, whose segment holds the frames), each counted once more. False when there
    /// is no frame for a table.
    pub(crate) fn map_frames(&mut self, f: &mut Frames, a: u64, frames: &[u64], prot: u8) -> bool {
        for (i, &pa) in frames.iter().enumerate() {
            if !self.set(f, a + i as u64 * PAGE, pa | bits(prot, true)) {
                return false;
            }
            f.incref(pa);
        }
        self.stale = true;
        true
    }

    /// The valid pages of `a..b`: (address, descriptor's address, descriptor).
    fn leaves(&self, f: &Frames, a: u64, b: u64) -> Vec<(u64, u64, u64)> {
        let mut out = Vec::new();
        let mut va = a & !(PAGE - 1);
        let b = b.min(VA_END);
        while va < b {
            let d1 = f.get(self.root + 8 * idx(va, 1));
            if d1 & 1 == 0 {
                va = (va | ((1 << 30) - 1)) + 1;
                continue;
            }
            let t2 = d1 & ADDR;
            let d2 = f.get(t2 + 8 * idx(va, 2));
            if d2 & 1 == 0 {
                va = (va | ((1 << 21) - 1)) + 1;
                continue;
            }
            let t3 = d2 & ADDR;
            let end = ((va | ((1 << 21) - 1)) + 1).min(b);
            while va < end {
                let slot = t3 + 8 * idx(va, 3);
                let d = f.get(slot);
                if d & 1 != 0 {
                    out.push((va, slot, d));
                }
                va += PAGE;
            }
        }
        out
    }

    // ------------------------------------------------------------ areas

    pub(crate) fn find(&self, va: u64) -> Option<(u64, Vma)> {
        self.vmas
            .range(..=va)
            .next_back()
            .filter(|(_, v)| v.end > va)
            .map(|(&s, &v)| (s, v))
    }

    /// An area begins at `at` (the one that holds it is split there).
    fn split(&mut self, at: u64) {
        let Some((&s, &v)) = self.vmas.range(..at).next_back() else {
            return;
        };
        if v.end <= at {
            return;
        }
        self.vmas.insert(s, Vma { end: at, ..v });
        self.vmas.insert(
            at,
            Vma {
                file: v.file.map(|b| Back {
                    off: b.off + (at - s),
                    ..b
                }),
                ..v
            },
        );
    }

    /// Takes `a..b` out of the areas: the pieces taken.
    fn cut(&mut self, a: u64, b: u64) -> Vec<(u64, Vma)> {
        self.split(a);
        self.split(b);
        let keys: Vec<u64> = self.vmas.range(a..b).map(|(&s, _)| s).collect();
        keys.into_iter()
            .map(|s| (s, self.vmas.remove(&s).unwrap()))
            .collect()
    }

    /// Joins the anonymous areas next to each other around `a..b`.
    fn merge(&mut self, a: u64, b: u64) {
        let from = self.vmas.range(..a).next_back().map_or(a, |(&s, _)| s);
        let keys: Vec<u64> = self.vmas.range(from..=b).map(|(&s, _)| s).collect();
        let mut prev: Option<u64> = None;
        for s in keys {
            if let Some(p) = prev {
                let (v1, v2) = (self.vmas[&p], self.vmas[&s]);
                if v1.end == s && joins(&v1, &v2) {
                    self.vmas.insert(p, Vma { end: v2.end, ..v1 });
                    self.vmas.remove(&s);
                    continue;
                }
            }
            prev = Some(s);
        }
    }

    /// Whether `a..b` is all areas.
    pub(crate) fn covered(&self, a: u64, b: u64) -> bool {
        let mut at = a;
        while at < b {
            match self.find(at) {
                Some((_, v)) => at = v.end,
                None => return false,
            }
        }
        true
    }

    fn overlaps(&self, a: u64, b: u64) -> bool {
        self.find(a).is_some() || self.vmas.range(a..b).next().is_some()
    }

    /// Pages of `file` with contents to read, `n` more (or fewer)
    fn lazy_add(&mut self, ino: Ino, n: u64) {
        if n == 0 {
            return;
        }
        let c = self.per_file.entry(ino).or_default();
        if *c == 0 {
            self.refs.push((ino, true));
        }
        *c += n;
    }

    fn lazy_less(&mut self, ino: Ino, n: u64) {
        let Some(c) = self.per_file.get_mut(&ino) else {
            return;
        };
        *c = c.saturating_sub(n);
        if *c == 0 {
            self.per_file.remove(&ino);
            self.refs.push((ino, false));
        }
    }

    /// Pages of `a..b` with file contents (those before `data_end`), of an area at `s`
    fn data_pages(v: &Vma, s: u64) -> u64 {
        v.file.map_or(0, |b| {
            page_up(b.data_end).min(v.end).saturating_sub(s) / PAGE
        })
    }

    /// Takes a new area in: `a..b` is clear.
    pub(crate) fn insert(&mut self, a: u64, b: u64, v: Vma) {
        if let Some(bk) = v.file {
            let end = page_up(bk.data_end).min(b);
            if end > a {
                self.lazy_add(bk.ino, (end - a) / PAGE);
            }
        }
        self.vmas.insert(a, Vma { end: b, ..v });
        self.merge(a, b);
    }

    /// The tables `a..b` reaches that have no page left in them, given back.
    fn trim(&mut self, f: &mut Frames, a: u64, b: u64) {
        let b = b.min(VA_END);
        if a >= b {
            return;
        }
        let (first, last) = (idx(a, 1), idx(b - 1, 1));
        for i1 in first..=last {
            let at1 = self.root + 8 * i1;
            let d1 = f.get(at1);
            if d1 & 1 == 0 {
                continue;
            }
            let t2 = d1 & ADDR;
            let lo = if i1 == first { idx(a, 2) } else { 0 };
            let hi = if i1 == last { idx(b - 1, 2) } else { 511 };
            for i2 in lo..=hi {
                let at2 = t2 + 8 * i2;
                let d2 = f.get(at2);
                if d2 & 1 != 0 && (0..512).all(|i| f.get((d2 & ADDR) + 8 * i) & 1 == 0) {
                    f.put(at2, 0);
                    f.decref(d2 & ADDR);
                }
            }
            if (0..512).all(|i| f.get(t2 + 8 * i) & 1 == 0) {
                f.put(at1, 0);
                f.decref(t2);
            }
        }
    }

    /// `a..b` unmapped: its areas gone, its frames given back, and the tables left with nothing.
    pub(crate) fn unmap(&mut self, f: &mut Frames, a: u64, b: u64) {
        let mut taken = false;
        for (s, v) in self.cut(a, b) {
            let data_end = v.file.map_or(0, |b| page_up(b.data_end));
            let mut filled = 0;
            for (va, slot, d) in self.leaves(f, s, v.end) {
                f.put(slot, 0);
                f.decref(d & ADDR);
                taken = true;
                if va < data_end {
                    filled += 1;
                }
            }
            if let Some(bk) = v.file {
                self.lazy_less(bk.ino, Mm::data_pages(&v, s).saturating_sub(filled));
            }
        }
        if taken {
            self.trim(f, a, b);
        }
        self.stale = true;
        f.reap();
    }

    /// The pages of the anonymous areas in `a..b` given back: they read as zeros again.
    pub(crate) fn discard(&mut self, f: &mut Frames, a: u64, b: u64) {
        for (&s, v) in self.vmas.range(..b) {
            if v.end <= a || v.file.is_some() || v.shared {
                continue;
            }
            for (_, slot, d) in self.leaves(f, s.max(a), v.end.min(b)) {
                f.put(slot, 0);
                f.decref(d & ADDR);
            }
        }
        self.stale = true;
        f.reap();
    }

    /// `a..b` with the protection `prot`: its areas, and the pages it has.
    pub(crate) fn protect(&mut self, f: &mut Frames, a: u64, b: u64, prot: u8) {
        self.split(a);
        self.split(b);
        let keys: Vec<u64> = self.vmas.range(a..b).map(|(&s, _)| s).collect();
        for s in keys {
            let v = self.vmas.get_mut(&s).unwrap();
            v.prot = prot;
            let (end, shared) = (v.end, v.shared);
            for (_, slot, d) in self.leaves(f, s, end) {
                f.put(slot, (d & ADDR) | bits(prot, shared || writable(d)));
            }
        }
        self.stale = true;
        self.merge(a, b);
    }

    /// `a..b` made areas of `prot`: where there is none, a new one, and the ones there widened.
    pub(crate) fn ensure(&mut self, f: &mut Frames, a: u64, b: u64, prot: u8) {
        let mut at = a;
        while at < b {
            match self.find(at) {
                Some((_, v)) => {
                    let e = v.end.min(b);
                    if v.prot | prot != v.prot {
                        self.protect(f, at, e, v.prot | prot);
                    }
                    at = e;
                },
                None => {
                    let e = self.vmas.range(at..).next().map_or(b, |(&s, _)| s).min(b);
                    self.insert(
                        at,
                        e,
                        Vma {
                            end: e,
                            prot,
                            shared: false,
                            file: None,
                        },
                    );
                    at = e;
                },
            }
        }
    }

    /// `old..old + len` moved to `new` (clear): its areas, and the frames it has, without copying.
    /// False when there are no frames for the tables, and then nothing moved.
    pub(crate) fn relocate(&mut self, f: &mut Frames, old: u64, len: u64, new: u64) -> bool {
        let leaves = self.leaves(f, old, old + len);
        for (i, &(va, _, d)) in leaves.iter().enumerate() {
            if !self.set(f, new + (va - old), d) {
                for &(va, _, _) in &leaves[..i] {
                    if let Some(s) = self.slot(f, new + (va - old)) {
                        f.put(s, 0);
                    }
                }
                return false;
            }
        }
        for &(_, slot, _) in &leaves {
            f.put(slot, 0);
        }
        for (s, v) in self.cut(old, old + len) {
            self.vmas.insert(
                new + (s - old),
                Vma {
                    end: new + (v.end - old),
                    file: v.file.map(|b| Back {
                        data_end: b.data_end + new - old,
                        ..b
                    }),
                    ..v
                },
            );
        }
        self.stale = true;
        self.merge(new, new + len);
        true
    }

    // ------------------------------------------------------------ the mmap area

    /// Highest free range of `len` bytes.
    pub(crate) fn alloc(&mut self, len: u64) -> Option<u64> {
        let (&s, &e) = self.free.iter().rev().find(|(&s, &e)| e - s >= len)?;
        let at = e - len;
        self.free.remove(&s);
        if at > s {
            self.free.insert(s, at);
        }
        Some(at)
    }

    /// Takes `[a, b)` out of the free ranges (MAP_FIXED over it).
    pub(crate) fn carve(&mut self, a: u64, b: u64) {
        let hit: Vec<(u64, u64)> = self
            .free
            .range(..b)
            .filter(|(_, &e)| e > a)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in hit {
            self.free.remove(&s);
            if s < a {
                self.free.insert(s, a);
            }
            if e > b {
                self.free.insert(b, e);
            }
        }
    }

    /// Whether `[a, b)` is all free.
    pub(crate) fn is_free(&self, a: u64, b: u64) -> bool {
        self.free
            .range(..=a)
            .next_back()
            .is_some_and(|(&s, &e)| s <= a && e >= b)
    }

    /// Gives `[a, b)` back to the free ranges, merged with its neighbours.
    pub(crate) fn release(&mut self, a: u64, b: u64) {
        let (a, b) = (a.max(self.mmap_floor), b.min(self.mmap_top));
        if a >= b {
            return;
        }
        self.carve(a, b);
        let (mut s, mut e) = (a, b);
        if let Some((&ps, &pe)) = self.free.range(..s).next_back() {
            if pe == s {
                self.free.remove(&ps);
                s = ps;
            }
        }
        if let Some(&ne) = self.free.get(&e) {
            self.free.remove(&e);
            e = ne;
        }
        self.free.insert(s, e);
    }

    // ------------------------------------------------------------ fork and the end

    /// A copy of this space for a child: the same frames, those a write could change read-only
    /// in both, to be copied when one writes. None when there are no frames for it.
    pub(crate) fn fork(&mut self, f: &mut Frames) -> Option<Mm> {
        let mut child = Mm {
            root: f.alloc()?,
            asid: 0,
            gen: u64::MAX,
            stale: true,
            flush: Vec::new(),
            vmas: self.vmas.clone(),
            brk_start: self.brk_start,
            brk: self.brk,
            mmap_floor: self.mmap_floor,
            mmap_top: self.mmap_top,
            free: self.free.clone(),
            per_file: self.per_file.clone(),
            refs: self.per_file.keys().map(|&i| (i, true)).collect(),
        };
        // What is shared has all its pages first, for both sides to see the same ones (a PROT_NONE
        // one, a reservation, has none to share and costs nothing; a shared file's untouched pages
        // come from Process::file_pages to whichever side touches them)
        let shared: Vec<(u64, u64)> = self
            .vmas
            .iter()
            .filter(|(_, v)| v.shared && v.prot != 0 && v.file.is_none())
            .map(|(&s, v)| (s, v.end))
            .collect();
        for (s, e) in shared {
            let have: HashSet<u64> = self.leaves(f, s, e).iter().map(|l| l.0).collect();
            let mut at = s;
            while at < e {
                if !have.contains(&at) {
                    let ok = f.alloc().is_some_and(|pa| {
                        let ok = self.set(f, at, pa | bits(PROT_READ | PROT_WRITE, true));
                        if !ok {
                            f.decref(pa);
                        }
                        ok
                    });
                    if !ok {
                        child.refs.clear();
                        child.per_file.clear();
                        child.destroy(f);
                        return None;
                    }
                }
                at += PAGE;
            }
        }
        for (&s, v) in &self.vmas {
            for (va, slot, mut d) in self.leaves(f, s, v.end) {
                if !v.shared && writable(d) {
                    d |= AP_RO;
                    f.put(slot, d);
                }
                f.incref(d & ADDR);
                if !child.set(f, va, d) {
                    f.decref(d & ADDR);
                    child.refs.clear();
                    child.per_file.clear();
                    child.destroy(f);
                    return None;
                }
            }
        }
        self.stale = true;
        Some(child)
    }

    /// Gives everything back: the pages' frames (counted down), the tables, the files.
    pub(crate) fn destroy(&mut self, f: &mut Frames) {
        for (_, _, d) in self.leaves(f, 0, VA_END) {
            f.decref(d & ADDR);
        }
        for i in 0..512 {
            let d1 = f.get(self.root + 8 * i);
            if d1 & 1 == 0 {
                continue;
            }
            let t2 = d1 & ADDR;
            for j in 0..512 {
                let d2 = f.get(t2 + 8 * j);
                if d2 & 1 != 0 {
                    f.decref(d2 & ADDR);
                }
            }
            f.decref(t2);
        }
        f.decref(self.root);
        for &ino in self.per_file.keys() {
            self.refs.push((ino, false));
        }
        self.per_file.clear();
        self.vmas.clear();
        f.reap();
    }

    pub(crate) fn take_refs(&mut self) -> Vec<(Ino, bool)> {
        std::mem::take(&mut self.refs)
    }

    pub(crate) fn lazy_left(&self) -> u64 {
        self.per_file.values().sum()
    }

    pub(crate) fn save(&self, w: &mut W) {
        for v in [
            self.root,
            self.brk_start,
            self.brk,
            self.mmap_floor,
            self.mmap_top,
        ] {
            w.u(v);
        }
        w.u(self.vmas.len() as u64);
        for (&s, v) in &self.vmas {
            w.u(s);
            w.u(v.end);
            w.u(v.prot as u64 | (v.shared as u64) << 8 | (v.file.is_some() as u64) << 9);
            if let Some(b) = v.file {
                w.u(b.ino);
                w.u(b.off);
                w.u(b.data_end);
            }
        }
        w.u(self.free.len() as u64);
        for (&s, &e) in &self.free {
            w.u(s);
            w.u(e);
        }
        w.u(self.per_file.len() as u64);
        for (&i, &n) in &self.per_file {
            w.u(i);
            w.u(n);
        }
    }

    pub(crate) fn load(r: &mut R, ram_size: u64) -> Result<Mm, String> {
        let bad = || "bad memory map".to_string();
        let (root, brk_start, brk, mmap_floor, mmap_top) = (r.u()?, r.u()?, r.u()?, r.u()?, r.u()?);
        let mut m = Mm {
            root,
            asid: 0,
            gen: u64::MAX,
            stale: true,
            flush: Vec::new(),
            vmas: BTreeMap::new(),
            brk_start,
            brk,
            mmap_floor,
            mmap_top,
            free: BTreeMap::new(),
            per_file: BTreeMap::new(),
            refs: Vec::new(),
        };
        if root >= ram_size || mmap_top > VA_END || mmap_floor > mmap_top {
            return Err(bad());
        }
        for _ in 0..r.u()? {
            let (s, end, flags) = (r.u()?, r.u()?, r.u()?);
            if s >= end || end > VA_END {
                return Err(bad());
            }
            let file = if flags & 1 << 9 != 0 {
                Some(Back {
                    ino: r.u()?,
                    off: r.u()?,
                    data_end: r.u()?,
                })
            } else {
                None
            };
            m.vmas.insert(
                s,
                Vma {
                    end,
                    prot: flags as u8 & 7,
                    shared: flags & 1 << 8 != 0,
                    file,
                },
            );
        }
        for _ in 0..r.u()? {
            let (s, e) = (r.u()?, r.u()?);
            if s >= e || e > VA_END {
                return Err(bad());
            }
            m.free.insert(s, e);
        }
        for _ in 0..r.u()? {
            m.per_file.insert(r.u()?, r.u()?);
        }
        Ok(m)
    }
}

// ---------------------------------------------------------------- the process's side

/// What an access asks of a page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Acc {
    Read,
    Write,
    Exec,
    /// The host's write, whatever the area's protection (it sets pages up before the program
    /// has them)
    Force,
}

/// Why a page could not be had.
pub(crate) enum Miss {
    /// Not mapped, or not allowed
    Segv,
    /// No frame left
    Oom,
    /// Blobs of the filesystem it needs, which haven't arrived
    Need(Vec<String>),
}

impl Process {
    /// Takes a reference to a file, or gives one back, for the areas that changed.
    pub(crate) fn settle(&self) {
        let refs = std::mem::take(&mut self.p.mm.borrow_mut().refs);
        self.settle_refs(refs);
    }

    pub(crate) fn settle_refs(&self, refs: Vec<(Ino, bool)>) {
        for (ino, add) in refs {
            self.fs(|fs| if add { fs.add_ref(ino) } else { fs.drop_ref(ino) });
        }
    }

    // ------------------------------------------------------------ the host's access

    /// The frame of the page at `va` for an access: the page faulted in, or its sharing broken.
    fn page_pa(&self, va: u64, acc: Acc) -> Result<u64, Miss> {
        let page = va & !(PAGE - 1);
        let (desc, area) = {
            let mm = self.p.mm.borrow();
            let f = self.frames.borrow();
            (mm.pte(&f, page), mm.find(page))
        };
        let Some((start, vma)) = area else {
            return Err(Miss::Segv);
        };
        let allowed = match acc {
            Acc::Read => vma.prot != 0,
            Acc::Write => vma.prot & PROT_WRITE != 0,
            Acc::Exec => vma.prot & PROT_EXEC != 0,
            Acc::Force => true,
        };
        if !allowed {
            return Err(Miss::Segv);
        }
        let pa = desc & ADDR;
        if desc & 1 == 0 {
            return self.populate(page, start, &vma);
        }
        let write = matches!(acc, Acc::Write | Acc::Force);
        if !write || writable(desc) {
            return Ok(pa);
        }
        if acc == Acc::Force && self.frames.borrow().count(pa) == 1 {
            return Ok(pa);
        }
        self.unshare(
            page,
            pa,
            &vma,
            acc == Acc::Force && vma.prot & PROT_WRITE == 0,
        )
    }

    /// A frame for the page at `page` of the area at `start`: its file's bytes, or zeros.
    fn populate(&self, page: u64, start: u64, vma: &Vma) -> Result<u64, Miss> {
        // A file mapped MAP_SHARED: the one frame every mapping of that page of it has (Postgres's
        // dynamic shared memory is files in pg_dynshmem that its processes map)
        let key = vma
            .file
            .filter(|_| vma.shared)
            .map(|b| (b.ino, b.off + (page - start)));
        if let Some(key) = key {
            let have = self.file_pages.borrow().get(&key).copied();
            if let Some(pa) = have {
                let counted = page < page_up(vma.file.unwrap().data_end);
                {
                    let mut mm = self.p.mm.borrow_mut();
                    let mut f = self.frames.borrow_mut();
                    if !mm.set(&mut f, page, pa | bits(vma.prot, true)) {
                        return Err(Miss::Oom);
                    }
                    f.incref(pa);
                    if counted {
                        mm.lazy_less(key.0, 1);
                    }
                }
                self.settle();
                return Ok(pa);
            }
        }
        let mut data = Vec::new();
        let mut counted = false;
        if let Some(b) = vma.file {
            let n = b.data_end.min(page + PAGE).saturating_sub(page);
            counted = page < page_up(b.data_end);
            if n > 0 {
                let off = b.off + (page - start);
                match self.fs(|fs| got(fs.read(b.ino, off, n as usize))) {
                    Some(Got::Ok(d)) => data = d,
                    Some(Got::Need(blobs)) => return Err(Miss::Need(blobs)),
                    // A file that fails or is gone reads as zeros
                    Some(Got::Err(_)) | None => {},
                }
            }
        }
        let pa = {
            let mut mm = self.p.mm.borrow_mut();
            let mut f = self.frames.borrow_mut();
            let Some(pa) = f.alloc() else {
                return Err(Miss::Oom);
            };
            if !data.is_empty() {
                self.ram.write(pa as usize, &data);
            }
            if !mm.set(&mut f, page, pa | bits(vma.prot, true)) {
                f.decref(pa);
                return Err(Miss::Oom);
            }
            if counted {
                mm.lazy_less(vma.file.unwrap().ino, 1);
                self.filled.set(self.filled.get() + 1);
            }
            if let Some(key) = key {
                // The table's own hold on it, for the next mapping of this page
                f.incref(pa);
                self.file_pages.borrow_mut().insert(key, pa);
            }
            pa
        };
        self.settle();
        Ok(pa)
    }

    /// The shared file pages no mapping holds any more (the table's is their last reference)
    /// written back to their files, as far as each file goes, and let go: after an unmap, an exec
    /// or a process's end. What was written through a MAP_SHARED mapping is then in the file, for
    /// the next mapping of it and for read.
    pub(crate) fn trim_file_pages(&self) {
        if self.file_pages.borrow().is_empty() {
            return;
        }
        let unmapped: Vec<((Ino, u64), u64)> = {
            let f = self.frames.borrow();
            self.file_pages
                .borrow()
                .iter()
                .filter(|(_, &pa)| f.count(pa) == 1)
                .map(|(&k, &pa)| (k, pa))
                .collect()
        };
        let now = self.now_secs();
        for ((ino, off), pa) in unmapped {
            let mut page = vec![0u8; PAGE as usize];
            self.ram.read(pa as usize, &mut page);
            self.fs(|fs| {
                let size = fs.get(ino).map_or(0, |i| i.size);
                let n = size.saturating_sub(off).min(PAGE) as usize;
                if n > 0 {
                    let _ = fs.write(ino, off, &page[..n], now);
                }
            });
            self.file_pages.borrow_mut().remove(&(ino, off));
            self.frames.borrow_mut().decref(pa);
        }
        self.frames.borrow_mut().reap();
    }

    /// The write to the page at `page` (in the frame `pa`, which others may map): it gets a frame
    /// of its own, or the one it has made writable if nobody else maps it. `ro` keeps it read-only
    /// (a host write to a read-only page).
    fn unshare(&self, page: u64, pa: u64, vma: &Vma, ro: bool) -> Result<u64, Miss> {
        let mut mm = self.p.mm.borrow_mut();
        let mut f = self.frames.borrow_mut();
        let Some(slot) = mm.slot(&f, page) else {
            return Err(Miss::Segv);
        };
        let d = f.get(slot);
        let keep = if ro || vma.prot & PROT_WRITE == 0 { d } else { d & !AP_RO };
        let out = if f.count(pa) == 1 {
            f.put(slot, keep);
            pa
        } else {
            let Some(new) = f.alloc() else {
                return Err(Miss::Oom);
            };
            f.copy(pa, new);
            f.put(slot, (keep & !ADDR) | new);
            f.decref(pa);
            new
        };
        mm.flush.push(page >> 12);
        Ok(out)
    }

    fn host_pa(&self, va: u64, acc: Acc) -> Option<u64> {
        match self.page_pa(va, acc) {
            Ok(pa) => Some(pa + va % PAGE),
            Err(Miss::Need(b)) => {
                self.need.borrow_mut().extend(b);
                None
            },
            Err(_) => None,
        }
    }

    /// Whether `addr..addr + len` is inside the address space (its pages may not be there)
    fn in_space(addr: u64, len: usize) -> bool {
        addr.checked_add(len as u64)
            .is_some_and(|end| end <= VA_END)
    }

    pub(crate) fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        if !Process::in_space(addr, buf.len()) {
            return false;
        }
        let mut done = 0;
        while done < buf.len() {
            let va = addr + done as u64;
            let n = ((PAGE - va % PAGE) as usize).min(buf.len() - done);
            let Some(pa) = self.host_pa(va, Acc::Read) else {
                return false;
            };
            if !self.ram.read(pa as usize, &mut buf[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    pub(crate) fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.write_as(addr, data, Acc::Write)
    }

    /// Writes where the program has no right to, for setting its memory up.
    pub(crate) fn write_any(&self, addr: u64, data: &[u8]) -> bool {
        self.write_as(addr, data, Acc::Force)
    }

    fn write_as(&self, addr: u64, data: &[u8], acc: Acc) -> bool {
        if !Process::in_space(addr, data.len()) {
            return false;
        }
        let mut done = 0;
        while done < data.len() {
            let va = addr + done as u64;
            let n = ((PAGE - va % PAGE) as usize).min(data.len() - done);
            let Some(pa) = self.host_pa(va, acc) else {
                return false;
            };
            if !self.ram.write(pa as usize, &data[done..done + n]) {
                return false;
            }
            self.frames.borrow().purge(pa);
            done += n;
        }
        true
    }

    /// Whether the host may write `len` bytes at `addr` of the program's memory: its pages there
    /// are had first (faulted in, their sharing broken). Not if filling them needs blobs (the
    /// system call then waits for them and runs again, see `syscall_lazily`).
    pub(crate) fn touch(&self, addr: u64, len: u64) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len).filter(|&e| e <= VA_END) else {
            return false;
        };
        let mut at = addr & !(PAGE - 1);
        while at < end {
            if self.host_pa(at, Acc::Write).is_none() {
                return false;
            }
            at += PAGE;
        }
        true
    }

    /// The pages of file contents of `a..b` read now (ARMLESS_USER_EAGER, or for a comparison).
    fn read_pages(&self, a: u64, b: u64) -> Result<(), Miss> {
        let mut at = a;
        while at < b {
            self.page_pa(at, Acc::Read)?;
            at += PAGE;
        }
        Ok(())
    }

    /// Lazy pages left, and pages filled so far.
    pub fn lazy_pages(&self) -> (u64, u64) {
        (self.p.mm.borrow().lazy_left(), self.filled.get())
    }

    // ------------------------------------------------------------ the CPU's side

    /// The CPU translates with this process's tables (and its ASID): set on (and flushed, where
    /// what the process mapped changed).
    pub(crate) fn sync_mmu<H: Host>(&self, cpu: &mut Cpu<H>) {
        let mut mm = self.p.mm.borrow_mut();
        let (mut next, mut gen) = self.asids.get();
        let smp = self.machine.ncpus > 1;
        if mm.gen != gen {
            // Out of ASIDs: new ones for everything, and nothing of the old is in the TLB (any CPU's)
            if next > 0xffff {
                next = 1;
                gen += 1;
                if smp {
                    cpu.tlb_shootdown(&[], true);
                } else {
                    cpu.tlb_flush();
                }
            }
            mm.asid = next as u16;
            mm.gen = gen;
            self.asids.set((next + 1, gen));
        }
        let ttbr = mm.root | ((mm.asid as u64) << 48);
        if cpu.sys.ttbr0 != ttbr || cpu.sys.tcr != TCR || cpu.sys.sctlr & 1 == 0 {
            cpu.sys.tcr = TCR;
            cpu.sys.mair = MAIR;
            cpu.sys.ttbr0 = ttbr;
            cpu.sys.sctlr |= 1;
            cpu.ttbr_changed();
        }
        if smp {
            // Other CPUs may run threads of this address space: what they hold of what changed
            // goes too, before the call that changed it returns (and before their next
            // instruction)
            let all = std::mem::take(&mut mm.stale);
            if all || !mm.flush.is_empty() {
                let pages = std::mem::take(&mut mm.flush);
                cpu.tlb_shootdown(&pages, all);
            }
        } else if std::mem::take(&mut mm.stale) {
            mm.flush.clear();
            cpu.tlb_flush();
        } else {
            for p in mm.flush.drain(..) {
                cpu.tlb_flush_page(p);
            }
        }
    }

    /// A system call (`syscall`), with the memory it writes had first where it is not there
    /// yet: when that needs blobs, the call waits for them and runs again.
    pub(crate) fn syscall_lazily<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize) -> Ret {
        self.need.borrow_mut().clear();
        // A read has its buffer first, so it doesn't take what it then can't store (its first
        // 64 KiB: a count far past what a read gives must not have what follows the buffer; past
        // that, the read has what it stores as any access does)
        let (nr, a1, a2) = (cpu.x[8], cpu.x[1], cpu.x[2]);
        let bufs = match nr {
            63 | 67 | 207 => vec![(a1, a2)],
            65 | 69 => self.iovecs(a1, a2).unwrap_or_default(),
            // recvmsg: the msghdr's iovec array
            212 => match (self.u64_at(a1 + 16), self.u64_at(a1 + 24)) {
                (Some(iov), Some(n)) => self.iovecs(iov, n).unwrap_or_default(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        for (at, len) in bufs {
            if !self.touch(at, len.min(64 << 10)) {
                let need = std::mem::take(&mut *self.need.borrow_mut());
                if !need.is_empty() {
                    return Ret::Need(need);
                }
            }
        }
        let ret = self.syscall(cpu, cur);
        self.sync_mmu(cpu);
        let need = std::mem::take(&mut *self.need.borrow_mut());
        match ret {
            Ret::Val(_) if !need.is_empty() => Ret::Need(need),
            r => r,
        }
    }

    /// The CPU stopped at an exception that is not a system call (`esr`, at `elr`, for the
    /// address `far`): a missing page is had, or a shared one copied, and the instruction runs
    /// again (true); or its thread waits for the page's blobs, or the program faults (false: the
    /// next thread runs).
    pub(crate) fn abort<H: Host>(
        &mut self,
        cpu: &mut Cpu<H>,
        esr: u64,
        elr: u64,
        far: u64,
    ) -> bool {
        cpu.pc = elr;
        let (ec, fsc) = (esr >> 26, esr & 0x3f);
        // An instruction or data abort from EL0, a translation or a permission fault
        let acc = match ec {
            0x20 => Some(Acc::Exec),
            0x24 if esr & (1 << 6) != 0 => Some(Acc::Write),
            0x24 => Some(Acc::Read),
            _ => None,
        };
        let res = match acc {
            Some(acc) if matches!(fsc >> 2, 1 | 3) => self.page_pa(far, acc).map(|_| ()),
            _ => Err(Miss::Segv),
        };
        match res {
            Ok(()) => {
                self.p.mm.borrow_mut().flush.push(far >> 12);
                self.sync_mmu(cpu);
                true
            },
            Err(Miss::Need(blobs)) => {
                self.want_blobs(&blobs);
                self.park(cpu);
                let cur = self.cur;
                self.threads[cur].wait = Some((Wait::Blobs(blobs), None));
                self.end_slice();
                false
            },
            Err(Miss::Oom) => {
                self.crash(format!("out of memory at pc {elr:#x}, far {far:#x}"));
                false
            },
            Err(Miss::Segv) => {
                // A read of an ID register, which EL0 can't make: Linux answers it for the
                // program (HWCAP_CPUID), and so does this (Go's x/sys/cpu reads
                // ID_AA64ISAR0_EL1 when it starts)
                if matches!(ec, 0 | 0x18) && self.emulate_id_read(cpu, elr) {
                    return true;
                }
                // An undefined instruction (EC 0) is SIGILL: a program probing for a CPU
                // feature (OpenSSL's, for SVE or SM3) sets a handler and goes on without it
                if ec == 0 && self.raise_fault(SIGILL, ILL_ILLOPC) {
                    return false;
                }
                self.crash(format!(
                    "exception class {ec:#x} (esr {esr:#x}) at pc {elr:#x}, far {far:#x}"
                ));
                false
            },
        }
    }

    /// The instruction at `elr` is `mrs Xt, <ID register>` (op0 3, op1 0, CRn 0: MIDR_EL1,
    /// MPIDR_EL1, the ID_AA64* registers): Xt gets the CPU's value (0 for an ID register it
    /// doesn't have, as Linux's emulation gives) and the program goes on after it.
    fn emulate_id_read<H: Host>(&mut self, cpu: &mut Cpu<H>, elr: u64) -> bool {
        let mut w = [0u8; 4];
        if !self.read(elr, &mut w) {
            return false;
        }
        let insn = u32::from_le_bytes(w);
        // MRS (L = 1), op0 3, op1 0, CRn 0
        let (op1, crn) = ((insn >> 16) & 7, (insn >> 12) & 0xf);
        if insn & 0xfff8_0000 != 0xd538_0000 || op1 != 0 || crn != 0 {
            return false;
        }
        let value = cpu.read_sysreg((insn >> 5) & 0xffff).unwrap_or(0);
        let rt = (insn & 31) as usize;
        if rt != 31 {
            cpu.x[rt] = value;
        }
        cpu.pc = elr + 4;
        true
    }

    // ------------------------------------------------------------ areas

    /// `a..b` mapped as `prot` (`shared`: MAP_SHARED anonymous), over what was there; its pages
    /// from `file` if given, or zeros.
    pub(crate) fn map_area(&mut self, a: u64, b: u64, prot: u8, shared: bool, file: Option<Back>) {
        {
            let mut mm = self.p.mm.borrow_mut();
            let mut f = self.frames.borrow_mut();
            mm.unmap(&mut f, a, b);
            mm.carve(a, b);
            mm.insert(
                a,
                b,
                Vma {
                    end: b,
                    prot,
                    shared,
                    file,
                },
            );
        }
        self.settle();
    }

    /// A free range of `len` bytes of the mmap area mapped as `prot` anonymously (for the
    /// kernel's own pages: a signal's trampoline, an alternate stack).
    pub(crate) fn alloc_map(&mut self, len: u64, prot: u8) -> Option<u64> {
        let at = self.p.mm.borrow_mut().alloc(len)?;
        self.map_area(at, at + len, prot, false, None);
        Some(at)
    }

    /// What the futex at `addr` is, for waits and wakes to meet. A word of a shared area (of a
    /// call without FUTEX_PRIVATE_FLAG) is its frame's, so processes that map it meet there
    /// (Postgres's semaphores are in its shared memory); anything else is this process's own
    /// address. (Bit 63, `SHARED_FUTEX`, tells them apart: no address reaches it.)
    pub(crate) fn futex_key(&self, addr: u64, private: bool) -> u64 {
        if !private {
            let mm = self.p.mm.borrow();
            if mm.find(addr).is_some_and(|(_, v)| v.shared) {
                let d = mm.pte(&self.frames.borrow(), addr & !(PAGE - 1));
                if d & 1 != 0 {
                    return (d & ADDR) | (addr & (PAGE - 1)) | SHARED_FUTEX;
                }
            }
        }
        addr
    }

    // ------------------------------------------------------------ system calls

    pub(crate) fn sys_brk(&mut self, to: u64) -> i64 {
        let mut mm = self.p.mm.borrow_mut();
        let mut f = self.frames.borrow_mut();
        let (s, old) = (mm.brk_start, mm.brk);
        if to >= s && to <= s + crate::BRK_SIZE && to != old {
            let (was, now) = (page_up(old), page_up(to));
            if now < was {
                mm.unmap(&mut f, now, was);
            } else if now > was {
                mm.insert(
                    was,
                    now,
                    Vma {
                        end: now,
                        prot: PROT_READ | PROT_WRITE,
                        shared: false,
                        file: None,
                    },
                );
            }
            mm.brk = to;
        }
        mm.brk as i64
    }

    pub(crate) fn sys_mmap(
        &mut self,
        addr: u64,
        len: u64,
        prot: u64,
        flags: u64,
        fd: i32,
        off: u64,
    ) -> Ret {
        let v = Ret::Val;
        if len == 0 {
            return v(-EINVAL);
        }
        if len > VA_END {
            return v(-ENOMEM);
        }
        let len = page_up(len);
        let anon = flags & 0x20 != 0;
        let fixed = flags & (0x10 | 0x10_0000) != 0;
        if fixed
            && (!addr.is_multiple_of(PAGE)
                || addr < PAGE
                || addr.checked_add(len).is_none_or(|e| e > VA_END))
        {
            return v(-EINVAL);
        }
        if !anon && !off.is_multiple_of(PAGE) {
            return v(-EINVAL);
        }
        // A file mapping's pages are read as they are touched. A MAP_PRIVATE one is a copy of its
        // own; a MAP_SHARED one's pages are one frame each for every mapping of them (its writes
        // are seen by the others, but not written back to the file)
        let mut file = None;
        if !anon {
            let Some(Desc { kind, .. }) = self.files.get(fd).cloned() else {
                return v(-EBADF);
            };
            match kind {
                Kind::File { ino, .. } => {
                    let size = self.fs(|fs| fs.get(ino).map_or(0, |i| i.size)).unwrap_or(0);
                    file = Some((ino, size.saturating_sub(off).min(len)));
                },
                Kind::Zero => {},
                _ => return v(-ENODEV),
            }
        }
        let at = if fixed {
            if flags & 0x10_0000 != 0 && self.p.mm.borrow().overlaps(addr, addr + len) {
                return v(-EEXIST);
            }
            addr
        } else {
            // Hints are ignored: the highest free range that fits
            match self.p.mm.borrow_mut().alloc(len) {
                Some(at) => at,
                None => return v(-ENOMEM),
            }
        };
        let back = file.map(|(ino, data)| Back {
            ino,
            off,
            data_end: at + data,
        });
        // MAP_SHARED (and MAP_SHARED_VALIDATE): an anonymous area's pages are shared with the
        // processes forked from this one, a file's with every mapping of it (file_pages)
        let shared = flags & 1 != 0;
        self.map_area(at, at + len, prot as u8 & 7, shared, back);
        if back.is_some() && eager() {
            // A failure here leaves the pages lazy
            let _ = self.read_pages(at, at + len);
        }
        v(at as i64)
    }

    pub(crate) fn sys_munmap(&mut self, addr: u64, len: u64) -> i64 {
        if len == 0 {
            return -EINVAL;
        }
        let a = addr & !(PAGE - 1);
        let b = page_up(addr.saturating_add(len).min(VA_END));
        if a >= b {
            return 0;
        }
        {
            let mut mm = self.p.mm.borrow_mut();
            let mut f = self.frames.borrow_mut();
            mm.unmap(&mut f, a, b);
            mm.release(a, b);
        }
        self.settle();
        self.trim_file_pages();
        0
    }

    pub(crate) fn sys_mprotect(&mut self, addr: u64, len: u64, prot: u64) -> i64 {
        if !addr.is_multiple_of(PAGE) || addr.saturating_add(len) > VA_END {
            return -EINVAL;
        }
        let b = page_up(addr + len);
        let mut mm = self.p.mm.borrow_mut();
        if !mm.covered(addr, b) {
            return -ENOMEM;
        }
        if addr < b {
            mm.protect(&mut self.frames.borrow_mut(), addr, b, prot as u8 & 7);
        }
        0
    }

    /// madvise: DONTNEED and FREE give the pages of anonymous memory back.
    pub(crate) fn sys_madvise(&mut self, addr: u64, len: u64, advice: u64) -> i64 {
        if advice == 4 || advice == 8 {
            let a = addr & !(PAGE - 1);
            let b = page_up(addr.saturating_add(len).min(VA_END));
            self.p
                .mm
                .borrow_mut()
                .discard(&mut self.frames.borrow_mut(), a, b);
        }
        0
    }

    pub(crate) fn sys_mremap(&mut self, old: u64, old_len: u64, new_len: u64, flags: u64) -> i64 {
        if !old.is_multiple_of(PAGE)
            || old_len == 0
            || new_len == 0
            || new_len > VA_END
            || old >= VA_END
            || old_len > VA_END
        {
            return -EINVAL;
        }
        let mut mm = self.p.mm.borrow_mut();
        let mut f = self.frames.borrow_mut();
        if !mm.covered(old, old + old_len) {
            return -EFAULT;
        }
        if new_len <= old_len {
            mm.unmap(&mut f, old + new_len, old + old_len);
            mm.release(old + new_len, old + old_len);
            drop((mm, f));
            self.settle();
            return old as i64;
        }
        // The pages after are free: it grows where it is, the new ones zeros
        let prot = mm.find(old).map_or(PROT_READ | PROT_WRITE, |(_, v)| v.prot);
        if mm.is_free(old + old_len, old + new_len) {
            mm.carve(old + old_len, old + new_len);
            mm.insert(
                old + old_len,
                old + new_len,
                Vma {
                    end: old + new_len,
                    prot,
                    shared: false,
                    file: None,
                },
            );
            return old as i64;
        }
        if flags & 1 == 0 {
            return -ENOMEM; // not MREMAP_MAYMOVE
        }
        let Some(at) = mm.alloc(new_len) else {
            return -ENOMEM;
        };
        if !mm.relocate(&mut f, old, old_len, at) {
            mm.release(at, at + new_len);
            return -ENOMEM;
        }
        mm.release(old, old + old_len);
        mm.insert(
            at + old_len,
            at + new_len,
            Vma {
                end: at + new_len,
                prot,
                shared: false,
                file: None,
            },
        );
        at as i64
    }
}

// ---------------------------------------------------------------- executables

/// How much of an executable exec reads up front to load it lazily: usually its headers and its
/// loader's path are in there (when they are not, they are read where they are).
const HEAD: usize = 64 << 10;

/// si_code of SIGILL: an illegal opcode
const ILL_ILLOPC: i32 = 1;

/// An executable to load: its bytes, or a file of the filesystem (and its first bytes) whose
/// segments are mapped lazily.
pub(crate) enum Image<'a> {
    Bytes(std::borrow::Cow<'a, [u8]>),
    File(Ino, Vec<u8>),
}

impl Image<'_> {
    pub(crate) fn head(&self) -> &[u8] {
        match self {
            Image::Bytes(b) => b,
            Image::File(_, h) => h,
        }
    }

    /// `len` bytes of the image from `off`: from its first bytes when they are there, else read
    /// from the file (a program header table or a loader's path can be anywhere in it).
    pub(crate) fn read(
        &self,
        board: Option<&armless::Board>,
        off: u64,
        len: usize,
    ) -> Result<std::borrow::Cow<'_, [u8]>, ExecError> {
        let truncated = || ExecError::Fail("truncated ELF".into());
        let head = self.head();
        let end = off.checked_add(len as u64).ok_or_else(truncated)?;
        if end <= head.len() as u64 {
            return Ok(std::borrow::Cow::Borrowed(
                &head[off as usize..end as usize],
            ));
        }
        let (Image::File(ino, _), Some(board)) = (self, board) else {
            return Err(truncated());
        };
        match board.with_fs(|fs| got(fs.read(*ino, off, len))) {
            Some(Got::Ok(d)) if d.len() == len => Ok(std::borrow::Cow::Owned(d)),
            Some(Got::Ok(_)) => Err(truncated()),
            Some(Got::Need(blobs)) => {
                board.want_blobs(&blobs);
                Err(ExecError::Need(blobs))
            },
            Some(Got::Err(e)) => Err(ExecError::Fail(format!("reading it: errno {}", -e))),
            None => Err(ExecError::Fail("the machine has no filesystem".into())),
        }
    }

    /// The executable `ino` of the machine's filesystem: its first bytes (its pages are had as
    /// they are touched), else all of it (its blobs asked for when they aren't there).
    pub(crate) fn of(board: &armless::Board, ino: Ino) -> Result<Image<'static>, ExecError> {
        if eager() {
            return crate::read_all(board, ino).map(|b| Image::Bytes(b.into()));
        }
        match board.with_fs(|fs| got(fs.read(ino, 0, HEAD))) {
            Some(Got::Ok(h)) => Ok(Image::File(ino, h)),
            Some(Got::Need(blobs)) => {
                board.want_blobs(&blobs);
                Err(ExecError::Need(blobs))
            },
            Some(Got::Err(e)) => Err(ExecError::Fail(format!("reading it: errno {}", -e))),
            None => Err(ExecError::Fail("the machine has no filesystem".into())),
        }
    }
}

impl Process {
    /// The PT_LOAD segments `segs` of `image` in memory: copied, or mapped from its file (read
    /// at once where they can't be: a segment not page-aligned in its file).
    pub(crate) fn place(&mut self, image: &Image, segs: &[Seg]) -> Result<(), ExecError> {
        for s in segs {
            let a = s.at & !(PAGE - 1);
            let lead = s.at - a;
            let end = page_up(s.at + s.memsz);
            // A page the segment shares with the one before it (text then data in one page) is
            // written into, not mapped over: the file path is for segments with pages of their own
            let shared_head = self.p.mm.borrow().find(a).is_some();
            if let &Image::File(ino, _) = image {
                if s.filesz > 0 && s.off % PAGE == lead && !shared_head {
                    let file_end = page_up(s.at + s.filesz);
                    let back = Back {
                        ino,
                        off: s.off - lead,
                        data_end: s.at + s.filesz,
                    };
                    self.map_area(a, file_end, s.prot, false, Some(back));
                    if end > file_end {
                        self.map_area(file_end, end, s.prot, false, None);
                    }
                    continue;
                }
            }
            {
                let mut mm = self.p.mm.borrow_mut();
                mm.ensure(&mut self.frames.borrow_mut(), a, end, s.prot);
            }
            let data = match image {
                Image::Bytes(b) => b
                    .get(s.off as usize..(s.off + s.filesz) as usize)
                    .ok_or_else(|| ExecError::Fail("truncated ELF".into()))?
                    .to_vec(),
                &Image::File(_, _) if s.filesz == 0 => Vec::new(),
                &Image::File(ino, _) => {
                    match self.fs(|fs| got(fs.read(ino, s.off, s.filesz as usize))) {
                        Some(Got::Ok(d)) => d,
                        Some(Got::Need(blobs)) => {
                            self.want_blobs(&blobs);
                            return Err(ExecError::Need(blobs));
                        },
                        Some(Got::Err(e)) => {
                            return Err(ExecError::Fail(format!("reading it: errno {}", -e)))
                        },
                        None => return Err(ExecError::Fail("no filesystem".into())),
                    }
                },
            };
            if !data.is_empty() && !self.write_any(s.at, &data) {
                return Err(ExecError::Fail("no memory for the program".into()));
            }
        }
        if eager() {
            let files: Vec<(u64, u64)> = {
                let mm = self.p.mm.borrow();
                mm.vmas
                    .iter()
                    .filter_map(|(&s, v)| v.file.map(|b| (s, page_up(b.data_end).min(v.end))))
                    .collect()
            };
            for (s, e) in files {
                if let Err(Miss::Need(blobs)) = self.read_pages(s, e) {
                    self.want_blobs(&blobs);
                    return Err(ExecError::Need(blobs));
                }
            }
        }
        Ok(())
    }
}

/// An address space, shared by the processes that have it (vfork, CLONE_VM). Every use is under
/// the lock that the CPUs of a machine take to answer their programs (`Process`'s), so a borrow
/// never waits: one that would is a borrow of the same space twice, which panics as a
/// `RefCell`'s would.
#[derive(Clone)]
pub(crate) struct SharedMm(std::sync::Arc<std::sync::Mutex<Mm>>);

impl SharedMm {
    pub(crate) fn new(mm: Mm) -> SharedMm {
        SharedMm(std::sync::Arc::new(std::sync::Mutex::new(mm)))
    }

    pub(crate) fn borrow(&self) -> std::sync::MutexGuard<'_, Mm> {
        self.0.try_lock().expect("address space already borrowed")
    }

    pub(crate) fn borrow_mut(&self) -> std::sync::MutexGuard<'_, Mm> {
        self.borrow()
    }

    pub(crate) fn ptr_eq(a: &SharedMm, b: &SharedMm) -> bool {
        std::sync::Arc::ptr_eq(&a.0, &b.0)
    }

    pub(crate) fn strong_count(m: &SharedMm) -> usize {
        std::sync::Arc::strong_count(&m.0)
    }
}
