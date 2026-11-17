// The landing page's CPU count for a device (site/static/cpus.js).
//
//   node --test site/test/cpus.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const { cpuCount } = createRequire(import.meta.url)("../static/cpus.js");
const device = (over) => cpuCount({ isolated: true, cores: 8, memoryGb: 8, coarse: false, max: 8, ...over });

test("one core is left to the page", () => {
  assert.equal(device({ cores: 8 }), 7);
  assert.equal(device({ cores: 4 }), 3);
});

test("never more than armless's limit", () => {
  assert.equal(device({ cores: 24 }), 8);
  assert.equal(device({ cores: 24, max: 4 }), 4);
});

test("at least one CPU, whatever the device reports", () => {
  assert.equal(device({ cores: 1 }), 1);
  assert.equal(device({ cores: 2 }), 1);
  assert.equal(device({ cores: 0 }), 1);
  assert.equal(device({ cores: undefined }), 1);
});

test("phones and tablets stay at four", () => {
  assert.equal(device({ coarse: true, cores: 8 }), 4);
  assert.equal(device({ coarse: true, cores: 4 }), 3);
});

test("two on devices with 2 GB of memory or less", () => {
  assert.equal(device({ memoryGb: 2 }), 2);
  assert.equal(device({ memoryGb: 1 }), 2);
  assert.equal(device({ memoryGb: 4 }), 7);
  assert.equal(device({ memoryGb: undefined }), 7);
});

test("one CPU without cross-origin isolation", () => {
  assert.equal(device({ isolated: false }), 1);
  assert.equal(device({ isolated: false, cores: 32 }), 1);
});
