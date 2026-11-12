//! Files mapped lazily. A page of a file mapping (an mmap of a file, and the segments of the
//! executable and of its loader) gets its contents when something first touches it, so a snapshot
//! carries only the pages the program used, and the filesystem fetches only the blobs of those.
//!
//! How: once a process has a lazy mapping, its CPU translates with page tables the process keeps
//! in the program's RAM, still VA = PA: 2 MiB blocks for all of RAM, and a table of 4 KiB pages
//! only for a block that holds lazy pages, those left invalid. A touch of one is a translation
//! fault, which stops the CPU (`stop_on_exceptions`): the page is filled from its file (or its
//! thread waits for the blobs that needs, and touches it again once they arrived), made valid, and
//! the instruction runs again. What the host reads or writes of the program's memory for a system
//! call (a write(2) from a mapped page) is filled first the same way. A block whose pages are all
//! filled is a block again.
//!
//! Making a page valid needs no TLB maintenance: a fault is not cached. Making pages lazy (a new
//! mapping over them) flushes the CPU's TLB after the call.
//!
//! The tables take one 2 MiB block of the mmap area, which the program cannot touch (it is not
//! mapped, and the host refuses it to system calls): its first page the top-level table, then one
//! table per GiB of RAM, the rest 4 KiB tables for the blocks that hold lazy pages. When those run
//! out, or the mmap area has no room for them, or RAM is not whole blocks, a file is read in at
//! once as before. ARMLESS_USER_EAGER=1 does that always (for comparisons).

use std::collections::BTreeMap;

use armless::cpu::Cpu;
use armless::dev::fs::Ino;
use armless::{Host, Ram};

use crate::elf::Seg;
use crate::files::{got, Got};
use crate::ser::{R, W};
use crate::signal::SIGILL;
use crate::{page_up, ExecError, Process, Ret, Wait, PAGE};

/// What one level-2 entry maps: the tables' unit, and the size of their area
pub(crate) const BLOCK: u64 = 2 << 20;
const GIB: u64 = 1 << 30;
/// Descriptor bits: valid; a table (above level 3) or a page (at it); and for memory: EL0 may read
/// and write it (AP[1]), inner shareable, accessed, attributes 0 of MAIR (normal, write-back).
const VALID: u64 = 1;
const TABLE: u64 = 2;
const MEM: u64 = (1 << 6) | (3 << 8) | (1 << 10);
/// TCR_EL1: a 39-bit space from level 1 (T0SZ 25), write-back walks, inner shareable, 4 KiB
/// granule, no walks from TTBR1 (EPD1), 40-bit physical addresses.
const TCR: u64 = 25 | (1 << 8) | (1 << 10) | (3 << 12) | (1 << 23) | (2 << 32);
const MAIR: u64 = 0xff;

/// Lazy pages `start..end` of a file: the page at `p` holds the file's bytes from
/// `off + (p - start)`, up to the address `data_end` (zeros past it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    end: u64,
    ino: Ino,
    off: u64,
    data_end: u64,
}

/// The page tables, in the 2 MiB of RAM at `base`.
struct Tables {
    base: u64,
    /// The blocks mapped by a table of pages: block number -> (the table, its lazy pages)
    l3: BTreeMap<u64, (u64, u64)>,
    /// Tables of the area not in use
    spare: Vec<u64>,
}

#[derive(Default)]
pub(crate) struct Lazy {
    runs: BTreeMap<u64, Run>,
    /// Lazy pages per file: the process holds a reference to a file while it has some, so a
    /// mapped file that is unlinked keeps its contents.
    per_file: BTreeMap<Ino, u64>,
    tables: Option<Tables>,
    /// The CPU's translation is to be set again (the tables are new or the process restored), or
    /// flushed (pages became lazy).
    stale: bool,
    /// Blobs a host access of a lazy page waits for: its system call runs again once they arrived.
    need: Vec<String>,
    /// Pages filled so far (for reports)
    pub filled: u64,
}

/// What a fault the CPU stopped at turned out to be.
pub(crate) enum Abort {
    /// A lazy page, filled: run the instruction again
    Filled,
    /// A lazy page whose blobs haven't arrived: its thread waits for them
    Need(Vec<String>),
    /// Not a lazy page: the program faults
    No,
}

fn get(ram: &Ram, at: u64) -> u64 {
    let mut b = [0; 8];
    ram.read(at as usize, &mut b);
    u64::from_le_bytes(b)
}

fn put(ram: &Ram, at: u64, v: u64) {
    ram.write(at as usize, &v.to_le_bytes());
}

