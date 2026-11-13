//! The native JIT: the wasm function `gen` writes for a page, compiled to host code with
//! Cranelift instead of handed to a browser.
//!
//! `gen` uses a small part of wasm (i32/i64 integers, structured control flow, locals, three
//! helper calls), so this translates exactly that, from the function body's bytes. Natively the
//! static offset of every memory access is a host address (`jit_layout_el` gives host addresses
//! here) and the dynamic part on the stack (a page offset into RAM, a TLB index) is added to it
//! zero-extended, as wasm adds it to its memory's base. The code `gen` writes never accesses
//! memory out of bounds, so there are no bounds checks.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F32, F64, I16, I32, I64, I8};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, InstBuilder, JumpTableData, MemFlagsData, SigRef, Type, Value,
};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use super::wasm;

#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
#[allow(unsafe_code)]
mod code;

/// Code memory for a module (`code::CodeMemory`) and how far from where it is written its code
/// runs; None where the host has none, and Cranelift's own memory is used.
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
fn code_memory() -> Option<(Box<dyn cranelift_jit::JITMemoryProvider + Send>, isize)> {
    // Room for the code limit and what is still compiled once it is reached.
    let memory = code::CodeMemory::new(2 * code_limit())?;
    let offset = memory.exec_address(std::ptr::null()) as isize;
    Some((Box::new(memory), offset))
}

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
fn code_memory() -> Option<(Box<dyn cranelift_jit::JITMemoryProvider + Send>, isize)> {
    None
}

/// Let the compiler thread write code memory, or stop it (see `code::CodeMemory::writable`).
fn code_writable(on: bool) {
    #[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
    code::CodeMemory::writable(on);
    let _ = on;
}

/// Code compiled into one module before it is dropped and the JIT starts over (`full`). Code of
/// replaced modules stays until then, so this bounds the code a CPU holds: Node starting Storage
/// compiles 3 to 17 MB per CPU, and a lower limit (`ARMLESS_JIT_CODE_MB`) costs recompiles.
const CODE_LIMIT: usize = 16 << 20;

/// The code limit: `ARMLESS_JIT_CODE_MB` megabytes of compiled code per CPU, or `CODE_LIMIT`.
fn code_limit() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("ARMLESS_JIT_CODE_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&mb| mb > 0)
            .map_or(CODE_LIMIT, |mb| mb << 20)
    })
}

/// A field of the CPU whose value the compiled code remembers after storing or loading it, so
/// that a later load of it within the same function reuses the value instead of reading memory
/// again: its host address, and whether it is a u64 (a general register, SP, the instruction
/// count) or a byte (a flag, read with `i32.load8_u` and written with `i32.store8`). Stores still
/// go to memory, so the CPU's state there is always current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cached {
    pub addr: u64,
    pub byte: bool,
}

/// What `BASES` functions take after the PC: the host regions their code reaches at fixed
/// addresses, as (start, length) — the CPU itself, guest RAM and the fast TLB's four tables for the
/// code's EL. An address in one of them is compiled as the parameter plus an offset, one
/// instruction where its 64-bit constant took four or five; the caller passes the same starts, so
/// the addresses are the ones compiled in (the code was always for this CPU's addresses).
pub const BASES: usize = 6;

#[derive(Clone, Copy, Default, Debug)]
pub struct Bases(pub [(u64, u64); BASES]);

/// The helpers blocks call, as host function addresses (see `gen::imports`).
#[derive(Clone, Copy)]
pub struct Helpers {
    /// (va: i64, size: i32) -> i64
    pub read: usize,
    /// (va: i64, size: i32, value: i64)
    pub write: usize,
    /// (insn: i32, pc: i64) -> i32
    pub interp: usize,
    /// (op: i32, x: i64, y: i64, d: i64) -> i64
    pub fp3: usize,
}

/// A request to a CPU's compiler thread.
enum Job {
    Compile {
        generation: u64,
        slot: u32,
        body: Vec<u8>,
        cached: Vec<Cached>,
        bases: Bases,
    },
    /// Free the module and compile into a new one: nothing compiled before is reachable any more.
    Reset,
    /// Something was queued in `later`.
    Wake,
}

/// A compiled slot: the address of its function, or 0 if it could not be compiled, and the size
/// of its code.
pub struct Done {
    pub generation: u64,
    pub slot: u32,
    pub function: usize,
    pub size: usize,
}

/// ARMLESS_JIT_CODE_DUMP=DIR: the code of every compiled function, appended to
/// `DIR/code-<pid>.bin`, and `DIR/code-<pid>.idx` with a line per function: its address, size and
/// offset in the `.bin`, for disassembling what a profile found hot.
pub(crate) fn code_dump() -> Option<&'static std::sync::Mutex<(std::fs::File, std::fs::File, u64)>>
{
    static DUMP: std::sync::OnceLock<
        Option<std::sync::Mutex<(std::fs::File, std::fs::File, u64)>>,
    > = std::sync::OnceLock::new();
    DUMP.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("ARMLESS_JIT_CODE_DUMP")?);
        let pid = std::process::id();
        let open = |name: String| std::fs::File::create(dir.join(name)).ok();
        Some(std::sync::Mutex::new((
            open(format!("code-{pid}.bin"))?,
            open(format!("code-{pid}.idx"))?,
            0,
        )))
    })
    .as_ref()
}

/// ARMLESS_JIT_PERF_MAP: `/tmp/perf-<pid>.map`, which profilers (samply, perf) read to name the
/// host code the JIT made, one line per compiled function (`Cpu::jit_take_compiled` writes them).
/// Whether stores of the cached fields wait until something could see them (`Translator::flush`),
/// so that one a later store replaces is never made. `ARMLESS_JIT_DEFER_STORES=0` makes each.
fn defer_stores() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("ARMLESS_JIT_DEFER_STORES").map_or(true, |v| v != "0"))
}

pub(crate) fn perf_map() -> Option<&'static std::fs::File> {
    static MAP: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        std::env::var_os("ARMLESS_JIT_PERF_MAP")?;
        // Appended to, a whole line per write, from every CPU
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("/tmp/perf-{}.map", std::process::id()))
            .ok()
    })
    .as_ref()
}

#[derive(Default)]
struct Shared {
    /// Results sent and not yet taken, so the CPU can check for them with one load.
    pending: AtomicUsize,
    /// Jobs submitted and not yet answered.
    in_flight: AtomicUsize,
    code_bytes: AtomicUsize,
    compiles: AtomicU64,
    /// Compiles that failed (the block stays interpreted): `gen` made something this does not
    /// translate.
    failures: AtomicU64,
    compile_ns: AtomicU64,
    /// Jobs that wait for the compiler to have nothing else to do (`submit_later`).
    later: Mutex<VecDeque<Job>>,
}

