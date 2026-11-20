#!/usr/bin/env node
// Which of the race's files (demo/py, site/demo.exs) python3 reads before its first >>>, with no
// kernel and under Linux: demo/py/startup.json, { user: [blob, ...], kernel: [blob, ...] }. The
// race (site/static/race.js) downloads them, and Linux's kernel and initramfs, before it starts
// either machine, so neither is timed waiting on the network.
//
//   node site/py-startup.mjs [DEMO_DIR]
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.resolve(process.argv[2] || path.join(here, "static/demo"));
const config = JSON.parse(fs.readFileSync(path.join(dir, "config.json"), "utf8"));
if (!config.python) {
  console.log("py-startup: no python filesystem in this build");
  process.exit(0);
}
const { Armless } = createRequire(import.meta.url)(path.join(dir, "armless.js"));
const at = (f) => path.join(dir, f);
const plain = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");

// The blobs a machine asked for before python3 printed its prompt
async function startup(options) {
  const blobs = new Set();
  const vm = new Armless({
    wasm: at("armless.wasm"),
    memory_mb: config.memory_mb,
    cpus: 1,
    filesystem: { json: at("py/filesystem.json"), baseurl: at("py/blobs") + "/" },
    ...options,
  });
  let out = "";
  const decoder = new TextDecoder();
  vm.on("blob", (name) => blobs.add(name));
  vm.on("console", (b) => { out += decoder.decode(b, { stream: true }); });
  await vm.ready;
  const end = performance.now() + 120_000;
  while (!/>>> $/.test(plain(out))) {
    if (performance.now() > end) throw new Error("no >>>\n" + plain(out).slice(-500));
    await new Promise((r) => setTimeout(r, 20));
  }
  vm.destroy();
  return [...blobs].sort();
}

const user = await startup({ user: { argv: ["python3"], env: ["PATH=/usr/bin:/bin", "HOME=/root", "TERM=xterm"], cwd: "/root" } });
const kernel = await startup({ boot: { kernel: at("Image.gz"), initrd: at("initrd.cpio"), cmdline: `${config.cmdline} python` } });
fs.writeFileSync(at("py/startup.json"), JSON.stringify({ user, kernel }));
console.log(`py-startup: ${user.length} blobs with no kernel, ${kernel.length} under Linux`);
