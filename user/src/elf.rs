//! Loading an ELF executable as execve does: its segments where they ask (a PIE, or the dynamic
//! loader, at a base of its own), and the initial stack: argc, argv, envp and the auxiliary vector.

use armless::Board;

use crate::mm::Image;
use crate::{ExecError, PAGE};

/// What loading an ELF gave.
pub(crate) struct Loaded {
    /// Where execution starts in it (its base added)
    pub entry: u64,
    /// Where its program headers are in memory, and how many
    pub phdr: u64,
    pub phnum: u64,
    /// The end of its highest segment
    pub end: u64,
    /// The dynamic loader it asks for (PT_INTERP), if any
    pub interp: Option<Vec<u8>>,
}

/// What an ELF needs before it can be loaded: its base when it is position independent (a PIE or
/// a shared object: ET_DYN), and how much address space it spans from there.
pub(crate) struct Header {
    pub dyn_: bool,
    pub span: u64,
}

/// An ELF's headers, read from wherever they are in its file: the program header table and the
/// loader's path (PT_INTERP) need not be among its first bytes.
pub(crate) struct Elf {
    /// ET_EXEC, or ET_DYN (a PIE or a shared object: position independent)
    dyn_: bool,
    entry: u64,
    phoff: u64,
    phnum: u64,
    segs: Vec<Segment>,
    /// The dynamic loader it asks for (PT_INTERP), if any
    pub interp: Option<Vec<u8>>,
}

/// How long a loader's path may be (PATH_MAX)
const INTERP_MAX: u64 = 4096;

fn u16_at(e: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(e[o..o + 2].try_into().unwrap())
}
fn u32_at(e: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(e[o..o + 4].try_into().unwrap())
}
fn u64_at(e: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(e[o..o + 8].try_into().unwrap())
}

/// A program header: (type, file offset, vaddr, file size, memory size, flags)
type Segment = (u32, u64, u64, u64, u64, u32);

/// The headers of `image`: its first bytes say where the program headers and the loader's path
/// are, and those are read from there when they are past them.
pub(crate) fn parse(image: &Image, board: Option<&Board>) -> Result<Elf, ExecError> {
    let elf = image.head();
    if elf.len() < 64
        || &elf[..4] != b"\x7fELF"
        || elf[4] != 2
        || elf[5] != 1
        || u16_at(elf, 18) != 183
    {
        return Err(ExecError::Fail(
            "not a 64-bit little-endian aarch64 ELF".into(),
        ));
    }
    let dyn_ = match u16_at(elf, 16) {
        2 => false,
        3 => true,
        t => {
            return Err(ExecError::Fail(format!(
                "ELF type {t} is not an executable"
            )))
        },
    };
    let (phoff, phentsize, phnum) = (
        u64_at(elf, 32),
        u16_at(elf, 54) as usize,
        u16_at(elf, 56) as usize,
    );
    if phentsize < 56 {
        return Err(ExecError::Fail("truncated ELF".into()));
    }
    let table = image.read(board, phoff, phnum * phentsize)?;
    let segs: Vec<Segment> = (0..phnum)
        .map(|i| {
            let p = i * phentsize;
            (
                u32_at(&table, p),
                u64_at(&table, p + 8),
                u64_at(&table, p + 16),
                u64_at(&table, p + 32),
                u64_at(&table, p + 40),
                u32_at(&table, p + 4),
            )
        })
        .collect();
    let mut interp = None;
    if let Some(&(_, off, _, filesz, _, _)) = segs.iter().find(|s| s.0 == 3) {
        if filesz > INTERP_MAX {
            return Err(ExecError::Fail("its loader's path is too long".into()));
        }
        let path = image.read(board, off, filesz as usize)?;
        interp = Some(path.split(|&b| b == 0).next().unwrap_or_default().to_vec());
    }
    Ok(Elf {
        dyn_,
        entry: u64_at(elf, 24),
        phoff,
        phnum: phnum as u64,
        segs,
        interp,
    })
}

