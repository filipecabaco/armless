#!/usr/bin/env node
// Boots the landing page's machine as the page does (site/static/demo: armless from its release,
// the kernel and the initramfs) and checks what a visitor would see: the banner with the CPU
// count, a shell prompt, nproc, trench drawing frames on every CPU, ctrl+c handing the shell
// back, and the shell still working after.
//
//   node site/test/guest.mjs [DEMO_DIR] [CPUS]
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.resolve(process.argv[2] || path.join(here, "../static/demo"));
const cpus = Number(process.argv[3] || 4);
const config = JSON.parse(fs.readFileSync(path.join(dir, "config.json"), "utf8"));
const { Armless } = createRequire(import.meta.url)(path.join(dir, "armless.js"));

const t0 = performance.now();
const secs = () => ((performance.now() - t0) / 1000).toFixed(1) + "s";
const vm = new Armless({
  wasm: path.join(dir, "armless.wasm"),
  wasm_smp: path.join(dir, "armless-smp.wasm"),
  memory_mb: config.memory_mb,
  cpus,
  boot: { kernel: path.join(dir, "Image.gz"), initrd: path.join(dir, "initrd.cpio"), cmdline: config.cmdline },
});

const decoder = new TextDecoder();
let out = "";
vm.on("console", (bytes) => { out += decoder.decode(bytes, { stream: true }); });
vm.on("stop", ({ reason }) => fail(`the machine stopped (${reason})`));

const plain = () => out.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");
function fail(why) {
  console.error(`guest.mjs: ${cpus} CPU(s): ${why} at ${secs()}\n--- console (last 1500 chars) ---\n${plain().slice(-1500)}`);
  process.exit(1);
}
// Wait for the console to match, from where the last wait left off
let from = 0;
async function expect(re, what, timeout = 120_000) {
  const end = performance.now() + timeout;
  while (performance.now() < end) {
    const text = plain();
    const m = re.exec(text.slice(from));
    if (m) {
      from += m.index + m[0].length;
      console.log(`ok ${secs()}: ${what}`);
      return m;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  fail(`no ${what}`);
}
const type = (text) => vm.console_input(text);

await vm.ready;
const banner = await expect(/Linux (\S+) on (\d+) CPUs/, "the boot banner");
if (Number(banner[2]) !== cpus) fail(`the banner says ${banner[2]} CPUs`);
await expect(/# $/m, "a shell prompt");

type("nproc\n");
const n = await expect(/^(\d+)\s*$/m, "nproc's answer");
if (Number(n[1]) !== cpus) fail(`nproc says ${n[1]}`);

type("trench 40 12\n");
const unit = cpus === 1 ? "CPU" : "CPUs";
await expect(new RegExp(`frame 1: stay on target \\(${cpus} ${unit}\\)`), "trench's first frame");
await expect(new RegExp(`frame 3: stay on target \\(${cpus} ${unit}\\)`), "trench's third frame");

// The picture: each row is written to its place on the screen, 40 characters wide once the colours
// are taken out of it
const rows = new Set();
for (const m of out.matchAll(/\x1b\[(\d+);1H((?:[^\x1b\r\n]|\x1b\[[0-9;]*m)+)/g)) {
  if (m[2].replace(/\x1b\[[0-9;]*m/g, "").length === 40) rows.add(Number(m[1]));
}
if (rows.size !== 12) fail(`rows ${[...rows].sort((a, b) => a - b).join(",")} of the picture, not 1 to 12`);

type("\x03");
await expect(/# $/m, "the prompt back after ctrl+c", 30_000);
type("echo done-$((6*7))\n");
await expect(/done-42/, "the shell working after ctrl+c", 30_000);

console.log(`guest.mjs: ${cpus} CPU(s) passed in ${secs()}`);
vm.destroy?.();
process.exit(0);
