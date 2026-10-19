#!/usr/bin/env node
// Run a bare-metal AArch64 binary on armless.wasm through armless.js, with the
// serial port on stdout. Runs the self-test guest and the differential tests
// through WebAssembly, with or without the JIT.
//
//   node tests/js/run-bare.mjs BINARY [-m MB] [--jit-threshold N] [--jit-sync] [--no-jit] [--stats] [--cpus N]
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));

const args = process.argv.slice(2);
const flag = (name) => { const i = args.indexOf(name); return i >= 0 ? (args.splice(i, 1), true) : false; };
const opt = (name, def) => { const i = args.indexOf(name); return i >= 0 ? args.splice(i, 2)[1] : def; };
const no_jit = flag("--no-jit");
const jit_sync = flag("--jit-sync");
const stats = flag("--stats");
const jit_threshold = Number(opt("--jit-threshold", "64"));
const memory_mb = Number(opt("-m", "64"));
const cpus = Number(opt("--cpus", "1"));
const [binary] = args;

const t0 = performance.now();
const vm = new Armless({
  wasm: path.join(root, "build/armless.wasm"),
  wasm_smp: path.join(root, "build/armless-smp.wasm"),
  memory_mb,
  cpus,
  console: false,
  jit: !no_jit,
  jit_threshold,
  jit_sync,
  boot: { raw: binary, addr: 0x200000 },
});
vm.on("serial", (bytes) => process.stdout.write(bytes));
const stop = await new Promise((resolve, reject) => {
  vm.on("stop", resolve);
  vm.ready.catch(reject);
});
if (stats) {
  const secs = (performance.now() - t0) / 1000;
  const n = vm.instruction_count;
  const s = (i) => vm.exports.armless_jit_stat(i);
  console.error(`run-bare: ${n} instructions in ${secs.toFixed(2)}s (${(n / secs / 1e6).toFixed(1)} MIPS), ` +
    `jit compiled ${s(0)} invalidated ${s(1)} runs ${s(2)}`);
}
if (stop.reason === "fault") console.error(`stopped at pc=0x${stop.pc.toString(16)}`);
process.stdout.write("", () => process.exit({ poweroff: 0, halt: 1 }[stop.reason] ?? 5));
