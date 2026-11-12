//! Loading an ELF executable as execve does: its segments where they ask (a PIE, or the dynamic
//! loader, at a base of its own), and the initial stack: argc, argv, envp and the auxiliary vector.

use crate::PAGE;

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

fn segments(elf: &[u8]) -> Result<Vec<Segment>, String> {
    if elf.len() < 64
        || &elf[..4] != b"\x7fELF"
        || elf[4] != 2
        || elf[5] != 1
        || u16_at(elf, 18) != 183
    {
        return Err("not a 64-bit little-endian aarch64 ELF".into());
    }
    let (phoff, phentsize, phnum) = (
        u64_at(elf, 32) as usize,
        u16_at(elf, 54) as usize,
        u16_at(elf, 56) as usize,
    );
    (0..phnum)
        .map(|i| {
            let p = phoff + i * phentsize;
            elf.get(p..p + 56).ok_or("truncated ELF")?;
            Ok((
                u32_at(elf, p),
                u64_at(elf, p + 8),
                u64_at(elf, p + 16),
                u64_at(elf, p + 32),
                u64_at(elf, p + 40),
                u32_at(elf, p + 4),
            ))
        })
        .collect()
}

pub(crate) fn header(elf: &[u8]) -> Result<Header, String> {
    let segs = segments(elf)?;
    let dyn_ = match u16_at(elf, 16) {
        2 => false,
        3 => true,
        t => return Err(format!("ELF type {t} is not an executable")),
    };
    let span = segs
        .iter()
        .filter(|s| s.0 == 1)
        .map(|s| s.2 + s.4)
        .max()
        .unwrap_or(0);
    Ok(Header {
        dyn_,
        span: span.next_multiple_of(PAGE),
    })
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

/// Where the PT_LOAD segments go at `base` plus their addresses, from the ELF's first bytes (its
/// headers, and the loader's path PT_INTERP names, are there): what loading it gives, and the
/// segments to copy or map.
pub(crate) fn plan(elf: &[u8], limit: u64, base: u64) -> Result<(Loaded, Vec<Seg>), String> {
    let all = segments(elf)?;
    let phoff = u64_at(elf, 32);
    let mut phdr = 0;
    let mut end = 0;
    let mut interp = None;
    let mut segs = Vec::new();
    for (ty, off, vaddr, filesz, memsz, flags) in all {
        match ty {
            3 => {
                let path = elf
                    .get(off as usize..(off + filesz) as usize)
                    .ok_or("truncated ELF")?;
                interp = Some(path.split(|&b| b == 0).next().unwrap_or_default().to_vec());
            },
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
                    phdr = at + phoff;
                }
                end = end.max(at + memsz);
            },
            _ => {},
        }
    }
    let loaded = Loaded {
        entry: base + u64_at(elf, 24),
        phdr,
        phnum: u16_at(elf, 56) as u64,
        end,
        interp,
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

    // FP | ASIMD | AES | PMULL | SHA1 | SHA2 | CRC32 | ATOMICS (what armless implements)
    let hwcap = 0x1fb;
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
