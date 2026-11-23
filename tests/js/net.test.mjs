// What armless.js lets the guest's HTTP requests and WebSockets reach, and what it sends with
// them: the relay's _http and _websocket driven with a stub fetch() and WebSocket and a stand-in
// for the wasm side, so no module, network or browser is needed.
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

/**
 * A relay with `network` as its options and the wasm side recorded: `calls` holds each call
 * with any bytes passed decoded as text, `fetches` and `sockets` what the guest's traffic made.
 */
function relay(network = {}) {
  const vm = Object.create(Armless.prototype);
  const calls = [];
  const fetches = [];
  const sockets = [];
  let next = 1_000_000; // pointers, apart from the ids
  const held = new Map();
  vm.exports = { armless_free: () => {} };
  vm._web = new Map();
  vm._copy_in = (bytes) => { held.set(next, bytes); return next++; };
  vm._with_bytes = (bytes, fn) => fn(vm._copy_in(bytes), bytes.length);
  vm._call = (name, ...args) => {
    calls.push([name, ...args.map((a) => (held.has(a) ? new TextDecoder().decode(held.get(a)) : a))]);
    return 0;
  };
  vm._drain = () => {};
  vm._poke = () => {};
  class FakeSocket {
    constructor(url, protocols) {
      this.url = url;
      this.protocols = protocols;
      this.readyState = 0;
      sockets.push(this);
    }
  }
  vm._web_options = Armless._web_options({
    fetch: async (url, init) => {
      fetches.push({ url, init });
      return new Response("hi", { status: 200, headers: { "x-from": "stub" } });
    },
    WebSocket: FakeSocket,
    upgrade_https: false,
    ...network,
  });
  return { vm, calls, fetches, sockets };
}

const failed = (calls, id) => calls.find(([n, i]) => n === "armless_http_fail" && i === id);
const tick = () => new Promise((r) => setTimeout(r, 10));

test("private and loopback destinations are refused before anything is fetched", async () => {
  const { vm, calls, fetches } = relay();
  const urls = [
    "http://127.0.0.1/", "http://2130706433/", "http://0x7f.1:8080/", "http://localhost:3000/",
    "http://db.localhost/", "http://169.254.169.254/latest/meta-data/", "http://10.0.0.1/",
    "http://192.168.1.1/", "http://[::1]/", "http://[::ffff:127.0.0.1]/", "http://[fd00:ec2::254]/",
    "http://[64:ff9b::a9fe:a9fe]/",
  ];
  let id = 1;
  for (const url of urls) {
    await vm._http(id, "GET", url, [], new Uint8Array());
    const f = failed(calls, id);
    assert.ok(f, `${url} was not refused`);
    assert.match(f[2], /private address/, url);
    id++;
  }
  assert.equal(fetches.length, 0, "a refused request reached fetch()");
});

test("a public destination is fetched without the page's cookies or referrer", async () => {
  const { vm, calls, fetches } = relay();
  await vm._http(1, "POST", "http://93.184.215.14/form", [["content-type", "text/plain"]], new TextEncoder().encode("x"));
  assert.equal(fetches.length, 1);
  const { init } = fetches[0];
  assert.equal(init.credentials, "omit");
  assert.equal(init.referrerPolicy, "no-referrer");
  // Server-side, redirects go back to the guest, whose next request is checked again
  assert.equal(init.redirect, "manual");
  assert.ok(calls.some(([n, i, status]) => n === "armless_http_respond" && i === 1 && status === 200));
  assert.ok(calls.some(([n, i]) => n === "armless_http_end" && i === 1));
});

test("a HEAD with no length is sized by a request for its first byte", async () => {
  // As Hugging Face answers a small file it would compress: no Content-Length on the HEAD
  const answers = (sized) => async (url, init) => {
    const range = new Headers(init.headers).get("range");
    if (init.method === "HEAD") return new Response(null, { status: 200 });
    assert.equal(range, "bytes=0-0");
    return sized
      ? new Response("C", { status: 206, headers: { "content-range": "bytes 0-0/11343" } })
      : new Response("whole", { status: 200 });
  };
  const respond = (calls) => calls.find(([n]) => n === "armless_http_respond");
  {
    const { vm, calls, fetches } = relay({ fetch: (url, init) => { fetches.push({ url, init }); return answers(true)(url, init); } });
    await vm._http(1, "HEAD", "http://93.184.215.14/v2/x/blobs/sha256:00", [["accept", "*/*"]], new Uint8Array());
    assert.deepEqual(fetches.map(({ init }) => init.method), ["HEAD", "GET"]);
    // The probe asks the server: a browser's cache answers a range of a compressed response in
    // the compressed bytes
    assert.equal(fetches[0].init.cache, undefined);
    assert.equal(fetches[1].init.cache, "no-store");
    assert.equal(new Headers(fetches[1].init.headers).get("accept"), "*/*", "the probe asks as the guest did");
    assert.match(respond(calls)[5], /(^|\0)content-length\x0011343($|\0)/);
  }
  {
    // A server that ignores the range says nothing of the length, so neither does the guest's HEAD
    const { vm, calls } = relay({ fetch: answers(false) });
    await vm._http(1, "HEAD", "http://93.184.215.14/x", [], new Uint8Array());
    assert.doesNotMatch(respond(calls)[5], /content-length/);
  }
});

