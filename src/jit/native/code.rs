//! Memory for the native JIT's code: one region reserved up front, with the functions packed next
//! to each other. Cranelift's own provider maps every function separately and changes its
//! protection when it is done, which rounds each function up to a page (16 KiB on Apple
//! hardware) and makes a few system calls per function; the page JIT compiles thousands.
//!
//! The region is never writable and executable for the same thread at once. On macOS (arm64) it
//! is one MAP_JIT mapping that the compiler thread makes writable for itself while it writes
//! (every other thread only executes it). On Linux it is a memory file mapped twice: the compiler
//! writes through one mapping and the CPUs run code through the other. The functions refer to
//! nothing outside themselves (helpers are called through addresses in registers; see
//! `Compiler::compile`), so code written at one address runs at the other.

use cranelift_jit::{BranchProtection, JITMemoryKind, JITMemoryProvider};
use cranelift_module::ModuleResult;

/// A region of code memory; `CodeMemory::new` returns None where the host refuses one (the JIT
/// then uses Cranelift's own memory). Only on macOS (arm64) and Linux; elsewhere the JIT always
/// uses Cranelift's.
pub struct CodeMemory {
    /// Where the compiler writes, and where the code runs (the same on macOS).
    write: *mut u8,
    exec: *mut u8,
    size: usize,
    used: usize,
    /// Start of what was written since the last `finalize`.
    fresh: usize,
}

// SAFETY: only the compiler thread that owns the provider writes the region.
unsafe impl Send for CodeMemory {}

impl CodeMemory {
    /// Where code written at `p` runs.
    pub fn exec_address(&self, p: *const u8) -> *const u8 {
        p.wrapping_offset(self.exec as isize - self.write as isize)
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn new(size: usize) -> Option<CodeMemory> {
        // SAFETY: a fresh anonymous mapping.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        (p != libc::MAP_FAILED).then_some(CodeMemory {
            write: p as *mut u8,
            exec: p as *mut u8,
            size,
            used: 0,
            fresh: 0,
        })
    }

    #[cfg(target_os = "linux")]
    pub fn new(size: usize) -> Option<CodeMemory> {
        // SAFETY: a fresh memory file, mapped twice; the descriptor is not needed after.
        unsafe {
            let fd = libc::syscall(
                libc::SYS_memfd_create,
                c"armless-jit".as_ptr(),
                libc::MFD_CLOEXEC,
            ) as libc::c_int;
            if fd < 0 {
                return None;
            }
            let map = |prot| libc::mmap(std::ptr::null_mut(), size, prot, libc::MAP_SHARED, fd, 0);
            let ok = libc::ftruncate(fd, size as libc::off_t) == 0;
            let write = if ok { map(libc::PROT_READ | libc::PROT_WRITE) } else { libc::MAP_FAILED };
            let exec = if write != libc::MAP_FAILED {
                map(libc::PROT_READ | libc::PROT_EXEC)
            } else {
                libc::MAP_FAILED
            };
            libc::close(fd);
            if exec == libc::MAP_FAILED {
                if write != libc::MAP_FAILED {
                    libc::munmap(write, size);
                }
                return None;
            }
            Some(CodeMemory {
                write: write as *mut u8,
                exec: exec as *mut u8,
                size,
                used: 0,
                fresh: 0,
            })
        }
    }

    /// Let this thread write code memory (`true`), or stop it. Only macOS needs this: elsewhere
    /// the compiler writes through a mapping of its own.
    pub fn writable(on: bool) {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        // SAFETY: changes this thread's view of MAP_JIT memory only.
        unsafe {
            pthread_jit_write_protect_np(!on as libc::c_int)
        };
        let _ = on;
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe extern "C" {
    fn pthread_jit_write_protect_np(enabled: libc::c_int);
    fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
}

/// Make the `len` bytes of code just written at `p` visible to instruction fetch on every core
/// (each core still needs an instruction barrier before it runs them; see `Native::take_done`).
#[cfg(target_arch = "aarch64")]
fn sync_icache(p: *mut u8, len: usize) {
    #[cfg(target_os = "macos")]
    // SAFETY: `p..p + len` is code memory.
    unsafe {
        sys_icache_invalidate(p as *mut libc::c_void, len)
    };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: cache maintenance of `p..p + len`, which is mapped; it changes no data.
    unsafe {
        let ctr: u64;
        std::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack));
        let dline = 4usize << ((ctr >> 16) & 0xf);
        let iline = 4usize << (ctr & 0xf);
        let (start, end) = (p as usize, p as usize + len);
        let mut a = start & !(dline - 1);
        while a < end {
            std::arch::asm!("dc cvau, {}", in(reg) a, options(nostack));
            a += dline;
        }
        std::arch::asm!("dsb ish", options(nostack));
        let mut a = start & !(iline - 1);
        while a < end {
            std::arch::asm!("ic ivau, {}", in(reg) a, options(nostack));
            a += iline;
        }
        std::arch::asm!("dsb ish", "isb", options(nostack));
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn sync_icache(_: *mut u8, _: usize) {}

impl JITMemoryProvider for CodeMemory {
    fn allocate(&mut self, size: usize, align: u64, _: JITMemoryKind) -> std::io::Result<*mut u8> {
        let at = self.used.next_multiple_of(align.max(16) as usize);
        if at + size > self.size {
            return Err(std::io::Error::other("the JIT's code memory is full"));
        }
        self.used = at + size;
        // SAFETY: inside the region.
        Ok(unsafe { self.write.add(at) })
    }

    unsafe fn free_memory(&mut self) {
        // SAFETY: no code in the region runs any more (see `Compiler::drop`).
        unsafe {
            libc::munmap(self.write as *mut libc::c_void, self.size);
            if self.exec != self.write {
                libc::munmap(self.exec as *mut libc::c_void, self.size);
            }
        }
        self.used = 0;
        self.fresh = 0;
    }

    fn finalize(&mut self, _: BranchProtection) -> ModuleResult<()> {
        // SAFETY: inside the region.
        sync_icache(unsafe { self.exec.add(self.fresh) }, self.used - self.fresh);
        self.fresh = self.used;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_written_runs() {
        #[cfg(target_arch = "aarch64")]
        let function: &[u8] = &[0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6]; // mov w0, #42; ret
        #[cfg(target_arch = "x86_64")]
        let function: &[u8] = &[0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3]; // mov eax, 42; ret
        let mut memory = CodeMemory::new(1 << 20).expect("code memory");
        for _ in 0..3 {
            let p = memory
                .allocate(function.len(), 4, JITMemoryKind::Executable)
                .unwrap();
            CodeMemory::writable(true);
            // SAFETY: `p` has room for the function.
            unsafe { std::ptr::copy_nonoverlapping(function.as_ptr(), p, function.len()) };
            memory.finalize(BranchProtection::None).unwrap();
            CodeMemory::writable(false);
            // SAFETY: the function was just written and made visible to instruction fetch.
            let f: extern "C" fn() -> i32 = unsafe { std::mem::transmute(memory.exec_address(p)) };
            assert_eq!(f(), 42);
        }
        // SAFETY: none of its code runs any more.
        unsafe { memory.free_memory() };
    }
}
