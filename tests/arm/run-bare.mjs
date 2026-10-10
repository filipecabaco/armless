#!/usr/bin/env node
// Run a bare-metal AArch64 binary on armless.wasm directly (no v86 devices),
// with a minimal 16550 on stdout. Used to run the self-test guest and the
// differential tests through WebAssembly, with or without the JIT.
//
//   node tests/arm/run-bare.mjs BINARY [--jit-threshold N] [--no-jit] [--stats]
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { JIT_SLOT_BASE, JIT_SLOTS, UART_BASE } from "../../src/arm/board.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const args = process.argv.slice(2);
const flag = name => { const i = args.indexOf(name); return i >= 0 ? (args.splice(i, 1), true) : false; };
const opt = (name, def) => { const i = args.indexOf(name); return i >= 0 ? args.splice(i, 2)[1] : def; };
const no_jit = flag("--no-jit");
const sync = flag("--jit-sync");   // compile every block before it first runs
const stats = flag("--stats");
const threshold = Number(opt("--jit-threshold", "64"));
const memory_mb = Number(opt("-m", "64"));
const [binary] = args;

const table = new WebAssembly.Table({ element: "anyfunc", initial: JIT_SLOT_BASE + JIT_SLOTS });
let exports, memory;
const out = [];
const t0 = performance.now();
const jit_imports = {};

const env = {
    "__indirect_function_table": table,
    "microtick": () => performance.now(),
    "wall_clock_ms": () => Date.now(),
    "mmio_read": (addr, size) => {
        addr >>>= 0;
        if(addr >= UART_BASE && addr < UART_BASE + 8) return (addr - UART_BASE) === 5 ? 0x60 : 0;
        return -1;
    },
    "mmio_write": (addr, size, value) => {
        addr >>>= 0;
        if(addr === UART_BASE) out.push(value & 0xFF);
    },
    "jit_compile": (index, ptr, len) => {
        const code = new Uint8Array(memory.buffer, ptr >>> 0, len >>> 0).slice();
        const p = WebAssembly.instantiate(code, { "e": jit_imports }).then(r => {
            table.set(index, r.instance.exports["f"]);
            exports["armless_jit_ready"](index);
            pending.delete(p);
        }, e => { console.error("JIT compile failed:", e); process.exit(9); });
        pending.add(p);
        return 1;
    },
};

const { instance } = await WebAssembly.instantiate(fs.readFileSync(path.join(root, "build/armless.wasm")), { env });
exports = instance.exports;
memory = exports.memory;
Object.assign(jit_imports, {
    "m": memory,
    "armless_jit_read": exports.armless_jit_read,
    "armless_jit_write": exports.armless_jit_write,
    "armless_jit_interp": exports.armless_jit_interp,
});

if(exports.armless_init(memory_mb << 20) !== 0) throw new Error("init failed");
exports.armless_jit_config(no_jit ? 0 : 1, threshold);
exports.armless_jit_sync(sync ? 1 : 0);
const pending = new Set();
const bin = fs.readFileSync(binary);
const ptr = exports.armless_alloc(bin.length);
new Uint8Array(memory.buffer, ptr, bin.length).set(bin);
if(exports.armless_boot_raw(ptr, bin.length, 0x200000) !== 0) throw new Error("boot failed");

const flush = () => { if(out.length) { process.stdout.write(Buffer.from(out)); out.length = 0; } };
let code = 0;
for(;;)
{
    const r = exports.armless_run(200000);
    flush();
    if(r === 0)
    {
        if(pending.size) await Promise.all([...pending]);   // let compiles finish
        else await new Promise(res => setTimeout(res, 0));
        continue;
    }
    if(r === 1)
    {
        const wake = exports.armless_wake_ms();
        await new Promise(res => setTimeout(res, Math.max(0, wake - performance.now())));
        continue;
    }
    code = r === 2 ? 0 : r === 4 ? 1 : 5;
    if(r === 5) console.error(`stopped at pc=0x${exports.armless_pc().toString(16)}`);
    break;
}
if(stats)
{
    const secs = (performance.now() - t0) / 1000;
    const n = exports.armless_insn_count();
    console.error(`run-bare: ${n} instructions in ${secs.toFixed(2)}s (${(n / secs / 1e6).toFixed(1)} MIPS), ` +
        `jit compiled ${exports.armless_jit_stat(0)} invalidated ${exports.armless_jit_stat(1)} runs ${exports.armless_jit_stat(2)}`);
}
process.exit(code);
