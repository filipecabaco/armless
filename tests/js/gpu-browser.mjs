#!/usr/bin/env node
// armless-gpu in a browser: tests/js/gpu.html in headless Chromium (Playwright), on WebGPU.
// With no GPU, Chromium's SwiftShader stands in. Needs build/armless.wasm (build-wasm.sh) and
// the guest program:
//
//   cargo build --release -p armless-gpu --example vecadd --no-default-features \
//       --target aarch64-unknown-linux-musl
//   node tests/js/gpu-browser.mjs
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require("playwright");
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const types = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm" };

const server = http.createServer((req, res) => {
  const file = path.join(root, decodeURIComponent(new URL(req.url, "http://x").pathname));
  if (!file.startsWith(root + path.sep) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { "content-type": types[path.extname(file)] || "application/octet-stream" });
  fs.createReadStream(file).pipe(res);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = `http://localhost:${server.address().port}/tests/js/gpu.html`;

const browser = await chromium.launch({
  args: ["--enable-unsafe-webgpu", "--enable-features=Vulkan", "--use-angle=swiftshader", "--use-webgpu-adapter=swiftshader"],
});
let failed = 0;
try {
  const page = await browser.newPage();
  page.on("console", (m) => m.type() === "error" && console.error("page:", m.text()));
  page.on("pageerror", (e) => console.error("page:", e.message));
  await page.goto(url);
  await page.waitForFunction(() => window.results, null, { timeout: 180000 });
  for (const r of await page.evaluate(() => window.results)) {
    console.log(`${r.ok ? "ok  " : "FAIL"} ${r.name}${r.detail ? `\n       ${r.detail.replace(/\n/g, "\n       ")}` : ""}`);
    if (!r.ok) failed++;
  }
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