/// One CPU's native JIT: a compiler thread of its own owns the generated code, so the CPU goes on
/// interpreting while a page compiles, and takes the result when it is there (as in the browser,
/// where the host compiles asynchronously too).
///
/// Or, made with `new_here`, it compiles on the caller's thread before `submit` returns, and
/// starts no thread until `start_thread`: for a process that compiles and then forks, whose
/// children all run what it compiled. A child never writes into that module (on Linux its memory
/// is shared with the parent and every other child): `start_thread` keeps it, unwritten, for the
/// code already pointing into it, and compiles into a new module of its own.
pub struct Native {
    jobs: Option<Sender<Job>>,
    done: Receiver<Done>,
    shared: Arc<Shared>,
    /// Results of an older generation point into a module that has been freed.
    pub generation: u64,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Compiling on the caller's thread (`new_here`): the compiler, and where its results go
    here: Option<(Compiler, Sender<Done>)>,
    /// The module compiled into on the caller's thread before `start_thread`: written no more,
    /// kept until the next reset for the code that points into it
    kept: Option<Compiler>,
    locals: &'static [(u32, u8)],
    helpers: Helpers,
}

/// Whether code in Cranelift's own memory can be made visible to every core. On Linux arm64
/// Cranelift does that with membarrier(2) and panics if the kernel refuses it, which a sandbox may
/// (seccomp, gVisor): there, ask first and stay with the interpreter if neither command it uses is
/// available. (`CodeMemory` needs no system call for it.)
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[allow(unsafe_code)]
fn can_publish_code() -> bool {
    const QUERY: libc::c_long = 0;
    const GLOBAL: libc::c_long = 1;
    const PRIVATE_EXPEDITED_SYNC_CORE: libc::c_long = 1 << 5;
    // SAFETY: MEMBARRIER_CMD_QUERY only reports the supported commands.
    let supported = unsafe { libc::syscall(libc::SYS_membarrier, QUERY, 0) };
    supported > 0 && supported & (GLOBAL | PRIVATE_EXPEDITED_SYNC_CORE) != 0
}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn can_publish_code() -> bool {
    true
}

impl Native {
    /// None where Cranelift does not support the host. `locals` and the helpers are those of the
    /// functions `gen` writes for this CPU.
    pub fn new(locals: &'static [(u32, u8)], helpers: Helpers) -> Option<Native> {
        let (done_tx, done) = channel::<Done>();
        let shared = Arc::new(Shared::default());
        let (jobs, thread) = spawn(locals, helpers, shared.clone(), done_tx)?;
        Some(Native {
            jobs: Some(jobs),
            done,
            shared,
            generation: 0,
            thread: Some(thread),
            here: None,
            kept: None,
            locals,
            helpers,
        })
    }

    /// A native JIT that compiles on the caller's thread, and starts none, until `start_thread`
    /// (see `Native`). None where Cranelift does not support the host.
    pub fn new_here(locals: &'static [(u32, u8)], helpers: Helpers) -> Option<Native> {
        let compiler = Compiler::new()?;
        let (done_tx, done) = channel::<Done>();
        Some(Native {
            jobs: None,
            done,
            shared: Arc::new(Shared::default()),
            generation: 0,
            thread: None,
            here: Some((compiler, done_tx)),
            kept: None,
            locals,
            helpers,
        })
    }

    /// Compile on a thread of its own from now on, into a module of its own: what was compiled on
    /// the caller's thread stays where it is, unwritten. False if no thread could be started (the
    /// JIT then compiles nothing more).
    pub fn start_thread(&mut self) -> bool {
        let Some((compiler, done_tx)) = self.here.take() else {
            return self.jobs.is_some();
        };
        self.kept = Some(compiler);
        self.shared.code_bytes.store(0, Ordering::Relaxed);
        match spawn(self.locals, self.helpers, self.shared.clone(), done_tx) {
            Some((jobs, thread)) => {
                self.jobs = Some(jobs);
                self.thread = Some(thread);
                true
            },
            None => false,
        }
    }

    /// Whether this compiles on the caller's thread (`new_here`, before `start_thread`).
    pub fn compiles_here(&self) -> bool {
        self.here.is_some()
    }

    /// Compile `body` (a `gen` function) for `slot`, remembering the `cached` fields' values; the
    /// result comes back through `take_done`.
    pub fn submit(&mut self, slot: u32, body: Vec<u8>, cached: Vec<Cached>, bases: Bases) -> bool {
        let generation = self.generation;
        if let Some((compiler, done_tx)) = self.here.as_mut() {
            let (function, size) = compile_one(
                compiler,
                &self.shared,
                self.locals,
                &self.helpers,
                &body,
                &cached,
                &bases,
            );
            let _ = done_tx.send(Done {
                generation,
                slot,
                function,
                size,
            });
            self.shared.pending.fetch_add(1, Ordering::Release);
            return true;
        }
        self.shared.in_flight.fetch_add(1, Ordering::Relaxed);
        let sent = self.jobs.as_ref().is_some_and(|j| {
            j.send(Job::Compile {
                generation,
                slot,
                body,
                cached,
                bases,
            })
            .is_ok()
        });
        if !sent {
            self.shared.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        sent
    }

    /// `submit`, for code that is not needed yet (a profile's, see `Cpu::jit_prewarm`): compiled
    /// when nothing submitted is waiting.
    pub fn submit_later(
        &mut self,
        slot: u32,
        body: Vec<u8>,
        cached: Vec<Cached>,
        bases: Bases,
    ) -> bool {
        if self.here.is_some() {
            return self.submit(slot, body, cached, bases);
        }
        let Some(jobs) = self.jobs.as_ref() else {
            return false;
        };
        self.shared.in_flight.fetch_add(1, Ordering::Relaxed);
        self.shared.later.lock().unwrap().push_back(Job::Compile {
            generation: self.generation,
            slot,
            body,
            cached,
            bases,
        });
        let _ = jobs.send(Job::Wake);
        true
    }

    /// Testing aid: wait until every submitted page is compiled (the results still to be taken).
    pub fn wait_idle(&self) {
        while self.shared.in_flight.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }
    }

    /// Whether compiled slots are waiting to be taken.
    #[inline(always)]
    pub fn has_done(&self) -> bool {
        self.shared.pending.load(Ordering::Relaxed) != 0
    }

