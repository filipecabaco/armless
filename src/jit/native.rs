//! The native JIT: the wasm function `gen` writes for a page, compiled to host code with
//! Cranelift instead of handed to a browser.
//!
//! `gen` uses a small part of wasm (i32/i64 integers, structured control flow, locals, three
//! helper calls), so this translates exactly that, from the function body's bytes. Natively the
//! static offset of every memory access is a host address (`jit_layout_el` gives host addresses
//! here) and the dynamic part on the stack (a page offset into RAM, a TLB index) is added to it
//! zero-extended, as wasm adds it to its memory's base. The code `gen` writes never accesses
//! memory out of bounds, so there are no bounds checks.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I16, I32, I64, I8};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, InstBuilder, JumpTableData, MemFlagsData, SigRef, Type, Value,
};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use super::wasm;

/// Code compiled into one module before it is dropped and the JIT starts over (`full`).
const CODE_LIMIT: usize = 64 << 20;

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

/// The helpers blocks call, as host function addresses (see `gen::imports`).
#[derive(Clone, Copy)]
pub struct Helpers {
    /// (va: i64, size: i32) -> i64
    pub read: usize,
    /// (va: i64, size: i32, value: i64)
    pub write: usize,
    /// (insn: i32, pc: i64) -> i32
    pub interp: usize,
}

/// A request to a CPU's compiler thread.
enum Job {
    Compile {
        generation: u64,
        slot: u32,
        body: Vec<u8>,
        cached: Vec<Cached>,
    },
    /// Free the module and compile into a new one: nothing compiled before is reachable any more.
    Reset,
}

/// A compiled slot: the address of its function, or 0 if it could not be compiled.
pub struct Done {
    pub generation: u64,
    pub slot: u32,
    pub function: usize,
}

#[derive(Default)]
struct Shared {
    /// Results sent and not yet taken, so the CPU can check for them with one load.
    pending: AtomicUsize,
    /// Jobs submitted and not yet answered.
    in_flight: AtomicUsize,
    code_bytes: AtomicUsize,
    compiles: AtomicU64,
    compile_ns: AtomicU64,
}

