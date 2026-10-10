#!/usr/bin/env node
// armless.js end to end, in Node: boot from a 9p filesystem (zstd blobs), talk over
// the virtio console, fetch over HTTP through the page-side fetch() relay, snapshot,
// restore into a fresh machine and keep talking.
//
//   node tests/js/flow.mjs DIR [CPUS]      (DIR from tests/fs/make-9pfs.sh)
//
// HTTP goes to a server in this process: the guest asks for http://testhost/..., which the
// machine's web relay hands out as a request this test's fetch() points here.
import crypto from "node:crypto";
import http from "node:http";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));

const dir = path.resolve(process.argv[2] || ".");
const cpus = Number(process.argv[3] || 1);
// 3 MiB of random data to download, and an endpoint that hashes uploads
const DATA = crypto.randomBytes(3 << 20);
const md5 = (b) => crypto.createHash("md5").update(b).digest("hex");
const server = http.createServer((req, res) => {
  const parts = [];
  req.on("data", (c) => parts.push(c));
  req.on("end", () => {
    if (req.method === "POST") {
      const body = Buffer.concat(parts);
      res.end(`${body.length} ${md5(body)}\n`);
    } else if (req.url === "/data.bin") {
      res.end(DATA);
    } else {
      res.statusCode = 404;
      res.end("not here\n");
    }
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const PORT = server.address().port;
const network = {
  upgrade_https: false,
  fetch: (url, init) => fetch(url.replace(/^http:\/\/testhost\b/, `http://127.0.0.1:${PORT}`), init),
};

const CMDLINE = "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose init=/init console=ttyS0 quiet";

function machine(extra) {
  const vm = new Armless({
    wasm: path.join(root, "build/armless.wasm"),
    wasm_smp: path.join(root, "build/armless-smp.wasm"),
    memory_mb: 256,
    cpus,
    filesystem: { json: path.join(dir, "fs.json"), baseurl: path.join(dir, "blobs") + "/" },
    network,
    ...extra,
  });
  let text = "";
  const waiters = [];
  vm.on("console", (bytes) => {
    text += new TextDecoder().decode(bytes);
    for (const w of waiters.splice(0)) {
      const at = text.indexOf(w.needle);
      if (at >= 0) {
        const out = text.slice(0, at + w.needle.length);
        text = text.slice(at + w.needle.length);
        clearTimeout(w.timer);
        w.resolve(out);
      } else waiters.push(w);
    }
  });
  vm.wait = (needle, ms = 120000) => new Promise((resolve, reject) => {
    const w = { needle, resolve };
    w.timer = setTimeout(() => reject(new Error(`timeout waiting for ${JSON.stringify(needle)}; console:\n${text}`)), ms);
    waiters.push(w);
    vm.emit("console", new Uint8Array(0));
  });
  let n = 0;
  vm.sh = (cmd) => {
    const marker = `DONE${++n}X`;
    vm.console_input(`${cmd}; echo ${marker}\n`);
    return vm.wait(`${marker}\r\n`);
  };
  return vm;
}

function check(cond, what, out) {
  if (!cond) {
    console.error(`FAIL: ${what}\n${out}`);
    process.exit(1);
  }
  console.log(`ok: ${what}`);
}

const t0 = performance.now();
const a = machine({ boot: { kernel: path.join(dir, "vmlinuz"), cmdline: CMDLINE } });
await a.ready;
await a.wait("READY");
console.log(`booted in ${((performance.now() - t0) / 1000).toFixed(1)}s on ${cpus} CPU(s)`);
let out = await a.sh("cat /etc/motd; grep -c ^processor /proc/cpuinfo; md5sum /data/big.bin");
check(out.includes("hello from 9p"), "9p read", out);
check(out.includes(`${cpus}\r\n`), `${cpus} CPUs online`, out);
check(out.includes("2b1902602225a2f8cd62faa5381597b4"), "8 MiB file over 9p", out);
await a.sh("ifconfig eth0 192.168.86.100 netmask 255.255.255.0 up; route add default gw 192.168.86.1; echo nameserver 192.168.86.1 > /etc/resolv.conf");
out = await a.sh("wget -q -O - http://testhost/data.bin | md5sum");
check(out.includes(md5(DATA)), "HTTP download through fetch() (3 MiB)", out);
// busybox wget --post-file stops at NUL bytes: upload text
out = await a.sh("dd if=/data/big.bin bs=1M count=2 2>/dev/null | base64 > /tmp/up; md5sum /tmp/up; wget -q -O - --post-file=/tmp/up http://testhost/upload");
const sums = [...out.matchAll(/[0-9a-f]{32}/g)].map((m) => m[0]);
check(sums.length === 2 && sums[0] === sums[1], "HTTP upload through fetch() (2.7 MiB)", out);
out = await a.sh("wget -q -O - http://testhost/nope; echo status=$?");
check(out.includes("status=1"), "HTTP error status reaches the guest", out);

// A connection from the host into the guest (port forwarding)
await a.sh("(echo hello-from-guest | nc -l -p 7000 &) ; sleep 1");
const conn = a.connect(7000);
const got = await new Promise((resolve, reject) => {
  let text = "";
  conn.on("data", (b) => { text += new TextDecoder().decode(b); });
  conn.on("close", () => resolve(text));
  conn.on("end", () => conn.close());
  setTimeout(() => reject(new Error("port forward timed out: " + text)), 30000);
});
check(got.includes("hello-from-guest"), "connection into the guest", got);
out = await a.sh("echo kept > /root/note; sync; echo 3 > /proc/sys/vm/drop_caches");

const state = await a.save_state();
console.log(`snapshot: ${(state.length / 1048576).toFixed(1)} MiB`);
a.destroy();

const b = machine({ initial_state: state });
await b.ready;
out = await b.sh("cat /root/note; md5sum /data/big.bin; ls /data/small | wc -l");
check(out.includes("kept"), "guest file kept across the snapshot", out);
check(out.includes("2b1902602225a2f8cd62faa5381597b4"), "blobs fetched again after restore", out);
check(out.includes("300"), "directory listing", out);
b.console_resize(120, 40);
out = await b.sh("sleep 1; stty size");
check(out.includes("40 120"), "console resize", out);
out = await b.sh("wget -q -O - http://testhost/data.bin | md5sum");
check(out.includes(md5(DATA)), "HTTP after restore", out);
b.console_input("poweroff\n");
const stop = await new Promise((r) => b.on("stop", r));
check(stop.reason === "poweroff", "power off", JSON.stringify(stop));
b.destroy();
server.close();
console.log(`all good in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
process.exit(0);