    #[allow(unsafe_code)]
    pub fn take_done(&mut self) -> Vec<Done> {
        let done: Vec<Done> = self.done.try_iter().collect();
        self.shared.pending.fetch_sub(done.len(), Ordering::Acquire);
        // The code was written on the compiler thread's core: discard anything this core
        // prefetched from the addresses before (code memory is reused after a reset).
        #[cfg(target_arch = "aarch64")]
        if !done.is_empty() {
            // SAFETY: an instruction barrier has no other effect.
            unsafe { std::arch::asm!("isb", options(nostack, preserves_flags)) };
        }
        done
    }

    /// Whether the module holds enough code that the JIT should flush and `reset`.
    pub fn full(&self) -> bool {
        self.shared.code_bytes.load(Ordering::Relaxed) >= code_limit()
    }

    /// Start a new module. The caller has flushed every slot: no code of the old one is reachable,
    /// and results still to come from it are of an older generation.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.shared.code_bytes.store(0, Ordering::Relaxed);
        // Every slot is flushed: nothing points into a kept module any more
        self.kept = None;
        if let Some((compiler, _)) = self.here.as_mut() {
            if let Some(fresh) = Compiler::new() {
                *compiler = fresh;
            }
        }
        if let Some(j) = &self.jobs {
            let _ = j.send(Job::Reset);
        }
    }

    /// Bytes of compiled code in the current module (stale functions included, until a reset).
    pub fn code_bytes(&self) -> usize {
        self.shared.code_bytes.load(Ordering::Relaxed)
    }

    /// Statistics: (functions compiled, nanoseconds spent compiling them).
    /// Compiles that failed, leaving their blocks to the interpreter.
    pub fn failures(&self) -> u64 {
        self.shared.failures.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.shared.compiles.load(Ordering::Relaxed),
            self.shared.compile_ns.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // Closing the queue ends the thread, which frees the module.
        self.jobs.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start a compiler thread: it makes its module, then compiles the jobs sent to it, answering on
/// `done_tx`. None if it could not start or Cranelift does not support the host.
fn spawn(
    locals: &'static [(u32, u8)],
    helpers: Helpers,
    thread_shared: Arc<Shared>,
    done_tx: Sender<Done>,
) -> Option<(Sender<Job>, std::thread::JoinHandle<()>)> {
    let (jobs, job_rx) = channel::<Job>();
    let (ready_tx, ready_rx) = channel::<bool>();
    let thread = std::thread::Builder::new()
        .name("armless-jit".into())
        .spawn(move || {
            let Some(mut compiler) = Compiler::new() else {
                let _ = ready_tx.send(false);
                return;
            };
            let _ = ready_tx.send(true);
            loop {
                let job = match job_rx.try_recv() {
                    Ok(job) => job,
                    Err(TryRecvError::Disconnected) => return,
                    Err(TryRecvError::Empty) => {
                        // (not held while waiting below: `submit_later` takes it)
                        let next = thread_shared.later.lock().unwrap().pop_front();
                        match next {
                            Some(job) => job,
                            None => {
                                // Nothing queued: give back what compiling took, until the next job
                                compiler.release_scratch();
                                match job_rx.recv() {
                                    Ok(job) => job,
                                    Err(_) => return,
                                }
                            },
                        }
                    },
                };
                match job {
                    Job::Wake => {},
                    Job::Compile {
                        generation,
                        slot,
                        body,
                        cached,
                        bases,
                    } => {
                        let (function, size) = compile_one(
                            &mut compiler,
                            &thread_shared,
                            locals,
                            &helpers,
                            &body,
                            &cached,
                            &bases,
                        );
                        if done_tx
                            .send(Done {
                                generation,
                                slot,
                                function,
                                size,
                            })
                            .is_err()
                        {
                            return;
                        }
                        thread_shared.pending.fetch_add(1, Ordering::Release);
                        thread_shared.in_flight.fetch_sub(1, Ordering::Release);
                    },
                    Job::Reset => {
                        // What waits for later is for the old module: answer it, uncompiled
                        for job in std::mem::take(&mut *thread_shared.later.lock().unwrap()) {
                            if let Job::Compile {
                                generation, slot, ..
                            } = job
                            {
                                let _ = done_tx.send(Done {
                                    generation,
                                    slot,
                                    function: 0,
                                    size: 0,
                                });
                                thread_shared.pending.fetch_add(1, Ordering::Release);
                                thread_shared.in_flight.fetch_sub(1, Ordering::Release);
                            }
                        }
                        let Some(fresh) = Compiler::new() else {
                            return;
                        };
                        compiler = fresh;
                        thread_shared.code_bytes.store(0, Ordering::Relaxed);
                    },
                }
            }
        })
        .ok()?;
    if !ready_rx.recv().unwrap_or(false) {
        let _ = thread.join();
        return None;
    }
    Some((jobs, thread))
}

/// Compile one function, counting it: its address, or 0 if it could not be compiled, and the size
/// of its code.
fn compile_one(
    compiler: &mut Compiler,
    shared: &Shared,
    locals: &'static [(u32, u8)],
    helpers: &Helpers,
    body: &[u8],
    cached: &[Cached],
    bases: &Bases,
) -> (usize, usize) {
    let started = std::time::Instant::now();
    let (function, size) = compiler
        .compile(body, locals, helpers, cached, bases)
        .map_or((0, 0), |(f, size)| (f as usize, size));
    shared.compiles.fetch_add(1, Ordering::Relaxed);
    if function == 0 {
        shared.failures.fetch_add(1, Ordering::Relaxed);
    }
    shared
        .compile_ns
        .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    shared
        .code_bytes
        .store(compiler.code_bytes, Ordering::Relaxed);
    (function, size)
}

/// The compiler thread's state: the module the generated code lives in.
struct Compiler {
    /// Freed by hand in `drop`: its code must not outlive the slots that point into it.
    module: std::mem::ManuallyDrop<JITModule>,
    ctx: Context,
    fctx: FunctionBuilderContext,
    code_bytes: usize,
    functions: u32,
    /// How far from where it was written each function runs (with `CodeMemory` on Linux).
    exec_offset: isize,
    /// Whether `ctx` and `fctx` hold the buffers of a compile since `release_scratch`.
    scratch: bool,
}

impl Compiler {
    fn new() -> Option<Compiler> {
        let mut flags = settings::builder();
        let opt = std::env::var("ARMLESS_JIT_OPT").unwrap_or_else(|_| "speed".into());
        flags.set("opt_level", &opt).ok()?;
        if let Ok(regalloc) = std::env::var("ARMLESS_JIT_REGALLOC") {
            flags.set("regalloc_algorithm", &regalloc).ok()?;
        }
        // Checking the IR of every function takes about a third of the compile time; the block
        // tests and fuzzers keep the translation honest.
        flags.set("enable_verifier", "false").ok()?;
        flags.set("use_colocated_libcalls", "false").ok()?;
        flags.set("is_pic", "false").ok()?;
        let isa = cranelift_native::builder()
            .ok()?
            .finish(settings::Flags::new(flags))
            .ok()?;
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        let mut exec_offset = 0;
        if let Some((memory, offset)) = code_memory() {
            exec_offset = offset;
            builder.memory_provider(memory);
        } else if !can_publish_code() {
            return None;
        }
        let module = JITModule::new(builder);
        let ctx = module.make_context();
        Some(Compiler {
            module: std::mem::ManuallyDrop::new(module),
            ctx,
            fctx: FunctionBuilderContext::new(),
            code_bytes: 0,
            functions: 0,
            exec_offset,
            scratch: false,
        })
    }

    /// Compile the body of a `gen` function, `(pc: i64) -> i32` with `locals` after the
    /// parameter: its address and the size of its code. None if it uses something this does not
    /// translate.
    fn compile(
        &mut self,
        body: &[u8],
        locals: &[(u32, u8)],
        h: &Helpers,
        cached: &[Cached],
        bases: &Bases,
    ) -> Option<(*const u8, usize)> {
        self.scratch = true;
        let mut sig = self.module.make_signature();
        // (pc, the BASES starts) -> status
        for _ in 0..=BASES {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I32));
        let name = format!("page{}", self.functions);
        self.functions += 1;
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .ok()?;
        self.ctx.func.signature = sig;
        let target = self.module.target_config();
        let translated = {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let ok = Translator::new(&mut b, locals, h, cached, bases)
                .run(body)
                .is_some();
            if ok {
                b.seal_all_blocks();
                b.finalize(target);
            }
            ok
        };
        if !translated {
            self.module.clear_context(&mut self.ctx);
            return None;
        }
        code_writable(true);
        let defined = self.module.define_function(id, &mut self.ctx);
        // Code that runs where it was written may refer to other addresses; elsewhere it must
        // refer to none (`gen` calls the helpers through addresses in registers, and jump tables
        // are relative to the function).
        let movable = self.exec_offset == 0
            || self
                .ctx
                .compiled_code()
                .is_some_and(|c| c.buffer.relocs().is_empty());
        let size = self
            .ctx
            .compiled_code()
            .map_or(0, |code| code.code_buffer().len());
        self.code_bytes += size;
        self.module.clear_context(&mut self.ctx);
        let finalized = defined.is_ok() && movable && self.module.finalize_definitions().is_ok();
        code_writable(false);
        finalized.then(|| {
            (
                self.module
                    .get_finalized_function(id)
                    .wrapping_offset(self.exec_offset),
                size,
            )
        })
    }
}