fn eager() -> bool {
    std::env::var("ARMLESS_USER_EAGER").is_ok_and(|v| v == "1")
}

impl Tables {
    /// Tables of every block of RAM but their own, at `base` (zero, as the mmap area's free
    /// ranges are).
    fn new(ram: &Ram, ram_size: u64, base: u64) -> Tables {
        let l2s = ram_size.div_ceil(GIB);
        for i in 0..l2s {
            put(ram, base + 8 * i, (base + PAGE * (1 + i)) | VALID | TABLE);
        }
        let mut l2 = vec![0u8; (l2s * PAGE) as usize];
        for b in 0..ram_size / BLOCK {
            if b * BLOCK != base {
                let d = (b * BLOCK) | VALID | MEM;
                l2[b as usize * 8..][..8].copy_from_slice(&d.to_le_bytes());
            }
        }
        ram.write((base + PAGE) as usize, &l2);
        Tables {
            base,
            l3: BTreeMap::new(),
            spare: Tables::pool(ram_size, base).rev().collect(),
        }
    }

    /// The area's tables of pages
    fn pool(ram_size: u64, base: u64) -> impl DoubleEndedIterator<Item = u64> {
        (1 + ram_size.div_ceil(GIB)..BLOCK / PAGE).map(move |i| base + i * PAGE)
    }

    /// The level-2 entry of block `b` (the level-2 tables follow each other)
    fn l2_entry(&self, b: u64) -> u64 {
        self.base + PAGE + 8 * b
    }

    /// Pages `a..b` invalid. There are spare tables enough (`lazy_room`).
    fn make_lazy(&mut self, ram: &Ram, a: u64, b: u64) {
        for blk in a / BLOCK..b.div_ceil(BLOCK) {
            let (t, mut n) = match self.l3.get(&blk) {
                Some(&e) => e,
                None => {
                    let t = self.spare.pop().expect("lazy_room counted the tables");
                    let mut d = vec![0u8; PAGE as usize];
                    for (i, e) in d.chunks_mut(8).enumerate() {
                        let pa = blk * BLOCK + i as u64 * PAGE;
                        e.copy_from_slice(&(pa | VALID | TABLE | MEM).to_le_bytes());
                    }
                    ram.write(t as usize, &d);
                    put(ram, self.l2_entry(blk), t | VALID | TABLE);
                    (t, 0)
                },
            };
            let (s, e) = (a.max(blk * BLOCK), b.min((blk + 1) * BLOCK));
            for p in (s..e).step_by(PAGE as usize) {
                let at = t + (p % BLOCK) / PAGE * 8;
                if get(ram, at) != 0 {
                    put(ram, at, 0);
                    n += 1;
                }
            }
            self.l3.insert(blk, (t, n));
        }
    }

    /// Pages `a..b` valid again; a block with no lazy page left is a block again.
    fn make_valid(&mut self, ram: &Ram, a: u64, b: u64) {
        for blk in a / BLOCK..b.div_ceil(BLOCK) {
            let Some(&(t, mut n)) = self.l3.get(&blk) else {
                continue;
            };
            let (s, e) = (a.max(blk * BLOCK), b.min((blk + 1) * BLOCK));
            for p in (s..e).step_by(PAGE as usize) {
                let at = t + (p % BLOCK) / PAGE * 8;
                if get(ram, at) == 0 {
                    put(ram, at, p | VALID | TABLE | MEM);
                    n -= 1;
                }
            }
            if n == 0 {
                put(ram, self.l2_entry(blk), (blk * BLOCK) | VALID | MEM);
                ram.discard(t as usize, PAGE as usize);
                self.spare.push(t);
                self.l3.remove(&blk);
            } else {
                self.l3.insert(blk, (t, n));
            }
        }
    }
}

impl Lazy {
    /// Takes `a..b` out of the runs: the pieces taken, with their files.
    fn cut(&mut self, a: u64, b: u64) -> Vec<(u64, u64, Ino)> {
        let hit: Vec<(u64, Run)> = self
            .runs
            .range(..b)
            .rev()
            .take_while(|(_, r)| r.end > a)
            .map(|(&s, &r)| (s, r))
            .collect();
        let mut cut = Vec::new();
        for (s, r) in hit {
            self.runs.remove(&s);
            if s < a {
                self.runs.insert(s, Run { end: a, ..r });
            }
            if r.end > b {
                self.runs.insert(
                    b,
                    Run {
                        off: r.off + (b - s),
                        ..r
                    },
                );
            }
            cut.push((s.max(a), r.end.min(b), r.ino));
        }
        cut
    }