/// One CPU's native JIT: a compiler thread of its own owns the generated code, so the CPU goes on
/// interpreting while a page compiles, and takes the result when it is there (as in the browser,
/// where the host compiles asynchronously too).
pub struct Native {
    jobs: Option<Sender<Job>>,
    done: Receiver<Done>,
    shared: Arc<Shared>,
    /// Results of an older generation point into a module that has been freed.
    pub generation: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Whether generated code can be made visible to every core. On Linux arm64 Cranelift does that
/// with membarrier(2) and panics if the kernel refuses it, which a sandbox may (seccomp, gVisor):
/// there, ask first and stay with the interpreter if neither command it uses is available.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
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
        if !can_publish_code() {
            return None;
        }
        let (jobs, job_rx) = channel::<Job>();
        let (done_tx, done) = channel::<Done>();
        let (ready_tx, ready_rx) = channel::<bool>();
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("armless-jit".into())
            .spawn(move || {
                let Some(mut compiler) = Compiler::new() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                for job in job_rx {
                    match job {
                        Job::Compile {
                            generation,
                            slot,
                            body,
                            cached,
                        } => {
                            let started = std::time::Instant::now();
                            let function = compiler
                                .compile(&body, locals, &helpers, &cached)
                                .map_or(0, |f| f as usize);
                            thread_shared.compiles.fetch_add(1, Ordering::Relaxed);
                            thread_shared
                                .compile_ns
                                .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            thread_shared
                                .code_bytes
                                .store(compiler.code_bytes, Ordering::Relaxed);
                            if done_tx
                                .send(Done {
                                    generation,
                                    slot,
                                    function,
                                })
                                .is_err()
                            {
                                return;
                            }
                            thread_shared.pending.fetch_add(1, Ordering::Release);
                            thread_shared.in_flight.fetch_sub(1, Ordering::Release);
                        },
                        Job::Reset => {
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
        Some(Native {
            jobs: Some(jobs),
            done,
            shared,
            generation: 0,
            thread: Some(thread),
        })
    }

    /// Compile `body` (a `gen` function) for `slot`, remembering the `cached` fields' values; the
    /// result comes back through `take_done`.
    pub fn submit(&mut self, slot: u32, body: Vec<u8>, cached: Vec<Cached>) -> bool {
        let generation = self.generation;
        self.shared.in_flight.fetch_add(1, Ordering::Relaxed);
        let sent = self.jobs.as_ref().is_some_and(|j| {
            j.send(Job::Compile {
                generation,
                slot,
                body,
                cached,
            })
            .is_ok()
        });
        if !sent {
            self.shared.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        sent
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
        self.shared.code_bytes.load(Ordering::Relaxed) >= CODE_LIMIT
    }

    /// Start a new module. The caller has flushed every slot: no code of the old one is reachable,
    /// and results still to come from it are of an older generation.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.shared.code_bytes.store(0, Ordering::Relaxed);
        if let Some(j) = &self.jobs {
            let _ = j.send(Job::Reset);
        }
    }

    /// Statistics: (functions compiled, nanoseconds spent compiling them).
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

/// The compiler thread's state: the module the generated code lives in.
struct Compiler {
    /// Freed by hand in `drop`: its code must not outlive the slots that point into it.
    module: std::mem::ManuallyDrop<JITModule>,
    ctx: Context,
    fctx: FunctionBuilderContext,
    code_bytes: usize,
    functions: u32,
}

impl Compiler {
    fn new() -> Option<Compiler> {
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").ok()?;
        // Checking the IR of every function takes about a third of the compile time; the block
        // tests and fuzzers keep the translation honest.
        flags.set("enable_verifier", "false").ok()?;
        flags.set("use_colocated_libcalls", "false").ok()?;
        flags.set("is_pic", "false").ok()?;
        let isa = cranelift_native::builder()
            .ok()?
            .finish(settings::Flags::new(flags))
            .ok()?;
        let module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let ctx = module.make_context();
        Some(Compiler {
            module: std::mem::ManuallyDrop::new(module),
            ctx,
            fctx: FunctionBuilderContext::new(),
            code_bytes: 0,
            functions: 0,
        })
    }

    /// Compile the body of a `gen` function, `(pc: i64) -> i32` with `locals` after the
    /// parameter. None if it uses something this does not translate.
    fn compile(
        &mut self,
        body: &[u8],
        locals: &[(u32, u8)],
        h: &Helpers,
        cached: &[Cached],
    ) -> Option<*const u8> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(I64));
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
            let ok = Translator::new(&mut b, locals, h, cached)
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
        let defined = self.module.define_function(id, &mut self.ctx);
        if let Some(code) = self.ctx.compiled_code() {
            self.code_bytes += code.code_buffer().len();
        }
        self.module.clear_context(&mut self.ctx);
        defined.ok()?;
        self.module.finalize_definitions().ok()?;
        Some(self.module.get_finalized_function(id))
    }
}

impl Drop for Compiler {
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
    sigs: [Option<SigRef>; 3],
    /// The fields whose values are remembered, and what is known of them here.
    cached: &'a [Cached],
    known: Known,
    /// i32 constants by value, to tell accesses with a constant (zero) base from others.
    consts: std::collections::HashMap<Value, i64>,
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
    ) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let pc = b.block_params(entry)[0];
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
            sigs: [None; 3],
            cached,
            known: Known::new(),
            consts: std::collections::HashMap::new(),
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
            _ => (&[I32, I64], Some(I32)),
        };
        s.params.extend(params.iter().map(|&t| AbiParam::new(t)));
        s.returns.extend(ret.map(AbiParam::new));
        let r = self.b.import_signature(s);
        self.sigs[i] = Some(r);
        r
    }

