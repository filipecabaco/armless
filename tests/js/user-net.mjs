#!/usr/bin/env node
// User mode's sockets through armless.js and armless.wasm, in Node: the browser's host.
//
//   node tests/js/user-net.mjs FSDIR
//       FSDIR: filesystem.json and filesystem/ with python3 in it (a site's system/, say
//       snowglobe's python site's). Needs build/armless.wasm (sh build-wasm.sh).
//
// 1. A client: python3's urllib fetches http://example.test/hello. Its DNS lookup (UDP to port
//    53) and its connection to port 80 go through the machine's web relay to the host's fetch()
//    (a stub here), whose response it prints.
// 2. A server: python3's http.server listens on port 8000, and a connection the host opens to it
//    (`connect`, what snowglobe's previews use) gets its page.
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));
const wasm = path.join(root, "build/armless.wasm");
const decode = (b) => new TextDecoder().decode(b);

function fail(msg) {
  console.error("FAIL: " + msg);
  process.exit(1);
}

function check(cond, what, detail) {
  if (!cond) fail(`${what}\n${detail ?? ""}`);
  console.log(`ok: ${what}`);
}

const dir = path.resolve(process.argv[2] || fail("usage: user-net.mjs FSDIR"));
const env = ["PATH=/usr/local/bin:/usr/bin:/bin", "HOME=/root", "TERM=xterm"];

/** A machine running `argv` with a network whose fetch() is `fetch`. */
async function machine(argv, fetch) {
  const vm = new Armless({
    wasm, memory_mb: 512, autostart: false,
    filesystem: { json: path.join(dir, "filesystem.json"), baseurl: path.join(dir, "filesystem") + "/" },
    network: { fetch },
    user: { argv, env, cwd: "/tmp", cols: 100, rows: 30 },
  });
  vm.text = "";
  vm.on("console", (b) => (vm.text += decode(b)));
  await vm.ready;
  return vm;
}

/** Waits until `vm`'s console has `needle`, within `ms`. */
function printed(vm, needle, ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => fail(`no ${JSON.stringify(needle)} after ${ms} ms:\n${vm.text}`), ms);
    const look = () => {
      if (vm.text.includes(needle)) {
        clearTimeout(timer);
        vm.off("console", look);
        resolve();
      }
    };
    vm.on("console", look);
    look();
  });
}

// ------------------------------------------------------------ a client
{
  const asked = [];
  const stub = async (url, init) => {
    asked.push(`${init?.method ?? "GET"} ${url}`);
    return new Response("hi from the stub", { status: 200, headers: { "content-type": "text/plain" } });
  };
  const t0 = performance.now();
  const vm = await machine(
    ["python3", "-c", "import urllib.request; print('client:', urllib.request.urlopen('http://example.test/hello', timeout=60).read().decode())"],
    stub,
  );
  const stopped = new Promise((r) => vm.on("stop", r));
  vm.run();
  await printed(vm, "client: hi from the stub", 120000);
  const s = await stopped;
  check(asked.length === 1 && asked[0] === "GET http://example.test/hello", "the request reached the host's fetch() by name", asked.join("\n"));
  check(s.reason === "exit" && s.code === 0, `python3 exited 0 (${(performance.now() - t0).toFixed(0)} ms)`, JSON.stringify(s));
  vm.destroy();
}

// ------------------------------------------------------------ a server
{
  const vm = await machine(["python3", "-m", "http.server", "8000"], () => fail("the server fetched something"));
  vm.run();
  await printed(vm, "Serving HTTP", 120000);
  const page = await new Promise((resolve) => {
    let got = "";
    const timer = setTimeout(() => fail(`no page after 60 s: ${JSON.stringify(got)}\n${vm.text}`), 60000);
    const conn = vm.connect(8000);
    conn.on("connect", () => conn.write(new TextEncoder().encode("GET / HTTP/1.0\r\nHost: x\r\n\r\n")));
    conn.on("data", (b) => (got += decode(b)));
    conn.on("end", () => conn.close());
    conn.on("close", () => {
      clearTimeout(timer);
      resolve(got);
    });
  });
  check(page.startsWith("HTTP/1.0 200 OK") && page.includes("Directory listing for /"), "a connection into the program got its page", page.slice(0, 300));
  check(vm.text.includes('"GET / HTTP/1.0" 200'), "the server logged it", vm.text);
  // Nobody listens on 8001: refused
  const refused = await new Promise((resolve) => {
    const conn = vm.connect(8001);
    conn.on("connect", () => resolve(false));
    conn.on("close", () => resolve(true));
  });
  check(refused, "a port nobody listens on is refused");
  vm.destroy();
}
console.log("all good");
process.exit(0);
