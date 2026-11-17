// armless.js fetches a blob by its name relative to the filesystem's baseurl (or, under Node,
// a directory): a name that is no plain file name, which a snapshot from elsewhere could hold,
// is refused before anything is fetched.
//
//   node --test 'tests/js/*.test.mjs'
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));

function machine() {
  const vm = Object.create(Armless.prototype);
  const failed = [];
  const asked = [];
  const provided = [];
  vm.options = { filesystem: { baseurl: "https://site.example/fs/", fetch_blob: async (name) => { asked.push(name); return new Uint8Array([1]); } } };
  vm._blobs_in_flight = new Set();
  vm.exports = { armless_free: () => {} };
  vm._with_bytes = (bytes, fn) => fn(new TextDecoder().decode(bytes), bytes.length);
  vm._call = (name, arg) => { if (name === "armless_blob_failed") failed.push(arg); };
  vm._drain = () => {};
  vm._poke = () => {};
  vm.emit = () => {};
  vm.provide_blob = (name) => provided.push(name);
  return { vm, failed, asked, provided };
}

test("blob names that would leave the blob store are refused", async () => {
  const warn = console.warn;
  console.warn = () => {};
  try {
    const { vm, failed, asked } = machine();
    const bad = ["../secret", "https:evil.example", "//evil.example/x", "a/b", "..", ".hidden", "a%2fb", "a?b", "a#b", "a\\b", ""];
    for (const name of bad) await vm._fetch_blob(name);
    assert.deepEqual(asked, []);
    assert.deepEqual(failed, bad);
  } finally {
    console.warn = warn;
  }
  const { vm, failed, asked, provided } = machine();
  for (const name of ["0a1b2c3d.bin.zst", "true.elf", "good"]) await vm._fetch_blob(name);
  assert.deepEqual(asked, ["0a1b2c3d.bin.zst", "true.elf", "good"]);
  assert.deepEqual(provided, asked);
  assert.deepEqual(failed, []);
});