    fn call(&mut self, f: u64) -> Option<()> {
        let (i, addr, nargs) = match f {
            0 => (0, self.h.read, 2),
            1 => (1, self.h.write, 3),
            2 => (2, self.h.interp, 2),
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
        // An interpreted instruction may change any field; reads and writes of guest memory
        // change none (a fault is only recorded, and taken after the block).
        if f == 2 {
            self.known.clear();
        }
        Some(())
    }

    /// The host address of an access: the dynamic i32 on the stack, zero-extended, plus `offset`.
    fn address(&mut self, offset: u64) -> Option<Value> {
        let dynamic = self.pop()?;
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
        let p = self.address(offset)?;
        let flags = MemFlagsData::new().with_notrap();
        let ins = self.b.ins();
        let v = match op {
            0x28 => ins.load(I32, flags, p, 0),
            0x29 => ins.load(I64, flags, p, 0),
            0x2d => ins.uload8(I32, flags, p, 0),
            0x31 => ins.uload8(I64, flags, p, 0),
            0x30 => ins.sload8(I64, flags, p, 0),
            0x33 => ins.uload16(I64, flags, p, 0),
            0x32 => ins.sload16(I64, flags, p, 0),
            0x35 => ins.uload32(flags, p, 0),
            0x34 => ins.sload32(flags, p, 0),
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
            0x36 | 0x3e => 4,
            0x3b | 0x3d => 2,
            _ => 1,
        };
        let key = match op {
            0x37 => self.cached_key(dynamic, offset, false),
            0x3a => self.cached_key(dynamic, offset, true),
            _ => None,
        };
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
            0x36 | 0x37 => ins.store(flags, v, p, 0),
            0x3a | 0x3c => ins.istore8(flags, v, p, 0),
            0x3b | 0x3d => ins.istore16(flags, v, p, 0),
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
            0x0c => {
                let depth = r.leb_u()? as usize;
                let args = self.branch_args(depth)?;
                let target = self.target(depth)?;
                self.b.ins().jump(target, &args);
                self.reaches(depth)?;
                self.unreachable = true;
            },
            0x0d => {
                let depth = r.leb_u()? as usize;
                let c = self.pop()?;
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
            0x47 | 0x52 => self.compare(IntCC::NotEqual)?,
            0x49 | 0x54 => self.compare(IntCC::UnsignedLessThan)?,
            0x4d | 0x58 => self.compare(IntCC::UnsignedLessThanOrEqual)?,
            0x53 => self.compare(IntCC::SignedLessThan)?,
            0x55 => self.compare(IntCC::SignedGreaterThan)?,
            0x56 => self.compare(IntCC::UnsignedGreaterThan)?,
            0x57 => self.compare(IntCC::SignedLessThanOrEqual)?,
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
            0x73 | 0x85 => self.binary(|b, x, y| b.ins().bxor(x, y))?,
            // Shift counts are taken modulo the width, in wasm and in Cranelift alike
            0x74 | 0x86 => self.binary(|b, x, y| b.ins().ishl(x, y))?,
            0x76 | 0x88 => self.binary(|b, x, y| b.ins().ushr(x, y))?,
            0x87 => self.binary(|b, x, y| b.ins().sshr(x, y))?,
            0x89 => self.binary(|b, x, y| b.ins().rotl(x, y))?,
            0x8a => self.binary(|b, x, y| b.ins().rotr(x, y))?,
            0xa7 => self.unary(|b, x| b.ins().ireduce(I32, x))?,
            0xac => self.unary(|b, x| b.ins().sextend(I64, x))?,
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
        0xfe => {
            r.leb_u()?;
            r.byte()?;
        },
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use crate::board::Host;
    use crate::cpu::Cpu;

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

    const RAM: usize = 2 << 20;
    const CODE: u64 = 0x1000;
    const EXIT: u64 = 0x5000;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Non-branch encoding groups, so that every block ends in its own final branch.
    const BODY: &[(u32, u32)] = &[
        (0x1c00_0000, 0x1000_0000), // data processing (immediate)
        (0x0e00_0000, 0x0a00_0000), // data processing (register)
        (0x0a00_0000, 0x0800_0000), // loads and stores
        (0x3b00_0000, 0x3900_0000), // LDR/STR unsigned offset
        (0x3b20_0000, 0x3800_0000), // LDUR/STUR, pre/post, LDTR
        (0x3a00_0000, 0x2800_0000), // LDP/STP
        (0x0e00_0000, 0x0e00_0000), // SIMD/FP
    ];

    /// Everything a block can change: registers, flags, exception state and RAM.
    fn state(c: &Cpu<NoDevices>) -> impl PartialEq + std::fmt::Debug {
        (
            c.x,
            c.pc,
            (c.sp_el0, c.sp_el1, c.el, c.spsel, c.daif),
            (c.n, c.z, c.c, c.v),
            (c.sys.esr, c.sys.far, c.sys.elr, c.sys.spsr),
            c.vreg,
        )
    }

    /// Random blocks of random instructions from random states, run by the interpreter and as
    /// native code: registers, flags, exception state and RAM must match.
    #[test]
    fn compiled_blocks_do_what_the_interpreter_does() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let mut interp = Cpu::new(RAM, NoDevices);
        let mut jit = Cpu::new(RAM, NoDevices);
        if !jit.enable_native_jit() {
            return; // no Cranelift backend for this host
        }
        jit.jit.threshold = 1;
        let mut compared = 0;
        for case in 0..3000 {
            let k = 1 + rng.below(12) as usize;
            let start_pc = CODE + rng.below((1024 - k - 1) as u64) * 4;
            let mut mem = vec![0u8; RAM];
            for b in mem[0x10000..0x30000].iter_mut() {
                *b = rng.next() as u8;
            }
            let mut words: Vec<u32> = (0..k)
                .map(|_| loop {
                    let (mask, value) = BODY[rng.below(BODY.len() as u64) as usize];
                    let i = (rng.next() as u32 & !mask) | value;
                    if (i >> 26) & 7 != 0b101 {
                        break i;
                    }
                })
                .collect();
            let branch_at = start_pc + 4 * k as u64;
            words.push(0x1400_0000 | (((EXIT as i64 - branch_at as i64) / 4) as u32 & 0x3ff_ffff));
            for (i, w) in words.iter().enumerate() {
                let a = start_pc as usize + 4 * i;
                mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
            }
            mem[EXIT as usize..EXIT as usize + 4].copy_from_slice(&0x1400_0000u32.to_le_bytes());
            let regs: [u64; 31] = std::array::from_fn(|_| match rng.below(4) {
                0 | 1 => 0x10000 + rng.below(0x20000),
                2 => rng.below(64),
                _ => rng.next(),
            });
            let el = if rng.below(5) == 0 { 0 } else { 1 };
            let (sp0, sp1) = (0x10000 + rng.below(0x20000), 0x10000 + rng.below(0x20000));
            let flags = rng.next();
            let vregs: [u128; 32] =
                std::array::from_fn(|_| ((rng.next() as u128) << 64) | rng.next() as u128);
            let load = |c: &mut Cpu<NoDevices>, drop_code: bool| {
                c.mem[..].copy_from_slice(&mem);
                if drop_code {
                    c.jit.invalidate_page((CODE >> 12) as usize);
                }
                c.x = regs;
                c.pc = start_pc;
                (c.sp_el0, c.sp_el1) = (sp0, sp1);
                (c.n, c.z, c.c, c.v) = (
                    flags & 1 != 0,
                    flags & 2 != 0,
                    flags & 4 != 0,
                    flags & 8 != 0,
                );
                c.el = el;
                c.spsel = el == 1;
                c.daif = 0x3c0;
                (c.sys.esr, c.sys.far, c.sys.elr, c.sys.spsr) = (0, 0, 0, 0);
                c.vreg = vregs;
                c.set_excl(None);
                // Entered through a branch, as a block is
                c.jit_entry = true;
            };
            load(&mut interp, true);
            let block = start_pc..branch_at + 4;
            for _ in 0..=k {
                let _ = interp.step();
                if !block.contains(&interp.pc) {
                    break;
                }
            }
            for warm in 0..3 {
                load(&mut jit, warm == 0);
                jit.run(1);
                jit.wait_native_jit();
            }
            load(&mut jit, false);
            let runs = jit.jit.block_runs;
            jit.run(1);
            if jit.jit.block_runs == runs {
                continue; // e.g. a block that stores into its own page is never kept compiled
            }
            compared += 1;
            assert_eq!(
                state(&jit),
                state(&interp),
                "case {case}: block {words:08x?}"
            );
            assert!(
                jit.mem[..] == interp.mem[..],
                "case {case}: RAM differs, block {words:08x?}"
            );
        }
        assert!(compared > 2900, "only {compared} blocks ran as native code");
    }
}