    /// The tables' 2 MiB, if there are tables
    fn area(&self) -> Option<(u64, u64)> {
        self.tables.as_ref().map(|t| (t.base, t.base + BLOCK))
    }

    pub(crate) fn save(&self, w: &mut W) {
        w.u(self.runs.len() as u64);
        for (&s, r) in &self.runs {
            for v in [s, r.end, r.ino, r.off, r.data_end] {
                w.u(v);
            }
        }
        match &self.tables {
            None => w.u(0),
            Some(t) => {
                w.u(1);
                w.u(t.base);
                w.u(t.l3.len() as u64);
                for (&blk, &(at, n)) in &t.l3 {
                    w.u(blk);
                    w.u(at);
                    w.u(n);
                }
            },
        }
        w.u(self.filled);
    }

    pub(crate) fn load(r: &mut R, ram_size: u64) -> Result<Lazy, String> {
        let mut l = Lazy::default();
        for _ in 0..r.u()? {
            let (s, end, ino, off, data_end) = (r.u()?, r.u()?, r.u()?, r.u()?, r.u()?);
            if s >= end || end > ram_size {
                return Err("bad lazy mapping".into());
            }
            *l.per_file.entry(ino).or_default() += (end - s) / PAGE;
            l.runs.insert(
                s,
                Run {
                    end,
                    ino,
                    off,
                    data_end,
                },
            );
        }
        if r.u()? != 0 {
            let base = r.u()?;
            let mut l3 = BTreeMap::new();
            for _ in 0..r.u()? {
                l3.insert(r.u()?, (r.u()?, r.u()?));
            }
            let used: Vec<u64> = l3.values().map(|&(t, _)| t).collect();
            let spare = Tables::pool(ram_size, base)
                .rev()
                .filter(|t| !used.contains(t))
                .collect();
            l.tables = Some(Tables { base, l3, spare });
            l.stale = true;
        }
        l.filled = r.u()?;
        Ok(l)
    }
}

impl Process {
    /// Whether the host may read or write `len` bytes at `addr` of the program's memory: its lazy
    /// pages there filled first. Not if they are the page tables', or if filling them needs blobs
    /// (the system call then waits for them and runs again, see `syscall_lazily`).
    pub(crate) fn touch(&self, addr: u64, len: u64) -> bool {
        if len == 0 {
            return true;
        }
        let end = addr.saturating_add(len);
        {
            let l = self.lazy.borrow();
            if let Some((s, e)) = l.area() {
                if addr < e && end > s {
                    return false;
                }
            }
            let a = addr & !(PAGE - 1);
            if l.runs
                .range(..end)
                .next_back()
                .is_none_or(|(_, r)| r.end <= a)
            {
                return true;
            }
        }
        match self.fill(addr & !(PAGE - 1), page_up(end)) {
            Ok(()) => true,
            Err(need) => {
                self.lazy.borrow_mut().need.extend(need);
                false
            },
        }
    }

    /// The lazy pages of `a..b` filled from their files and valid; the blobs that needs if any
    /// haven't arrived (the pages they hold are filled meanwhile).
    fn fill(&self, a: u64, b: u64) -> Result<(), Vec<String>> {
        let pieces: Vec<(u64, u64, u64, Run)> = {
            let l = self.lazy.borrow();
            let mut v: Vec<_> = l
                .runs
                .range(..b)
                .rev()
                .take_while(|(_, r)| r.end > a)
                .map(|(&s, &r)| (s.max(a), r.end.min(b), s, r))
                .collect();
            v.reverse();
            v
        };
        let mut need = Vec::new();
        for (ps, pe, s, r) in pieces {
            let n = r.data_end.min(pe).saturating_sub(ps);
            if n > 0 {
                let off = r.off + (ps - s);
                match self.fs(|fs| got(fs.read(r.ino, off, n as usize))) {
                    Some(Got::Ok(data)) => {
                        // Through the machine, which drops code decoded from those pages before
                        let wrote = self.board.as_ref().is_some_and(|b| b.io.write(ps, &data));
                        if !wrote {
                            self.ram.write(ps as usize, &data);
                        }
                    },
                    Some(Got::Need(blobs)) => {
                        need.extend(blobs);
                        continue;
                    },
                    // A file that fails or is gone reads as zeros
                    Some(Got::Err(_)) | None => {},
                }
            }
            self.lazy.borrow_mut().filled += (pe - ps) / PAGE;
            self.forget_lazy(ps, pe);
        }
        if need.is_empty() {
            Ok(())
        } else {
            need.sort();
            need.dedup();
            Err(need)
        }
    }