impl Elf {
    pub(crate) fn header(&self) -> Header {
        let span = self
            .segs
            .iter()
            .filter(|s| s.0 == 1)
            .map(|s| s.2 + s.4)
            .max()
            .unwrap_or(0);
        Header {
            dyn_: self.dyn_,
            span: span.next_multiple_of(PAGE),
        }
    }
}

/// A PT_LOAD segment placed: its file bytes `off..off + filesz` at `at`, and zeros up to
/// `at + memsz`, with the protection (`mm::PROT_*`) its flags ask for.
pub(crate) struct Seg {
    pub at: u64,
    pub off: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub prot: u8,
}

/// Where the PT_LOAD segments go at `base` plus their addresses: what loading it gives, and the
/// segments to copy or map.
pub(crate) fn plan(elf: &Elf, limit: u64, base: u64) -> Result<(Loaded, Vec<Seg>), String> {
    let mut phdr = 0;
    let mut end = 0;
    let mut segs = Vec::new();
    for &(ty, off, vaddr, filesz, memsz, flags) in &elf.segs {
        match ty {
            6 => phdr = base + vaddr,
            1 => {
                let at = base + vaddr;
                if at.checked_add(memsz).is_none_or(|e| e > limit) || filesz > memsz {
                    return Err(format!("segment at {at:#x} is past the address space"));
                }
                // PF_X, PF_W, PF_R as PROT_EXEC, PROT_WRITE, PROT_READ
                let prot = (flags & 4 != 0) as u8 | (flags & 2) as u8 | ((flags & 1) as u8) << 2;
                segs.push(Seg {
                    at,
                    off,
                    filesz,
                    memsz,
                    prot,
                });
                if phdr == 0 && off == 0 {
                    phdr = at + elf.phoff;
                }
                end = end.max(at + memsz);
            },
            _ => {},
        }
    }
    let loaded = Loaded {
        entry: base + elf.entry,
        phdr,
        phnum: elf.phnum,
        end,
        interp: elf.interp.clone(),
    };
    Ok((loaded, segs))
}

/// argc, argv, envp and auxv at the top of the stack, as Linux lays them out. Returns sp.
#[allow(clippy::too_many_arguments)] // what execve lays out, each its own part
pub(crate) fn stack(
    write: &dyn Fn(u64, &[u8]) -> bool,
    top: u64,
    args: &[String],
    env: &[String],
    exe: &Loaded,
    interp_base: u64,
    execfn: &[u8],
    random: &[u8; 16],
) -> u64 {
    let mut sp = top;
    let mut put = |s: &[u8]| {
        sp -= s.len() as u64 + 1;
        write(sp, s);
        write(sp + s.len() as u64, &[0]);
        sp
    };
    let execfn = put(execfn);
    let argv: Vec<u64> = args.iter().map(|s| put(s.as_bytes())).collect();
    let envp: Vec<u64> = env.iter().map(|s| put(s.as_bytes())).collect();
    let platform = put(b"aarch64");
    sp -= 16;
    let rand = sp;
    write(rand, random);

    // FP | ASIMD | AES | PMULL | SHA1 | SHA2 | CRC32 | ATOMICS | SHA3 | SM3 | SM4 | SHA512 (what
    // armless implements)
    let hwcap = 0x2e_01fb;
    let auxv: &[(u64, u64)] = &[
        (3, exe.phdr),
        (4, 56),
        (5, exe.phnum),
        (6, PAGE),
        (7, interp_base),
        (8, 0),
        (9, exe.entry),
        (11, 0),
        (12, 0),
        (13, 0),
        (14, 0),
        (15, platform),
        (16, hwcap),
        (17, 100),
        (23, 0),
        (25, rand),
        (26, 0),
        (31, execfn),
        (51, 5120),
        (0, 0),
    ];
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * auxv.len();
    sp &= !15;
    if words % 2 == 1 {
        sp -= 8;
    }
    sp -= 8 * words as u64;
    let mut at = sp;
    let mut push = |w: u64| {
        write(at, &w.to_le_bytes());
        at += 8;
    };
    push(argv.len() as u64);
    argv.iter().for_each(|&a| push(a));
    push(0);
    envp.iter().for_each(|&e| push(e));
    push(0);
    for &(k, val) in auxv {
        push(k);
        push(val);
    }
    sp
}
