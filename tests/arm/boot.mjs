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
    emulator.destroy();
    const ok = !expect || out.includes(expect);
    console.log(ok ? "[boot.mjs] OK" : `[boot.mjs] FAIL: did not see "${expect}"`);
    process.exit(ok ? 0 : 1);
});
setTimeout(() => {
    console.log("\n[boot.mjs] timeout");
    process.exit(2);
}, 600_000).unref();