    /// `a..b` no longer lazy (unmapped, mapped over, or filled): valid, and what is in RAM there.
    pub(crate) fn forget_lazy(&self, a: u64, b: u64) {
        let mut l = self.lazy.borrow_mut();
        if l.runs.is_empty() {
            return;
        }
        let cut = l.cut(a, b);
        for (s, e, ino) in cut {
            if let Some(t) = l.tables.as_mut() {
                t.make_valid(&self.ram, s, e);
            }
            let left = l.per_file.entry(ino).or_default();
            *left = left.saturating_sub((e - s) / PAGE);
            if *left == 0 {
                l.per_file.remove(&ino);
                self.fs(|fs| fs.drop_ref(ino));
            }
        }
    }

    /// The page tables' area, which is never the program's.
    pub(crate) fn tables_area(&self) -> Option<(u64, u64)> {
        self.lazy.borrow().area()
    }

    /// Whether a mapping of `len` bytes can be lazy: the tables are made (now, if this is the
    /// first) and have room for it.
    pub(crate) fn lazy_room(&mut self, len: u64) -> bool {
        if self.board.is_none() || !self.ram_size.is_multiple_of(BLOCK) || eager() {
            return false;
        }
        if self.lazy.borrow().tables.is_none() {
            let Some(base) = self.alloc_block() else {
                return false;
            };
            let mut l = self.lazy.borrow_mut();
            l.tables = Some(Tables::new(&self.ram, self.ram_size, base));
            l.stale = true;
        }
        let l = self.lazy.borrow();
        let t = l.tables.as_ref().unwrap();
        // A range spans at most one block more than its size in blocks
        t.spare.len() as u64 > len.div_ceil(BLOCK)
    }

    /// A free 2 MiB block of the mmap area, the highest, taken out of it.
    fn alloc_block(&mut self) -> Option<u64> {
        let at = self.free.iter().rev().find_map(|(&s, &e)| {
            let at = e.checked_sub(BLOCK)? & !(BLOCK - 1);
            (at >= s).then_some(at)
        })?;
        self.carve(at, at + BLOCK);
        Some(at)
    }

    /// Maps `len` bytes at `at` (pages) to the file `ino` from `off` lazily, `data` of them its
    /// bytes and zeros past those. `lazy_room(len)` said yes.
    pub(crate) fn map_lazy(&mut self, at: u64, len: u64, ino: Ino, off: u64, data: u64) {
        let end = at + len;
        self.forget_lazy(at, end);
        self.zero_range(at, len);
        // Pages past the file's bytes are zeros: nothing to wait for
        let lazy_end = end.min(page_up(at + data));
        if lazy_end <= at {
            return;
        }
        let mut l = self.lazy.borrow_mut();
        if let Some(t) = l.tables.as_mut() {
            t.make_lazy(&self.ram, at, lazy_end);
        }
        l.runs.insert(
            at,
            Run {
                end: lazy_end,
                ino,
                off,
                data_end: at + data,
            },
        );
        let n = l.per_file.entry(ino).or_default();
        if *n == 0 {
            self.fs(|fs| fs.add_ref(ino));
        }
        *n += (lazy_end - at) / PAGE;
        l.stale = true;
    }

    /// The CPU's translation as the tables say: on (and flushed, when pages became lazy) once
    /// there are tables, else off as `setup_cpu` left it.
    pub(crate) fn sync_mmu<H: Host>(&self, cpu: &mut Cpu<H>) {
        let mut l = self.lazy.borrow_mut();
        if !std::mem::take(&mut l.stale) {
            return;
        }
        if let Some(t) = &l.tables {
            cpu.sys.tcr = TCR;
            cpu.sys.mair = MAIR;
            cpu.sys.ttbr0 = t.base;
            cpu.sys.sctlr |= 1;
            cpu.tlb_flush();
        }
    }

