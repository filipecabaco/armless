// RAM above 4 GiB on the browser build: a machine of more than 3 GiB gets a wasm memory for each
// 4 GiB of RAM past 4 GiB (and a module to reach it), and a bare-metal guest writes, reads and
// load-exclusives it there, compiled by the JIT and interpreted, on one CPU and on several.
//
//   node --test 'tests/js/*.test.mjs'
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));

// The guest of tests/high_ram.rs (its source is there)
const GUEST = new Uint32Array([
  0xd2be0209, 0xd2c00020, 0xd2800003, 0xd374cc65, 0x91015467, 0xf8256807, 0x91000463, 0xf110007f,
  0x54ffff61, 0xd2800003, 0xd2800008, 0xd374cc65, 0xf8656807, 0x8b070108, 0x91000463, 0xf110007f,
  0x54ffff61, 0xd28a400a, 0xf2a0012a, 0xeb0a011f, 0x54000181, 0xc85f7c0b, 0x9100056b, 0xc80c7c0b,
  0x35ffffac, 0xf940000b, 0xf101597f, 0x540000a1, 0x528009e1, 0x39000121, 0x52800961, 0x14000004,
  0x528009c1, 0x39000121, 0x528009e1, 0x39000121, 0x52800141, 0x39000121, 0xd2800100, 0xf2b08000,
  0xd4000002, 0xd503207f, 0x17ffffff,
]);

async function run(cpus) {
  const vm = new Armless({
    wasm: path.join(root, "build/armless.wasm"),
    wasm_smp: path.join(root, "build/armless-smp.wasm"),
    memory_mb: 3072 + 8,
    cpus,
    console: false,
    jit_threshold: 16,
    boot: { raw: new Uint8Array(GUEST.buffer), addr: 0x200000 },
  });
  let out = "";
  vm.on("serial", (b) => { out += new TextDecoder().decode(b); });
  const stop = await new Promise((resolve, reject) => {
    vm.on("stop", resolve);
    vm.ready.catch(reject);
  });
  const banks = vm._banks;
  vm.destroy();
  return { stop: stop.reason, out, banks };
}

const built = fs.existsSync(path.join(root, "build/armless.wasm"));

test("a guest uses RAM above 4 GiB on one CPU", { skip: !built && "no build/armless.wasm" }, async () => {
  const { stop, out, banks } = await run(1);
  assert.equal(banks.memories.length, 1);
  assert.equal(banks.memories[0].buffer.byteLength, 8 << 20);
  assert.equal(out, "OK\n");
  assert.equal(stop, "poweroff");
});

test("a guest uses RAM above 4 GiB on two CPUs", {
  skip: !fs.existsSync(path.join(root, "build/armless-smp.wasm")) && "no build/armless-smp.wasm",
}, async () => {
  const { stop, out } = await run(2);
  assert.equal(out, "OK\n");
  assert.equal(stop, "poweroff");
});
