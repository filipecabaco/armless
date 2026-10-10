#!/usr/bin/env node
// Boot an arm64 kernel through v86's public V86 API with arch: "arm64".
//
//   node tests/arm/boot.mjs KERNEL INITRD [--expect TEXT] [--cmdline ...] [--memory MB]
//
// KERNEL may be a raw Image or gzip/zboot compressed (vmlinuz).
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
// V86_MODULE=build/libv86.mjs tests the Closure-compiled bundle snowglobe ships.
const { V86 } = await import(path.resolve(root, process.env.V86_MODULE || "src/main.js"));
const args = process.argv.slice(2);
const opt = (name, def) => {
    const i = args.indexOf(name);
    return i >= 0 ? args.splice(i, 2)[1] : def;
};
const expect = opt("--expect", "");
const cmdline = opt("--cmdline", "console=ttyS0 rdinit=/init");
const memory = Number(opt("--memory", "512"));
const [kernel, initrd] = args;

const emulator = new V86({
    arch: "arm64",
    disable_jit: process.env.ARMLESS_JIT === "0",
    cpu_count: Number(process.env.ARMLESS_CPUS || 1),
    jit_threshold: Number(process.env.ARMLESS_JIT_THRESHOLD || 0),
    wasm_path: path.join(root, "build/armless.wasm"),
    memory_size: memory * 1024 * 1024,
    bzimage: { url: kernel },
    initrd: initrd ? { url: initrd } : undefined,
    cmdline,
    autostart: true,
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
});

const start = Date.now();
let out = "";
emulator.add_listener("serial0-output-byte", byte => {
    const c = String.fromCharCode(byte);
    out += c;
    process.stdout.write(c);
});
emulator.add_listener("cpu-event-halt", () => {
    const secs = ((Date.now() - start) / 1000).toFixed(1);
    console.log(`\n[boot.mjs] guest powered off after ${secs}s`);
    if(process.env.ARMLESS_STATS)
    {
        const e = emulator.v86.arm_cpu.exports;
        const insns = Number(e["armless_insn_count"]());
        console.log(`[boot.mjs] ${(insns / 1e6).toFixed(0)}M instructions, ` +
            `${(insns / 1e6 / secs).toFixed(1)} MIPS; jit: ${e["armless_jit_stat"](0)} compiled, ` +
            `${e["armless_jit_stat"](1)} invalidated, ${e["armless_jit_stat"](2)} block runs, ${e["armless_jit_stat"](3)} flushes, ${e["armless_jit_stat"](4)} pages written; ${(e["armless_jit_stat"](5)/1e6).toFixed(1)}M insns in blocks, ${(e["armless_jit_stat"](6)/1e6).toFixed(1)}M via interp helper`);
        if(e["armless_smp_stat"])
        {
            console.log(`[boot.mjs] smp: ${e["armless_smp_stat"](0)} broadcast TLBIs, ${e["armless_smp_stat"](1)} DSB waits, ` +
                `${e["armless_smp_stat"](2)} IC page invalidations, ${e["armless_smp_stat"](3)} kicks (CPU 0 figures above)`);
        }
    }
    emulator.destroy();
    const ok = !expect || out.includes(expect);
    console.log(ok ? "[boot.mjs] OK" : `[boot.mjs] FAIL: did not see "${expect}"`);
    process.exit(ok ? 0 : 1);
});
setTimeout(() => {
    console.log("\n[boot.mjs] timeout");
    process.exit(2);
}, 600_000).unref();