impl Compiler {
    /// Frees the buffers compiling grew: Cranelift keeps them between functions, as large as the
    /// largest function compiled (a page module can be), for as long as the compiler lives.
    fn release_scratch(&mut self) {
        if std::mem::take(&mut self.scratch) {
            self.ctx = self.module.make_context();
            self.fctx = FunctionBuilderContext::new();
            return_freed_memory();
        }
    }
}

/// Hands memory the allocator holds free back to the system: malloc keeps the pages of what is
/// freed, and counts them as the process's, until asked.
#[allow(unsafe_code)]
fn return_freed_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means every zone; a zero goal, as much as it can.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: only returns free memory to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

impl Drop for Compiler {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: a compiler is dropped on a reset (every slot flushed first) or with its CPU, so
        // none of its code is reachable any more; `module` is not used again.
        unsafe { std::mem::ManuallyDrop::take(&mut self.module).free_memory() };
    }
}

enum Kind {
    Block,
    Loop,
    If { else_block: Block, saw_else: bool },
    Function,
}

struct Frame {
    kind: Kind,
    /// Where a branch to this frame goes: the loop header, or the end of anything else.
    target: Block,
    end: Block,
    result: Option<Type>,
    /// Value-stack height at entry.
    height: usize,
    /// What is known of the cached fields where control reaches `end` (one per incoming path),
    /// and for an `if`, what was known before it.
    incoming: Vec<Known>,
    before: Option<Known>,
}

/// The values the cached fields are known to have at a point, by (address, byte).
type Known = std::collections::HashMap<(u64, bool), Value>;

/// What all of `paths` agree on: values defined before they split, which dominate where they meet.
fn agreed(paths: &[Known]) -> Known {
    let Some((first, rest)) = paths.split_first() else {
        return Known::new();
    };
    first
        .iter()
        .filter(|(k, v)| rest.iter().all(|p| p.get(k) == Some(v)))
        .map(|(&k, &v)| (k, v))
        .collect()
}

struct Translator<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    h: &'a Helpers,
    locals: Vec<Variable>,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// After an unconditional branch: code up to the matching `else`/`end` is unreachable.
    unreachable: bool,
    /// Blocks opened inside unreachable code.
    skipped: u32,
    sigs: [Option<SigRef>; 4],
    /// The fields whose values are remembered, and what is known of them here.
    cached: &'a [Cached],
    known: Known,
    /// i32 constants by value, to tell accesses with a constant (zero) base from others.
    consts: std::collections::HashMap<Value, i64>,
    /// The `Bases` regions with the parameters that hold their starts.
    bases: [(u64, u64, Value); BASES],
    /// Stores of cached fields not made yet, in order (`ARMLESS_JIT_DEFER_STORES`): a later store
    /// to the field replaces one, and they are all made before anything could see the field
    /// (`flush`).
    deferred: Vec<((u64, bool), Value)>,
    defer: bool,
}

fn val_type(t: u8) -> Option<Type> {
    match t {
        wasm::I32 => Some(I32),
        wasm::I64 => Some(I64),
        _ => None,
    }
}

fn block_type(t: u8) -> Option<Option<Type>> {
    match t {
        wasm::EMPTY => Some(None),
        t => val_type(t).map(Some),
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }

    fn leb_u(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn leb_s(&mut self) -> Option<i64> {
        let mut v = 0i64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            v |= ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    v |= -1 << shift;
                }
                return Some(v);
            }
            if shift >= 64 {
                return None;
            }
        }
    }
}