    /// A system call (`syscall`), with the memory it reads or writes filled first where it is
    /// mapped lazily: when that needs blobs, the call waits for them and runs again.
    pub(crate) fn syscall_lazily<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize) -> Ret {
        self.lazy.borrow_mut().need.clear();
        // A read fills its buffer beforehand, so it doesn't take what it then can't store (its
        // first 64 KiB: a count far past what a read gives must not fill whatever follows the
        // buffer; past that, the read fills what it stores as any access does)
        let (nr, a1, a2) = (cpu.x[8], cpu.x[1], cpu.x[2]);
        let bufs = match nr {
            63 | 67 => vec![(a1, a2)],
            65 | 69 => self.iovecs(a1, a2).unwrap_or_default(),
            _ => Vec::new(),
        };
        for (at, len) in bufs {
            if !self.touch(at, len.min(64 << 10)) {
                let need = std::mem::take(&mut self.lazy.borrow_mut().need);
                if !need.is_empty() {
                    return Ret::Need(need);
                }
            }
        }
        let ret = self.syscall(cpu, cur);
        self.sync_mmu(cpu);
        let need = std::mem::take(&mut self.lazy.borrow_mut().need);
        match ret {
            Ret::Val(_) if !need.is_empty() => Ret::Need(need),
            r => r,
        }
    }

    /// The CPU stopped at an exception that is not a system call (`esr`, at `elr`, for the
    /// address `far`): a lazy page is filled and the instruction runs again (true), or its thread
    /// waits for the page's blobs, or the program faults (false: the next thread runs).
    pub(crate) fn abort<H: Host>(
        &mut self,
        cpu: &mut Cpu<H>,
        esr: u64,
        elr: u64,
        far: u64,
    ) -> bool {
        cpu.pc = elr;
        match self.lazy_fault(esr, far) {
            Abort::Filled => true,
            Abort::Need(blobs) => {
                self.want_blobs(&blobs);
                self.park(cpu);
                let cur = self.cur;
                self.threads[cur].wait = Some((Wait::Blobs(blobs), None));
                self.cur = (cur + 1) % self.threads.len();
                false
            },
            Abort::No => {
                let ec = esr >> 26;
                // An undefined instruction (EC 0) is SIGILL: a program probing for a CPU
                // feature (OpenSSL's, for SVE or SM3) sets a handler and goes on without it
                if ec == 0 && self.raise_fault(SIGILL, ILL_ILLOPC) {
                    return false;
                }
                self.fault = Some(format!(
                    "exception class {ec:#x} (esr {esr:#x}) at pc {elr:#x}, far {far:#x}"
                ));
                false
            },
        }
    }

    fn lazy_fault(&mut self, esr: u64, far: u64) -> Abort {
        let (ec, fsc) = (esr >> 26, esr & 0x3f);
        // An instruction or data abort from EL0, a translation fault
        if !matches!(ec, 0x20 | 0x24) || fsc & 0x3c != 0x04 {
            return Abort::No;
        }
        let page = far & !(PAGE - 1);
        let lazy = {
            let l = self.lazy.borrow();
            l.runs
                .range(..=page)
                .next_back()
                .is_some_and(|(_, r)| r.end > page)
        };
        if !lazy {
            return Abort::No;
        }
        match self.fill(page, page + PAGE) {
            Ok(()) => Abort::Filled,
            Err(need) => Abort::Need(need),
        }
    }

    /// Lazy pages left, and pages filled so far.
    pub fn lazy_pages(&self) -> (u64, u64) {
        let l = self.lazy.borrow();
        (
            l.runs.iter().map(|(&s, r)| (r.end - s) / PAGE).sum(),
            l.filled,
        )
    }
}

