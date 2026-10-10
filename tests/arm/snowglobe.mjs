#!/usr/bin/env node
// End-to-end test of the snowglobe flow on armless:
//   1. boot an arm64 kernel whose root filesystem is v86's 9p (zstd blobs),
//      with the app on the virtio console (hvc0) and virtio-net (fetch);
//   2. talk to the guest over the virtio console, then save_state();
//   3. restore the zstd-compressed snapshot into a fresh emulator
//      (initial_state, as the static site does) and keep talking to it.
//
//   node tests/arm/snowglobe.mjs DIR     (DIR has vmlinuz, fs.json, blobs/)
import fs from "node:fs";
import path from "node:path";
import http from "node:http";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
// V86_MODULE=build/libv86.mjs tests the Closure-compiled bundle snowglobe ships.
const { V86 } = await import(path.resolve(root, process.env.V86_MODULE || "src/main.js"));
const dir = path.resolve(process.argv[2] || ".");
const MEMORY = 256 * 1024 * 1024;

function make(extra)
{
    return new V86({
        arch: "arm64",
        disable_jit: process.env.ARMLESS_JIT === "0",
        cpu_count: Number(process.env.ARMLESS_CPUS || 1),
        wasm_path: path.join(root, "build/armless.wasm"),
        memory_size: MEMORY,
        filesystem: {
            basefs: { url: path.join(dir, "fs.json") },
            baseurl: path.join(dir, "blobs") + "/",
        },
        virtio_console: true,
        net_device: { type: "virtio", relay_url: "fetch" },
        autostart: true,
        disable_keyboard: true,
        disable_mouse: true,
        disable_speaker: true,
        log_level: 0,
        ...extra,
    });
}

/** Collect virtio console output; `wait` resolves when `needle` shows up. */
function console_of(emulator)
{
    let text = "";
    const waiters = [];
    emulator.add_listener("virtio-console0-output-bytes", bytes => {
        const s = new TextDecoder().decode(bytes);
        text += s;
        process.stdout.write(s);
        for(const w of waiters.splice(0))
        {
            if(text.includes(w.needle)) { clearTimeout(w.timer); w.resolve(); }
            else waiters.push(w);
        }
    });
    // Like snowglobe: only send when the guest has a receive buffer queued,
    // otherwise v86's virtio console drops the input.
    const pending = [];
    const pump = () => {
        const rx = emulator.v86.cpu.devices.virtio_console.virtio.queues[0];
        while(pending.length && rx.has_request())
        {
            emulator.bus.send("virtio-console0-input-bytes", pending.shift());
        }
        if(pending.length) setTimeout(pump, 5);
    };
    return {
        wait: (needle, ms = Number(process.env.WAIT_MS || 300_000)) => new Promise(resolve => {
            if(text.includes(needle)) return resolve();
            const timer = setTimeout(() => { throw new Error("timeout waiting for " + JSON.stringify(needle)); }, ms);
            waiters.push({ needle, resolve, timer });
        }),
        send: line => {
            const bytes = new TextEncoder().encode(line + "\n");
            for(let i = 0; i < bytes.length; i += 64) pending.push(bytes.subarray(i, i + 64));
            pump();
        },
    };
}

const t0 = Date.now();
let serial = "";
process.on("uncaughtException", e => {
    console.log("\n[snowglobe.mjs] FAIL: " + e.message + "\n--- serial (kernel) log tail ---\n" + serial.slice(-3000));
    process.exit(1);
});
const log = msg => console.log(`\n[snowglobe.mjs +${((Date.now() - t0) / 1000).toFixed(1)}s] ${msg}`);

// ---- 1. boot from the 9p filesystem
let emulator = make({
    bzimage: { url: path.join(dir, "vmlinuz") },
    cmdline: "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose init=/init console=ttyS0 quiet",
});
emulator.add_listener("serial0-output-byte", b => { serial += String.fromCharCode(b); });
let con = console_of(emulator);

await con.wait("READY");
log("guest is up on hvc0 (root on 9p)");
con.send("cat /etc/motd; echo sum-$((40+2)); ls /bin | wc -l");
await con.wait("sum-42");
await con.wait("403");
log("console round trip ok; saving state");

const state = new Uint8Array(await emulator.save_state());
emulator.destroy();
const raw_path = path.join(dir, "state.bin");
fs.writeFileSync(raw_path, state);
execFileSync("python3", ["-c", `import zstandard,sys; open(sys.argv[2],'wb').write(zstandard.ZstdCompressor(level=3).compress(open(sys.argv[1],'rb').read()))`, raw_path, raw_path + ".zst"]);
log(`state: ${(state.length / 1048576).toFixed(1)} MiB raw, ${(fs.statSync(raw_path + ".zst").size / 1048576).toFixed(1)} MiB zstd`);

// ---- 2. restore the snapshot into a fresh emulator and continue
emulator = make({ initial_state: { url: raw_path + ".zst" } });
con = console_of(emulator);
await new Promise(resolve => emulator.add_listener("emulator-started", resolve));
log("restored from snapshot");
con.send("echo restored-$((6*7)) $(uname -m); cat /etc/motd");
await con.wait("restored-42 aarch64");
await con.wait("hello from 9p");
log("restored guest answers on hvc0");

// ---- 3. networking after restore: virtio-net + the fetch relay. The guest
// reaches a local HTTP server as http://PORT.external/ (fetch_network.js).
const server = http.createServer((req, res) => {
    res.end("pong " + req.url + " from the host\n");
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
con.send("ip addr add 192.168.86.100/24 dev eth0; ip link set eth0 up; " +
         "ip route add default via 192.168.86.1; echo nameserver 192.168.86.1 > /etc/resolv.conf; echo net-up");
await con.wait("net-up");
con.send(`wget -q -O - http://${port}.external/ping-arm64`);
await con.wait("pong /ping-arm64 from the host");
server.close();
log("guest fetched from the host over virtio-net (fetch relay)");

const halted = new Promise(resolve => emulator.add_listener("cpu-event-halt", resolve));
con.send("poweroff");
await halted;
emulator.destroy();
log("PASS");
process.exit(0);
