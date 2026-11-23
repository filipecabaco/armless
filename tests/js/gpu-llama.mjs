#!/usr/bin/env node
// llama.cpp on armless.wasm with its GPU work on WebGPU through armless-gpu.js: the browser's code
// path, run in Node with Dawn (the `webgpu` npm package) as navigator.gpu. Run by hand: it needs a
// filesystem with a guest llama.cpp (gpu/guest/build-llama.sh) and a model.
//
//   node tests/js/gpu-llama.mjs FSDIR [PROMPT] [-- LLAMA ARGS...]
//   GUEST_ARGV='["/usr/local/bin/ollama-compat","run","smollm2:135m","Hi"]' node tests/js/gpu-llama.mjs FSDIR
//       FSDIR: fs.json and blobs/ (tests/fs/fs2json.py and copy-to-sha256.py over a tree with
//       /bin/llama-completion and /models/model.gguf). WEBGPU_MODULE: where `webgpu` is installed
//       (default: resolved from here).
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));
const ArmlessGPU = require(path.join(root, "js/armless-gpu.js"));
const webgpu = await import(process.env.WEBGPU_MODULE || "webgpu");
Object.assign(globalThis, webgpu.globals);
const gpu = webgpu.create([]);

const argv = process.argv.slice(2);
const dash = argv.indexOf("--");
const [fsdir, prompt = "The capital of France is"] = dash < 0 ? argv : argv.slice(0, dash);
const extra = dash < 0 ? [] : argv.slice(dash + 1);
if (!fsdir) {
  console.error("usage: gpu-llama.mjs FSDIR [PROMPT] [-- LLAMA ARGS...]");
  process.exit(2);
}
const json = JSON.parse(fs.readFileSync(path.join(fsdir, "fs.json"), "utf8"));
// GUEST_ARGV (a JSON array) runs another program instead, ollama-compat say
const llama = process.env.GUEST_ARGV ? JSON.parse(process.env.GUEST_ARGV) : ["/bin/llama-completion", "-m", "/models/model.gguf",
  "-p", prompt, "-n", "16", "-no-cnv", "--temp", "0", "-t", "1", "-ngl", "99", ...extra];

const vm = new Armless({
  wasm: path.join(root, "build/armless.wasm"),
  memory_mb: 1536,
  autostart: false,
  network: true,
  filesystem: { json, fetch_blob: async (name) => fs.readFileSync(path.join(fsdir, "blobs", name)) },
  user: { argv: llama, env: ["PATH=/usr/local/bin:/bin", "HOME=/root", "OLLAMA_VERBOSE=1"] },
});
const stopGPU = ArmlessGPU.attach(vm, { gpu, log: (m) => console.error(m) });
const start = Date.now();
vm.on("console", (b) => process.stdout.write(b));
await vm.ready;
const stop = await new Promise((resolve) => {
  vm.on("stop", resolve);
  vm.run();
});
stopGPU();
console.error(`\n[gpu-llama] stopped (${JSON.stringify(stop)}) after ${((Date.now() - start) / 1000).toFixed(1)} s`);
process.exit(stop.code ?? 1);