/// How much of an executable exec reads to load it lazily: its headers and its loader's path
/// are in there.
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

    /// The executable `ino` of the machine's filesystem: its first bytes when its pages can be
    /// lazy on a machine of `ram_size`, else all of it (its blobs asked for when they aren't there).
    pub(crate) fn of(
        board: &armless::Board,
        ino: Ino,
        ram_size: u64,
    ) -> Result<Image<'static>, ExecError> {
        if !ram_size.is_multiple_of(BLOCK) || eager() {
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
    /// The PT_LOAD segments `segs` of `image` in memory: copied, or mapped lazily from its file
    /// (read at once where they can't be: no room for the tables, or a segment not page-aligned
    /// in its file).
    pub(crate) fn place(&mut self, image: &Image, segs: &[Seg]) -> Result<(), ExecError> {
        for s in segs {
            match image {
                Image::Bytes(b) => {
                    let data = b
                        .get(s.off as usize..(s.off + s.filesz) as usize)
                        .ok_or_else(|| ExecError::Fail("truncated ELF".into()))?;
                    self.ram.write(s.at as usize, data);
                },
                Image::File(_, _) if s.filesz == 0 => {},
                &Image::File(ino, _) => {
                    let a = s.at & !(PAGE - 1);
                    let lead = s.at - a;
                    let len = page_up(s.at + s.filesz) - a;
                    if s.off % PAGE == lead && self.lazy_room(len) {
                        self.map_lazy(a, len, ino, s.off - lead, lead + s.filesz);
                        continue;
                    }
                    match self.fs(|fs| got(fs.read(ino, s.off, s.filesz as usize))) {
                        Some(Got::Ok(d)) => {
                            self.ram.write(s.at as usize, &d);
                        },
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
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(end: u64, off: u64) -> Run {
        Run {
            end,
            ino: 7,
            off,
            data_end: 0x20000,
        }
    }

    #[test]
    fn cutting_a_run_keeps_the_file_offsets_of_what_is_left() {
        let mut l = Lazy::default();
        l.runs.insert(0x10000, run(0x20000, 0x3000));
        let cut = l.cut(0x14000, 0x15000);
        assert_eq!(cut, [(0x14000, 0x15000, 7)]);
        assert_eq!(l.runs[&0x10000], run(0x14000, 0x3000));
        assert_eq!(l.runs[&0x15000], run(0x20000, 0x8000));
        // Across both, and past them
        let cut = l.cut(0x12000, 0x30000);
        assert_eq!(cut.len(), 2);
        assert_eq!(l.runs.len(), 1);
        assert_eq!(l.runs[&0x10000], run(0x12000, 0x3000));
        assert!(l.cut(0x40000, 0x50000).is_empty());
    }

    #[test]
    fn a_block_with_lazy_pages_gets_a_table_and_is_a_block_again_once_they_are_filled() {
        let size = 64 << 20;
        let ram = Ram::zeroed(size as usize);
        let base = size - BLOCK;
        let mut t = Tables::new(&ram, size, base);
        let spare = t.spare.len();
        // The top level points at the level-2 table; blocks map all of RAM but the tables
        assert_eq!(get(&ram, base), (base + PAGE) | VALID | TABLE);
        assert_eq!(get(&ram, t.l2_entry(3)), (3 * BLOCK) | VALID | MEM);
        assert_eq!(get(&ram, t.l2_entry(base / BLOCK)), 0);

        // Two pages of block 3 lazy, across into block 4
        let a = 4 * BLOCK - PAGE;
        t.make_lazy(&ram, a, a + 2 * PAGE);
        assert_eq!(t.spare.len(), spare - 2);
        let (t3, n3) = t.l3[&3];
        assert_eq!(n3, 1);
        assert_eq!(get(&ram, t.l2_entry(3)), t3 | VALID | TABLE);
        assert_eq!(get(&ram, t3 + 511 * 8), 0, "the lazy page");
        assert_eq!(
            get(&ram, t3),
            (3 * BLOCK) | VALID | TABLE | MEM,
            "the others map as before"
        );
        // Lazy again: counted once
        t.make_lazy(&ram, a, a + PAGE);
        assert_eq!(t.l3[&3].1, 1);

        t.make_valid(&ram, a, a + 2 * PAGE);
        assert!(t.l3.is_empty());
        assert_eq!(t.spare.len(), spare);
        assert_eq!(get(&ram, t.l2_entry(3)), (3 * BLOCK) | VALID | MEM);
        assert!(
            ram.is_zero(t3 as usize, PAGE as usize),
            "the table is given back zeroed"
        );
    }

    #[test]
    fn the_lazy_mappings_and_tables_round_trip() {
        let size = 64 << 20;
        let ram = Ram::zeroed(size as usize);
        let base = size - BLOCK;
        let mut l = Lazy::default();
        let mut t = Tables::new(&ram, size, base);
        t.make_lazy(&ram, BLOCK, BLOCK + 3 * PAGE);
        l.tables = Some(t);
        l.runs.insert(BLOCK, run(BLOCK + 3 * PAGE, 0));
        l.filled = 5;
        let mut w = W(Vec::new());
        l.save(&mut w);
        let back = Lazy::load(&mut R(&w.0), size).unwrap();
        assert_eq!(back.runs, l.runs);
        assert_eq!(back.per_file[&7], 3);
        assert_eq!(back.filled, 5);
        let (a, b) = (l.tables.unwrap(), back.tables.unwrap());
        assert_eq!(a.l3, b.l3);
        let mut s1 = a.spare.clone();
        let mut s2 = b.spare.clone();
        s1.sort();
        s2.sort();
        assert_eq!(s1, s2);
        assert!(back.stale, "the CPU's translation is set again");
    }
}