impl<'a, 'b> Translator<'a, 'b> {
    fn new(
        b: &'a mut FunctionBuilder<'b>,
        locals: &[(u32, u8)],
        h: &'a Helpers,
        cached: &'a [Cached],
        regions: &Bases,
    ) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let pc = b.block_params(entry)[0];
        let params = b.block_params(entry)[1..=BASES].to_vec();
        let bases = std::array::from_fn(|i| (regions.0[i].0, regions.0[i].1, params[i]));
        let mut vars = Vec::new();
        let v = b.declare_var(I64);
        b.def_var(v, pc);
        vars.push(v);
        for &(n, t) in locals {
            let ty = val_type(t).unwrap_or(I64);
            for _ in 0..n {
                let v = b.declare_var(ty);
                let zero = b.ins().iconst(ty, 0);
                b.def_var(v, zero);
                vars.push(v);
            }
        }
        let end = b.create_block();
        b.append_block_param(end, I32);
        Translator {
            b,
            h,
            locals: vars,
            stack: Vec::new(),
            frames: vec![Frame {
                kind: Kind::Function,
                target: end,
                end,
                result: Some(I32),
                height: 0,
                incoming: Vec::new(),
                before: None,
            }],
            unreachable: false,
            skipped: 0,
            sigs: [None; 4],
            cached,
            known: Known::new(),
            consts: std::collections::HashMap::new(),
            bases,
            deferred: Vec::new(),
            defer: defer_stores(),
        }
    }

    /// The cached field an access at `offset` with dynamic address `dynamic` reaches: the dynamic
    /// part is the constant 0 and `offset` is the field's address, accessed in its own width.
    fn cached_key(&self, dynamic: Value, offset: u64, byte: bool) -> Option<(u64, bool)> {
        let cached = self
            .cached
            .iter()
            .any(|c| c.addr == offset && c.byte == byte);
        (cached && self.consts.get(&dynamic) == Some(&0)).then_some((offset, byte))
    }

    /// Record that control reaches the end of frame `depth` from here.
    fn reaches(&mut self, depth: usize) -> Option<()> {
        let i = self.frames.len().checked_sub(depth + 1)?;
        if !matches!(self.frames[i].kind, Kind::Loop) {
            let k = self.known.clone();
            self.frames[i].incoming.push(k);
        }
        Some(())
    }

    fn pop(&mut self) -> Option<Value> {
        self.stack.pop()
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    /// The arguments a branch to `frame` passes: its result, from the top of the stack.
    fn branch_args(&self, depth: usize) -> Option<Vec<BlockArg>> {
        let f = self.frames.get(self.frames.len().checked_sub(depth + 1)?)?;
        if matches!(f.kind, Kind::Loop) || f.result.is_none() {
            return Some(Vec::new());
        }
        Some(vec![BlockArg::Value(*self.stack.last()?)])
    }

    fn target(&self, depth: usize) -> Option<Block> {
        Some(
            self.frames
                .get(self.frames.len().checked_sub(depth + 1)?)?
                .target,
        )
    }

    fn run(mut self, body: &[u8]) -> Option<()> {
        let mut r = Reader { b: body, at: 0 };
        while r.at < body.len() {
            let op = r.byte()?;
            if self.unreachable {
                self.skip(op, &mut r)?;
                continue;
            }
            self.op(op, &mut r)?;
        }
        // The function's own `end`
        self.end()?;
        Some(())
    }

    /// Unreachable code: only nesting matters, until the `else`/`end` that makes code reachable.
    fn skip(&mut self, op: u8, r: &mut Reader) -> Option<()> {
        match op {
            0x02..=0x04 => {
                r.byte()?;
                self.skipped += 1;
            },
            0x05 if self.skipped == 0 => return self.else_(),
            0x0b if self.skipped == 0 => return self.end(),
            0x0b => self.skipped -= 1,
            _ => immediates(op, r)?,
        }
        Some(())
    }

    fn else_(&mut self) -> Option<()> {
        let f = self.frames.last_mut()?;
        let Kind::If {
            else_block,
            saw_else,
        } = &mut f.kind
        else {
            return None;
        };
        let (else_block, end, result, height) = (*else_block, f.end, f.result, f.height);
        *saw_else = true;
        let before = f.before.clone().unwrap_or_default();
        if !self.unreachable {
            self.flush();
            let args: Vec<BlockArg> = match result {
                Some(_) => vec![BlockArg::Value(self.pop()?)],
                None => Vec::new(),
            };
            self.b.ins().jump(end, &args);
            self.reaches(0)?;
        }
        self.stack.truncate(height);
        self.b.switch_to_block(else_block);
        self.known = before;
        self.unreachable = false;
        Some(())
    }

    fn end(&mut self) -> Option<()> {
        let mut f = self.frames.pop()?;
        if !self.unreachable {
            self.flush();
            let args: Vec<BlockArg> = match f.result {
                Some(_) => vec![BlockArg::Value(self.pop()?)],
                None => Vec::new(),
            };
            self.b.ins().jump(f.end, &args);
            f.incoming.push(std::mem::take(&mut self.known));
        }
        if let Kind::If {
            else_block,
            saw_else: false,
        } = f.kind
        {
            // An `if` without `else` (only ever without a result): its else path falls through.
            if f.result.is_some() {
                return None;
            }
            self.b.switch_to_block(else_block);
            self.b.ins().jump(f.end, &[]);
            f.incoming.push(f.before.take().unwrap_or_default());
        }
        self.known = agreed(&f.incoming);
        self.stack.truncate(f.height);
        self.b.switch_to_block(f.end);
        self.unreachable = false;
        if let Kind::Function = f.kind {
            let v = self.b.block_params(f.end)[0];
            self.b.ins().return_(&[v]);
            return Some(());
        }
        if f.result.is_some() {
            let v = self.b.block_params(f.end)[0];
            self.push(v);
        }
        Some(())
    }

    fn open(&mut self, kind: u8, ty: u8) -> Option<()> {
        let result = block_type(ty)?;
        if kind != 0x02 {
            // A loop's header and an `if`'s arms begin new blocks: what is deferred is made here
            self.flush();
        }
        let end = self.b.create_block();
        if let Some(t) = result {
            self.b.append_block_param(end, t);
        }
        let height = self.stack.len();
        match kind {
            0x02 => self.frames.push(Frame {
                kind: Kind::Block,
                target: end,
                end,
                result,
                height,
                incoming: Vec::new(),
                before: None,
            }),
            0x03 => {
                if result.is_some() {
                    return None;
                }
                let header = self.b.create_block();
                self.b.ins().jump(header, &[]);
                self.b.switch_to_block(header);
                // Branches back to the header bring what is not known here.
                self.known.clear();
                self.frames.push(Frame {
                    kind: Kind::Loop,
                    target: header,
                    end,
                    result,
                    height,
                    incoming: Vec::new(),
                    before: None,
                });
            },
            _ => {
                let c = self.pop()?;
                let then = self.b.create_block();
                let else_block = self.b.create_block();
                self.b.ins().brif(c, then, &[], else_block, &[]);
                self.b.switch_to_block(then);
                self.frames.push(Frame {
                    kind: Kind::If {
                        else_block,
                        saw_else: false,
                    },
                    target: end,
                    end,
                    result,
                    height: self.stack.len(),
                    incoming: Vec::new(),
                    before: Some(self.known.clone()),
                });
            },
        }
        Some(())
    }

    fn sig(&mut self, i: usize) -> SigRef {
        if let Some(s) = self.sigs[i] {
            return s;
        }
        let call_conv = self.b.func.signature.call_conv;
        let mut s = cranelift_codegen::ir::Signature::new(call_conv);
        let (params, ret): (&[Type], Option<Type>) = match i {
            0 => (&[I64, I32], Some(I64)),
            1 => (&[I64, I32, I64], None),
            2 => (&[I32, I64], Some(I32)),
            _ => (&[I32, I64, I64, I64], Some(I64)),
        };
        s.params.extend(params.iter().map(|&t| AbiParam::new(t)));
        s.returns.extend(ret.map(AbiParam::new));
        let r = self.b.import_signature(s);
        self.sigs[i] = Some(r);
        r
    }

    fn call(&mut self, f: u64) -> Option<()> {
        // Helpers see the CPU in memory
        self.flush();
        let (i, addr, nargs) = match f {
            0 => (0, self.h.read, 2),
            1 => (1, self.h.write, 3),
            2 => (2, self.h.interp, 2),
            3 => (3, self.h.fp3, 4),
            _ => return None,
        };
        let sig = self.sig(i);
        let at = self.stack.len().checked_sub(nargs)?;
        let args: Vec<Value> = self.stack.split_off(at);
        let callee = self.b.ins().iconst(I64, addr as i64);
        let call = self.b.ins().call_indirect(sig, callee, &args);
        if let Some(&v) = self.b.inst_results(call).first() {
            self.push(v);
        }
        // An interpreted instruction may change any field, and `fp3` changes FPSR; reads and
        // writes of guest memory change none (a fault is only recorded, and taken after the block).
        if f >= 2 {
            self.known.clear();
        }
        Some(())
    }

    /// The host address of the field at `offset` (a constant address).
    fn field_address(&mut self, offset: u64) -> Value {
        match self.bases.iter().find(|&&(start, len, _)| offset.wrapping_sub(start) < len) {
            Some(&(start, _, base)) => self.b.ins().iadd_imm_u(base, (offset - start) as i64),
            None => self.b.ins().iconst(I64, offset as i64),
        }
    }

    /// Make the deferred stores that `hit` selects (by field address and width), in order.
    fn flush_where(&mut self, hit: impl Fn(u64, u64) -> bool) {
        if self.deferred.is_empty() {
            return;
        }
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|&((addr, byte), _)| hit(addr, if byte { 1 } else { 8 }));
        self.deferred = later;
        let flags = MemFlagsData::new().with_notrap();
        for ((addr, byte), v) in now {
            let p = self.field_address(addr);
            if byte {
                self.b.ins().istore8(flags, v, p, 0);
            } else {
                self.b.ins().store(flags, v, p, 0);
            }
        }
    }

    /// Make every deferred store: before control leaves this point of the function or a helper
    /// runs.
    fn flush(&mut self) {
        self.flush_where(|_, _| true);
    }

    /// Before an access other than a cached field's own: make the deferred stores it may see or
    /// overwrite (with a dynamic address into the CPU, any of them).
    fn flush_for(&mut self, dynamic: Value, offset: u64, width: u64) {
        if self.deferred.is_empty() {
            return;
        }
        if self.consts.get(&dynamic) == Some(&0) {
            self.flush_where(|addr, len| addr < offset + width && offset < addr + len);
        } else if !self.bases[1..]
            .iter()
            .any(|&(start, len, _)| offset.wrapping_sub(start) < len)
        {
            // Not into guest RAM nor a TLB table (`Bases`, after the CPU): it may be the CPU
            self.flush();
        }
    }

    /// The host address of an access: the dynamic i32 on the stack, zero-extended, plus `offset`.
    fn address(&mut self, offset: u64) -> Option<Value> {
        let dynamic = self.pop()?;
        // In one of the regions: its parameter plus the offset into it (the same address), which
        // a load or store takes as its immediate
        if let Some(&(start, _, base)) = self
            .bases
            .iter()
            .find(|&&(start, len, _)| offset.wrapping_sub(start) < len)
        {
            let at = if self.consts.get(&dynamic) == Some(&0) {
                base
            } else {
                let wide = self.b.ins().uextend(I64, dynamic);
                self.b.ins().iadd(base, wide)
            };
            return Some(self.b.ins().iadd_imm_u(at, (offset - start) as i64));
        }
        let wide = self.b.ins().uextend(I64, dynamic);
        Some(self.b.ins().iadd_imm_u(wide, offset as i64))
    }

    fn load(&mut self, op: u8, r: &mut Reader) -> Option<()> {
        r.leb_u()?;
        let offset = r.leb_u()?;
        let key = match op {
            0x29 => self.cached_key(*self.stack.last()?, offset, false),
            0x2d => self.cached_key(*self.stack.last()?, offset, true),
            _ => None,
        };
        if let Some(&v) = key.and_then(|k| self.known.get(&k)) {
            self.pop()?;
            self.push(v);
            return Some(());
        }
        let width = match op {
            0x29 => 8,
            0x28 | 0x35 => 4,
            0x33 => 2,
            _ => 1,
        };
        self.flush_for(*self.stack.last()?, offset, width);
        let p = self.address(offset)?;
        let flags = MemFlagsData::new().with_notrap();
        let ins = self.b.ins();
        let v = match op {
            0x28 => ins.load(I32, flags, p, 0),
            0x29 => ins.load(I64, flags, p, 0),
            0x2d => ins.uload8(I32, flags, p, 0),
            0x31 => ins.uload8(I64, flags, p, 0),
            0x33 => ins.uload16(I64, flags, p, 0),
            0x35 => ins.uload32(flags, p, 0),
            _ => return None,
        };
        if let Some(k) = key {
            self.known.insert(k, v);
        }
        self.push(v);
        Some(())
    }

    fn store(&mut self, op: u8, r: &mut Reader) -> Option<()> {
        r.leb_u()?;
        let offset = r.leb_u()?;
        let v = self.pop()?;
        let dynamic = *self.stack.last()?;
        let width = match op {
            0x37 => 8,
            0x3e => 4,
            0x3d => 2,
            _ => 1,
        };
        let key = match op {
            0x37 => self.cached_key(dynamic, offset, false),
            0x3a => self.cached_key(dynamic, offset, true),
            _ => None,
        };
        if let Some(k) = key.filter(|_| self.defer) {
            // Replaces a deferred store to the field (which nothing saw); made later
            self.deferred.retain(|&(d, _)| d != k);
            self.pop()?;
            self.known.retain(|&(addr, byte), _| {
                let len = if byte { 1 } else { 8 };
                addr + len <= offset || offset + width <= addr
            });
            let known = if k.1 { self.b.ins().band_imm_u(v, 0xff) } else { v };
            self.known.insert(k, known);
            self.deferred.push((k, v));
            return Some(());
        }
        self.flush_for(dynamic, offset, width);
        if self.consts.get(&dynamic) == Some(&0) {
            // Forget the fields this store overwrites, in part or in whole.
            self.known.retain(|&(addr, byte), _| {
                let len = if byte { 1 } else { 8 };
                addr + len <= offset || offset + width <= addr
            });
        }
        let p = self.address(offset)?;
        let flags = MemFlagsData::new().with_notrap();
        let ins = self.b.ins();
        match op {
            0x37 => ins.store(flags, v, p, 0),
            0x3a | 0x3c => ins.istore8(flags, v, p, 0),
            0x3d => ins.istore16(flags, v, p, 0),
            0x3e => ins.istore32(flags, v, p, 0),
            _ => return None,
        };
        if let Some(k) = key {
            let v = if k.1 { self.b.ins().band_imm_u(v, 0xff) } else { v };
            self.known.insert(k, v);
        }
        Some(())
    }

    fn compare(&mut self, cc: IntCC) -> Option<()> {
        let y = self.pop()?;
        let x = self.pop()?;
        let c = self.b.ins().icmp(cc, x, y);
        let v = self.b.ins().uextend(I32, c);
        self.push(v);
        Some(())
    }

    fn fcompare(&mut self, cc: FloatCC) -> Option<()> {
        let y = self.pop()?;
        let x = self.pop()?;
        let c = self.b.ins().fcmp(cc, x, y);
        let v = self.b.ins().uextend(I32, c);
        self.push(v);
        Some(())
    }

    fn eqz(&mut self) -> Option<()> {
        let x = self.pop()?;
        let c = self.b.ins().icmp_imm_u(IntCC::Equal, x, 0);
        let v = self.b.ins().uextend(I32, c);
        self.push(v);
        Some(())
    }

    fn binary(
        &mut self,
        f: impl FnOnce(&mut FunctionBuilder, Value, Value) -> Value,
    ) -> Option<()> {
        let y = self.pop()?;
        let x = self.pop()?;
        let v = f(self.b, x, y);
        self.push(v);
        Some(())
    }

    fn unary(&mut self, f: impl FnOnce(&mut FunctionBuilder, Value) -> Value) -> Option<()> {
        let x = self.pop()?;
        let v = f(self.b, x);
        self.push(v);
        Some(())
    }

    fn op(&mut self, op: u8, r: &mut Reader) -> Option<()> {
        match op {
            0x02..=0x04 => {
                let ty = r.byte()?;
                self.open(op, ty)?;
            },
            0x05 => self.else_()?,
            0x0b => self.end()?,
            0x0d => {
                let depth = r.leb_u()? as usize;
                let c = self.pop()?;
                self.flush();
                let args = self.branch_args(depth)?;
                let target = self.target(depth)?;
                let next = self.b.create_block();
                self.b.ins().brif(c, target, &args, next, &[]);
                self.reaches(depth)?;
                self.b.switch_to_block(next);
            },
            0x0e => {
                let n = r.leb_u()? as usize;
                let mut depths = Vec::with_capacity(n);
                for _ in 0..n {
                    depths.push(r.leb_u()? as usize);
                }
                let default = r.leb_u()? as usize;
                let index = self.pop()?;
                self.flush();
                let call = |t: &mut Self, depth: usize| -> Option<_> {
                    // `gen` only branches to blocks without results through a table.
                    if !t.branch_args(depth)?.is_empty() {
                        return None;
                    }
                    let block = t.target(depth)?;
                    Some(t.b.func.dfg.block_call(block, &[]))
                };
                let def = call(self, default)?;
                self.reaches(default)?;
                let mut table = Vec::with_capacity(n);
                for d in depths {
                    table.push(call(self, d)?);
                    self.reaches(d)?;
                }
                let jt = self.b.create_jump_table(JumpTableData::new(def, &table));
                self.b.ins().br_table(index, jt);
                self.unreachable = true;
            },
            0x0f => {
                let v = self.pop()?;
                self.flush();
                self.b.ins().return_(&[v]);
                self.unreachable = true;
            },
            0x10 => {
                let f = r.leb_u()?;
                self.call(f)?;
            },
            0x1a => {
                self.pop()?;
            },
            0x1b => {
                let c = self.pop()?;
                let y = self.pop()?;
                let x = self.pop()?;
                let v = self.b.ins().select(c, x, y);
                self.push(v);
            },
            0x20 => {
                let l = *self.locals.get(r.leb_u()? as usize)?;
                let v = self.b.use_var(l);
                self.push(v);
            },
            0x21 | 0x22 => {
                let l = *self.locals.get(r.leb_u()? as usize)?;
                let v = if op == 0x21 { self.pop()? } else { *self.stack.last()? };
                self.b.def_var(l, v);
            },
            0x28..=0x35 => self.load(op, r)?,
            0x36..=0x3e => self.store(op, r)?,
            0x41 => {
                let v = r.leb_s()?;
                let c = self.b.ins().iconst(I32, v as i32 as i64);
                self.consts.insert(c, v as i32 as i64);
                self.push(c);
            },
            0x42 => {
                let v = r.leb_s()?;
                let c = self.b.ins().iconst(I64, v);
                self.push(c);
            },
            0x45 | 0x50 => self.eqz()?,
            0x46 | 0x51 => self.compare(IntCC::Equal)?,
            0x52 => self.compare(IntCC::NotEqual)?,
            0x54 => self.compare(IntCC::UnsignedLessThan)?,
            0x4d | 0x58 => self.compare(IntCC::UnsignedLessThanOrEqual)?,
            0x53 => self.compare(IntCC::SignedLessThan)?,
            0x55 => self.compare(IntCC::SignedGreaterThan)?,
            0x56 => self.compare(IntCC::UnsignedGreaterThan)?,
            0x59 => self.compare(IntCC::SignedGreaterThanOrEqual)?,
            0x5a => self.compare(IntCC::UnsignedGreaterThanOrEqual)?,
            0x67 | 0x79 => self.unary(|b, x| b.ins().clz(x))?,
            0x6a | 0x7c => self.binary(|b, x, y| b.ins().iadd(x, y))?,
            0x6b | 0x7d => self.binary(|b, x, y| b.ins().isub(x, y))?,
            0x7e => self.binary(|b, x, y| b.ins().imul(x, y))?,
            // `gen` branches around a zero divisor and INT_MIN / -1, as A64 defines them
            0x7f => self.binary(|b, x, y| b.ins().sdiv(x, y))?,
            0x80 => self.binary(|b, x, y| b.ins().udiv(x, y))?,
            0x71 | 0x83 => self.binary(|b, x, y| b.ins().band(x, y))?,
            0x72 | 0x84 => self.binary(|b, x, y| b.ins().bor(x, y))?,
            0x85 => self.binary(|b, x, y| b.ins().bxor(x, y))?,
            // Shift counts are taken modulo the width, in wasm and in Cranelift alike
            0x74 | 0x86 => self.binary(|b, x, y| b.ins().ishl(x, y))?,
            0x76 | 0x88 => self.binary(|b, x, y| b.ins().ushr(x, y))?,
            0x87 => self.binary(|b, x, y| b.ins().sshr(x, y))?,
            0x8a => self.binary(|b, x, y| b.ins().rotr(x, y))?,
            0xa7 => self.unary(|b, x| b.ins().ireduce(I32, x))?,
            0xad => self.unary(|b, x| b.ins().uextend(I64, x))?,
            0xc2 => self.unary(|b, x| {
                let n = b.ins().ireduce(I8, x);
                b.ins().sextend(I64, n)
            })?,
            0xc3 => self.unary(|b, x| {
                let n = b.ins().ireduce(I16, x);
                b.ins().sextend(I64, n)
            })?,
            0xc4 => self.unary(|b, x| {
                let n = b.ins().ireduce(I32, x);
                b.ins().sextend(I64, n)
            })?,
            // Floating point: comparisons, arithmetic, conversions and reinterpretation
            0x5b | 0x61 => self.fcompare(FloatCC::Equal)?,
            0x5c | 0x62 => self.fcompare(FloatCC::NotEqual)?,
            0x5d | 0x63 => self.fcompare(FloatCC::LessThan)?,
            0x60 | 0x66 => self.fcompare(FloatCC::GreaterThanOrEqual)?,
            0x8c | 0x9a => self.unary(|b, x| b.ins().fneg(x))?,
            0x8f | 0x9d => self.unary(|b, x| b.ins().trunc(x))?,
            0x91 => self.unary(|b, x| b.ins().sqrt(x))?,
            0x92 | 0xa0 => self.binary(|b, x, y| b.ins().fadd(x, y))?,
            0x93 | 0xa1 => self.binary(|b, x, y| b.ins().fsub(x, y))?,
            0xa2 => self.binary(|b, x, y| b.ins().fmul(x, y))?,
            0x95 | 0xa3 => self.binary(|b, x, y| b.ins().fdiv(x, y))?,
            0xb4 => self.unary(|b, x| b.ins().fcvt_from_sint(F32, x))?,
            0xb5 => self.unary(|b, x| b.ins().fcvt_from_uint(F32, x))?,
            0xb6 => self.unary(|b, x| b.ins().fdemote(F32, x))?,
            0xb9 => self.unary(|b, x| b.ins().fcvt_from_sint(F64, x))?,
            0xba => self.unary(|b, x| b.ins().fcvt_from_uint(F64, x))?,
            0xbb => self.unary(|b, x| b.ins().fpromote(F64, x))?,
            0xbc => self.unary(|b, x| b.ins().bitcast(I32, MemFlagsData::new(), x))?,
            0xbd => self.unary(|b, x| b.ins().bitcast(I64, MemFlagsData::new(), x))?,
            0xbe => self.unary(|b, x| b.ins().bitcast(F32, MemFlagsData::new(), x))?,
            0xbf => self.unary(|b, x| b.ins().bitcast(F64, MemFlagsData::new(), x))?,
            0xfc => {
                // Saturating float to integer: what it saturates to never matters (the generated
                // code only converts values in range), but unlike the trapping forms it can't
                // end the process
                // to i64 (4 to 7; `gen` makes no i32 ones)
                match r.leb_u()? {
                    4 | 6 => self.unary(|b, x| b.ins().fcvt_to_sint_sat(I64, x))?,
                    5 | 7 => self.unary(|b, x| b.ins().fcvt_to_uint_sat(I64, x))?,
                    super::wasm::FMA_F64 => {
                        let z = self.pop()?;
                        let y = self.pop()?;
                        let x = self.pop()?;
                        let v = self.b.ins().fma(x, y, z);
                        self.push(v);
                    },
                    _ => return None,
                }
            },
            0xfe => {
                // atomic.fence
                if r.leb_u()? != 3 || r.byte()? != 0 {
                    return None;
                }
                self.b.ins().fence();
            },
            _ => return None,
        }
        Some(())
    }
}

/// Step over the immediates of `op` (in unreachable code).
fn immediates(op: u8, r: &mut Reader) -> Option<()> {
    match op {
        0x05 | 0x0b | 0x0f | 0x1a | 0x1b | 0x45..=0xc4 => {},
        0x0c | 0x0d | 0x10 | 0x20..=0x22 => {
            r.leb_u()?;
        },
        0x0e => {
            let n = r.leb_u()?;
            for _ in 0..=n {
                r.leb_u()?;
            }
        },
        0x28..=0x3e => {
            r.leb_u()?;
            r.leb_u()?;
        },
        0x41 | 0x42 => {
            r.leb_s()?;
        },
        0xfc => {
            r.leb_u()?;
        },
        0xfe => {
            r.leb_u()?;
            r.byte()?;
        },
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests;