test("a guest's request for a range goes past the browser's cache", async () => {
  const { vm, fetches } = relay();
  await vm._http(1, "GET", "http://93.184.215.14/blob", [["range", "bytes=0-11342"]], new Uint8Array());
  await vm._http(2, "GET", "http://93.184.215.14/blob", [], new Uint8Array());
  assert.equal(fetches[0].init.cache, "no-store");
  assert.equal(fetches[1].init.cache, undefined, "a whole response may come from the cache");
});

test("a name that resolves to a private address is refused server-side", async () => {
  // The names resolve as this stub of node:dns says
  const answers = {
    "rebind.example": [{ address: "93.184.215.14", family: 4 }, { address: "127.0.0.1", family: 4 }],
    "metadata.example": [{ address: "fd00:ec2::254", family: 6 }],
    "public.example": [{ address: "93.184.215.14", family: 4 }],
  };
  const lookups = [];
  const real = process.getBuiltinModule;
  process.getBuiltinModule = (name) => (name === "node:dns"
    ? { promises: { lookup: async (host) => { lookups.push(host); if (answers[host]) return answers[host]; throw new Error("ENOTFOUND"); } } }
    : real.call(process, name));
  try {
    const { vm, calls, fetches } = relay();
    await vm._http(1, "GET", "http://rebind.example/", [], new Uint8Array());
    await vm._http(2, "GET", "https://metadata.example/", [], new Uint8Array());
    await vm._http(3, "GET", "http://public.example/", [], new Uint8Array());
    // A name that doesn't resolve is fetch()'s to report
    await vm._http(4, "GET", "http://nowhere.example/", [], new Uint8Array());
    assert.match(failed(calls, 1)[2], /private address \(127\.0\.0\.1\)/);
    assert.match(failed(calls, 2)[2], /private address \(fd00:ec2::254\)/);
    assert.deepEqual(fetches.map((f) => f.url), ["http://public.example/", "http://nowhere.example/"]);
    // Literals are never looked up
    await vm._http(5, "GET", "http://93.184.215.14/", [], new Uint8Array());
    assert.deepEqual(lookups, ["rebind.example", "metadata.example", "public.example", "nowhere.example"]);
  } finally {
    process.getBuiltinModule = real;
  }
});

test("allow_private lets them through, and allow decides alone", async () => {
  {
    const { vm, fetches } = relay({ allow_private: true });
    await vm._http(1, "GET", "http://127.0.0.1:8080/", [], new Uint8Array());
    assert.equal(fetches.length, 1);
  }
  {
    const asked = [];
    const { vm, calls, fetches } = relay({
      allow: ({ url, host, port, kind }) => {
        asked.push({ url, host, port, kind });
        return host === "10.0.0.5";
      },
    });
    await vm._http(1, "GET", "http://10.0.0.5:5000/x", [], new Uint8Array());
    await vm._http(2, "GET", "https://example.com/", [], new Uint8Array());
    assert.equal(fetches.length, 1);
    assert.equal(fetches[0].url, "http://10.0.0.5:5000/x");
    assert.match(failed(calls, 2)[2], /not allowed/);
    assert.deepEqual(asked[0], { url: "http://10.0.0.5:5000/x", host: "10.0.0.5", port: 5000, kind: "http" });
    assert.equal(asked[1].port, 443);
  }
  {
    // An allow that throws refuses
    const { vm, calls, fetches } = relay({ allow: () => { throw new Error("no"); } });
    await vm._http(1, "GET", "https://example.com/", [], new Uint8Array());
    assert.ok(failed(calls, 1));
    assert.equal(fetches.length, 0);
  }
});

test("WebSockets are checked the same way", async () => {
  const { vm, calls, sockets } = relay();
  vm._websocket(1, "ws://127.0.0.1:9000/", []);
  vm._websocket(2, "wss://[::1]/", ["chat"]);
  vm._websocket(3, "wss://93.184.215.14/socket", ["chat"]);
  await tick();
  assert.ok(failed(calls, 1));
  assert.ok(failed(calls, 2));
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].url, "wss://93.184.215.14/socket");
  assert.deepEqual(sockets[0].protocols, ["chat"]);
  assert.equal(vm._web.get(3).socket, sockets[0]);
  // The guest closing a WebSocket while it is being checked: nothing is opened
  const { vm: vm2, sockets: s2 } = relay({ allow: () => new Promise((r) => setTimeout(() => r(true), 5)) });
  vm2._websocket(7, "wss://93.184.215.14/", []);
  vm2._web.delete(7);
  await tick();
  assert.equal(s2.length, 0);
});

test("private_host", () => {
  for (const h of ["127.0.0.1", "[::1]", "localhost", "a.localhost.", "100.64.0.1", "198.18.0.1", "[fe80::1%25eth0]", "[2002:c0a8:101::1]", "[2001:0:4136:e378:8000:63bf:3fff:fdd2]"]) {
    assert.ok(Armless.private_host(h), h);
  }
  for (const h of ["example.com", "1.1.1.1", "[2606:4700::1111]", "[64:ff9b::101:101]", "localhost.example"]) {
    assert.ok(!Armless.private_host(h), h);
  }
});
