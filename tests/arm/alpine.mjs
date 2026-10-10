#!/usr/bin/env node
// Boot a real arm64 Alpine (linux-virt, OpenRC) from a v86 9p filesystem the
// way snowglobe does: bzimage_initrd_from_filesystem, root=host9p, a shell on
// ttyS0, then save_state and restore.
//
//   node tests/arm/alpine.mjs DIR    (DIR: fs.json + blobs/ from tests/arm/alpine/)
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { V86 } = await import(path.resolve(root, process.env.V86_MODULE || "src/main.js"));
const dir = path.resolve(process.argv[2] || ".");
const t0 = Date.now();
const log = msg => {
    const line = `+${((Date.now() - t0) / 1000).toFixed(1)}s ${msg}`;
    console.log(`\n[alpine.mjs ${line}]`);
    if(process.env.GITHUB_ACTIONS) console.log(`::notice title=alpine::${line.replace(/[\r\n%]/g, " ")}`);
};

let serial = "";
function fail(msg)
{
    console.log(`\n[alpine.mjs] FAIL: ${msg}\n--- serial tail ---\n${serial.slice(-4000)}`);
    if(process.env.GITHUB_ACTIONS)
    {
        const tail = serial.slice(-1800).replace(/[\r%]/g, "").replace(/\n/g, ";");
        console.log(`::error title=alpine: ${msg}::${tail}`);
    }
    process.exit(1);
}

function make(extra)
{
    return new V86({
        arch: "arm64",
        wasm_path: path.join(root, "build/armless.wasm"),
        memory_size: 512 * 1024 * 1024,
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

function wait_for(needle, ms)
{
    return new Promise(resolve => {
        const start = serial.length - needle.length;
        const timer = setTimeout(() => fail(`timeout waiting for ${JSON.stringify(needle)}`), ms);
        const poll = () => {
            if(serial.indexOf(needle, Math.max(0, start)) >= 0) { clearTimeout(timer); resolve(); }
            else setTimeout(poll, 50);
        };
        poll();
    });
}

let emulator = make({
    bzimage_initrd_from_filesystem: true,
    cmdline: "rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose modules=virtio_pci " +
             "tsc=reliable console=ttyS0 init_on_free=on",
});
emulator.add_listener("serial0-output-byte", b => {
    const c = String.fromCharCode(b);
    serial += c;
    if(process.env.VERBOSE) process.stdout.write(c);
});

await wait_for("armless:~#", 20 * 60_000);
log("Alpine booted to a root shell on ttyS0");
emulator.serial0_send("uname -m; cat /etc/alpine-release; echo ok-$((20+22)); ls /lib/modules\n");
await wait_for("ok-42", 120_000);
if(!serial.includes("aarch64")) fail("uname -m is not aarch64");
log("commands run: " + serial.slice(serial.lastIndexOf("uname -m")).split("\n").slice(1, 5).join(" | "));

emulator.serial0_send("sync; echo 3 > /proc/sys/vm/drop_caches; echo cache-$((40+2))\n");
await wait_for("cache-42", 120_000);
const state = await emulator.save_state();
emulator.destroy();
log(`snapshot ${(state.byteLength / 1048576).toFixed(1)} MiB`);

serial = "";
emulator = make({ initial_state: { buffer: state } });
emulator.add_listener("serial0-output-byte", b => { serial += String.fromCharCode(b); });
await new Promise(resolve => emulator.add_listener("emulator-started", resolve));
emulator.serial0_send("echo restored-$((6*7)) $(uname -r)\n");
await wait_for("restored-42", 120_000);
log("restored snapshot answers: " + serial.slice(serial.indexOf("restored-42")).split("\n")[0]);
emulator.destroy();
log("PASS");
process.exit(0);
