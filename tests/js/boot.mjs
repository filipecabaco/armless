#!/usr/bin/env node
// Boot an arm64 kernel (+ initrd) through armless.js; the serial port goes to stdout.
// Exits 0 when the guest powers off having printed --expect TEXT.
//
//   node tests/js/boot.mjs KERNEL INITRD [--expect TEXT] [--cmdline ...] [--memory MB] [--cpus N]
//
// KERNEL may be a raw Image, gzip, zstd or EFI zboot. ARMLESS_JIT=0 disables the JIT.
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));

const args = process.argv.slice(2);
const opt = (name, def) => { const i = args.indexOf(name); return i >= 0 ? args.splice(i, 2)[1] : def; };
const expect = opt("--expect", "");
const cmdline = opt("--cmdline", "console=ttyS0 rdinit=/init");
const memory_mb = Number(opt("--memory", "512"));
const cpus = Number(opt("--cpus", process.env.ARMLESS_CPUS || "1"));
const [kernel, initrd] = args;

const t0 = performance.now();
const vm = new Armless({
  wasm: path.join(root, "build/armless.wasm"),
  wasm_smp: path.join(root, "build/armless-smp.wasm"),
  memory_mb,
  cpus,
  jit: process.env.ARMLESS_JIT !== "0",
  boot: { kernel, initrd, cmdline },
});
let serial = "";
vm.on("serial", (bytes) => {
  const s = new TextDecoder().decode(bytes);
  serial += s;
  process.stdout.write(s);
});
const stop = await new Promise((resolve, reject) => {
  vm.on("stop", resolve);
  vm.ready.catch(reject);
});
const secs = (performance.now() - t0) / 1000;
console.error(`\nboot.mjs: ${stop.reason} after ${secs.toFixed(1)}s, ${(vm.instruction_count / 1e6).toFixed(0)}M instructions`);
const ok = stop.reason === "poweroff" && serial.includes(expect);
process.stdout.write("", () => process.exit(ok ? 0 : 1));
