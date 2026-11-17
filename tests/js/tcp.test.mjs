// Guest-to-host flow control in armless.js: what the page did with each connection's data goes
// back to the machine (armless_tcp_consumed), which opens the guest's window again. Driven through
// _event with a stand-in for the wasm side: no module needed.
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

function machine(network = {}) {
  const vm = Object.create(Armless.prototype);
  const consumed = [];
  const calls = [];
  vm._conns = new Map();
  vm._web = new Map();
  vm._listeners = new Map();
  vm._call = (name, ...args) => {
    calls.push([name, ...args]);
    if (name === "armless_tcp_consumed") consumed.push(args);
    return 0;
  };
  vm._with_bytes = (bytes, fn) => fn(0, bytes.length);
  vm._poke = () => {};
  vm._drain = () => {};
  vm._web_options = Armless._web_options(network);
  return { vm, consumed, calls };
}

const open = (vm, id, host, port) => vm._event(4, id, port, new TextEncoder().encode(host));

test("data a listener takes is consumed at once; data for no connection too", () => {
  const { vm, consumed } = machine();
  const got = [];
  vm.on("tcp-connection", (c) => { c.accept(); c.on("data", (d) => got.push(d.length)); });
  open(vm, 3, "db.example", 5432);
  vm._event(6, 3, 0, new Uint8Array(1000));
  vm._event(6, 3, 0, new Uint8Array(24));
  assert.deepEqual(got, [1000, 24]);
  assert.deepEqual(consumed, [[3, 1000], [3, 24]]);
  // A connection that is gone: dropped, consumed
  vm._event(6, 99, 0, new Uint8Array(7));
  assert.deepEqual(consumed.at(-1), [99, 7]);
});

test("a tunnel's data is consumed as its WebSocket sends it", async () => {
  const sockets = [];
  class FakeSocket {
    constructor(url) { this.url = url; this.readyState = 0; this.bufferedAmount = 0; this.sent = []; sockets.push(this); }
    send(b) { this.sent.push(b.length); this.bufferedAmount += b.length; }
    close() { this.readyState = 3; }
  }
  const { vm, consumed } = machine({ WebSocket: FakeSocket, tunnels: [{ port: 5432, url: "wss://gw.example/db" }] });
  open(vm, 5, "db.example", 5432);
  await new Promise((r) => setTimeout(r, 0)); // the tunnel's URL is settled first
  const ws = sockets[0];
  ws.readyState = 1;
  ws.onopen();
  vm._event(6, 5, 0, new Uint8Array(3000));
  // Still in the WebSocket's buffer: not consumed
  assert.deepEqual(ws.sent, [3000]);
  assert.deepEqual(consumed, []);
  // The WebSocket sends some, then the rest
  ws.bufferedAmount = 1000;
  await new Promise((r) => setTimeout(r, 20));
  assert.deepEqual(consumed, [[5, 2000]]);
  ws.bufferedAmount = 0;
  await new Promise((r) => setTimeout(r, 20));
  assert.deepEqual(consumed, [[5, 2000], [5, 1000]]);
});
