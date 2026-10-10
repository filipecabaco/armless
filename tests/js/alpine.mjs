#!/usr/bin/env node
// Boot a real arm64 Alpine (linux-virt, OpenRC) from a 9p filesystem the way
// snowglobe does: kernel and initrd from /boot in the filesystem, root=host9p,
// a shell on ttyS0; then save a snapshot and restore it.
//
//   node tests/js/alpine.mjs DIR [CPUS]   (DIR: fs.json + blobs/ from tests/fs/alpine/build.sh)
import http from "node:http";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));
const dir = path.resolve(process.argv[2] || ".");
const cpus = Number(process.argv[3] || 1);
const t0 = Date.now();
const log = (msg) => {
  const line = `+${((Date.now() - t0) / 1000).toFixed(1)}s ${msg}`;
  console.log(`\n[alpine.mjs ${line}]`);
  if (process.env.GITHUB_ACTIONS) console.log(`::notice title=alpine::${line.replace(/[\r\n%]/g, " ")}`);
};

let serial = "";
function fail(msg) {
  console.log(`\n[alpine.mjs] FAIL: ${msg}\n--- serial tail ---\n${serial.slice(-4000)}`);
  if (process.env.GITHUB_ACTIONS) {
    console.log(`::error title=alpine: ${msg}::${serial.slice(-1800).replace(/[\r%]/g, "").replace(/\n/g, ";")}`);
  }
  process.exit(1);
}

// HTTPS from the guest: terminated in the machine with the test CA, made here with fetch()
// pointed at this server
const server = http.createServer((req, res) => {
  const parts = [];
  req.on("data", (c) => parts.push(c));
  req.on("end", () => res.end(`secure ${req.method} ${req.url} ${Buffer.concat(parts).length}\n`));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const network = {
  tls: path.join(root, "tests/fixtures/tls.json"),
  upgrade_https: false,
  fetch: (url, init) => fetch(url.replace(/^https:\/\/[^/]+/, `http://127.0.0.1:${server.address().port}`), init),
};

function make(extra) {
  const vm = new Armless({
    wasm: path.join(root, "build/armless.wasm"),
    wasm_smp: path.join(root, "build/armless-smp.wasm"),
    memory_mb: 512,
    cpus,
    filesystem: { json: path.join(dir, "fs.json"), baseurl: path.join(dir, "blobs") + "/" },
    network,
    ...extra,
  });
  vm.on("serial", (bytes) => {
    const s = new TextDecoder().decode(bytes);
    serial += s;
    if (process.env.VERBOSE) process.stdout.write(s);
  });
  vm.ready.catch((e) => fail(String(e)));
  return vm;
}

function wait_for(needle, ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => fail(`timeout waiting for ${JSON.stringify(needle)}`), ms);
    const poll = () => {
      if (serial.includes(needle)) { clearTimeout(timer); resolve(); }
      else setTimeout(poll, 50);
    };
    poll();
  });
}

let vm = make({
  boot: {
    cmdline: "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose modules=virtio_pci " +
      "console=ttyS0 init_on_free=on",
  },
});
await wait_for("armless:~#", 20 * 60_000);
log(`Alpine booted to a root shell on ttyS0 (${cpus} CPU${cpus > 1 ? "s" : ""})`);
vm.serial_input("uname -m; cat /etc/alpine-release; nproc; echo ok-$((20+22)); ls /lib/modules\n");
await wait_for("ok-42", 120_000);
if (!serial.includes("aarch64")) fail("uname -m is not aarch64");
log("commands run: " + serial.slice(serial.lastIndexOf("uname -m")).split("\n").slice(1, 5).join(" | "));

vm.serial_input("ip link set eth0 up; ip addr add 192.168.86.100/24 dev eth0; ip route add default via 192.168.86.1; " +
  "echo nameserver 192.168.86.1 > /etc/resolv.conf; curl -sS -d hello https://api.example.test/v1?q=2; echo net-$((40+2))\n");
await wait_for("net-42", 120_000);
if (!serial.includes("secure POST /v1?q=2 5")) fail("HTTPS through the web relay");
log("HTTPS from the guest: " + serial.slice(serial.indexOf("secure POST")).split("\n")[0]);

vm.serial_input("sync; echo 3 > /proc/sys/vm/drop_caches; echo cache-$((40+2))\n");
await wait_for("cache-42", 120_000);
const state = await vm.save_state();
vm.destroy();
log(`snapshot ${(state.byteLength / 1048576).toFixed(1)} MiB`);

serial = "";
vm = make({ initial_state: state });
await vm.ready;
vm.serial_input("echo restored-$((6*7)) $(uname -r)\n");
await wait_for("restored-42", 120_000);
log("restored snapshot answers: " + serial.slice(serial.indexOf("restored-42")).split("\n")[0]);
vm.destroy();
server.close();
log("PASS");
process.exit(0);
