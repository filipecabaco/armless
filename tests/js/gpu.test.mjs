// armless-gpu.js on a real WebGPU in Node: Dawn (the `webgpu` npm package), on whatever adapter it
// finds (CI: Mesa's lavapipe, a software Vulkan with shader-f16, which Chromium's SwiftShader
// lacks). The checks are tests/js/gpu-checks.mjs, which tests/js/gpu.html runs in Chromium.
// Skipped where `webgpu` isn't installed or finds no adapter, unless ARMLESS_GPU_TEST_REQUIRE is set.
//
//   npm install --prefix /tmp/webgpu webgpu
//   WEBGPU_MODULE=/tmp/webgpu/node_modules/webgpu/index.js node --test tests/js/gpu.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { runChecks } from "./gpu-checks.mjs";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const ArmlessGPU = require(path.join(root, "js/armless-gpu.js"));

let gpu = null;
let why = "";
try {
  const where = process.env.WEBGPU_MODULE;
  const webgpu = await import(where ? pathToFileURL(where).href : "webgpu");
  Object.assign(globalThis, webgpu.globals);
  gpu = webgpu.create([]);
  if (!(await gpu.requestAdapter())) {
    why = "Dawn found no adapter";
    gpu = null;
  }
} catch (e) {
  why = `no webgpu package (${e.code || e.message})`;
}

// CI sets ARMLESS_GPU_TEST_REQUIRE=1: there, no adapter is a failure, not a skip
const required = !!process.env.ARMLESS_GPU_TEST_REQUIRE;
if (required && !gpu) test("a WebGPU adapter (ARMLESS_GPU_TEST_REQUIRE)", () => assert.fail(why));

test("armless-gpu.js's protocol on Dawn", { skip: gpu ? false : why }, async (t) => {
  const failed = [];
  await runChecks({
    gpu,
    ArmlessGPU,
    check: (name, ok, detail = "") => {
      t.diagnostic(`${ok ? "ok  " : "FAIL"} ${name}${detail ? `: ${String(detail).split("\n")[0]}` : ""}`);
      if (!ok) failed.push(`${name}: ${detail}`);
    },
  });
  assert.deepEqual(failed, []);
});

test("the adapter has what llama.cpp's WebGPU backend needs (shader-f16)", { skip: gpu ? false : why }, async () => {
  const a = await gpu.requestAdapter();
  assert.ok(a.features.has("shader-f16"), "no shader-f16");
});
