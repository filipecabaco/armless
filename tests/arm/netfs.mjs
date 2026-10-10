#!/usr/bin/env node
// Networking and 9p stress test for the arm64 machine, through the V86 API
// the way snowglobe uses it: 9p root with zstd blobs, the virtio console and
// virtio-net with the fetch relay.
//
//   node tests/arm/netfs.mjs DIR      (DIR from tests/arm/make-9pfs.sh)
//
// Checks data integrity (checksums on both sides) and prints timings.
// ARMLESS_JIT=0 runs without the JIT.

import fs from "node:fs";
import path from "node:path";
import http from "node:http";
import crypto from "node:crypto";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { V86 } = await import(path.resolve(root, process.env.V86_MODULE || "src/main.js"));
const dir = path.resolve(process.argv[2] || ".");

const SHORT = Number(process.env.NETFS_TIMEOUT || 300_000);
const LONG = 2 * SHORT;
const md5 = buf => crypto.createHash("md5").update(buf).digest("hex");

// The same 8 MiB as /data/big.bin in the image (see make-9pfs.sh).
const big = Buffer.alloc(8 * 1024 * 1024);
for(let i = 0; i < big.length / 32; i++)
{
    const n = Buffer.alloc(8);
    n.writeBigUInt64LE(BigInt(i));
    crypto.createHash("sha256").update(n).digest().copy(big, i * 32);
}
const BIG_MD5 = md5(big);

function make(extra)
{
    return new V86({
        arch: "arm64",
        disable_jit: process.env.ARMLESS_JIT === "0",
        cpu_count: Number(process.env.ARMLESS_CPUS || 1),
        wasm_path: path.join(root, "build/armless.wasm"),
        memory_size: 256 * 1024 * 1024,
        filesystem: {
            basefs: { url: path.join(dir, "fs.json") },
            baseurl: path.join(dir, "blobs") + "/",
        },
        virtio_console: true,
        net_device: { type: "virtio", relay_url: "fetch", ...(process.env.NET_MTU ? { mtu: Number(process.env.NET_MTU) } : {}) },
        autostart: true,
        disable_keyboard: true,
        disable_mouse: true,
        disable_speaker: true,
        log_level: 0,
        ...extra,
    });
}

function console_of(emulator)
{
    let text = "";
    const waiters = [];
    emulator.add_listener("virtio-console0-output-bytes", bytes => {
        text += new TextDecoder().decode(bytes);
        for(const w of waiters.splice(0))
        {
            const m = text.match(w.re);
            if(m) { clearTimeout(w.timer); text = text.slice(m.index + m[0].length); w.resolve(m); }
            else waiters.push(w);
        }
    });
    const pending = [];
    const pump = () => {
        const rx = emulator.v86.cpu.devices.virtio_console.virtio.queues[0];
        while(pending.length && rx.has_request())
        {
            emulator.bus.send("virtio-console0-input-bytes", pending.shift());
        }
        if(pending.length) setTimeout(pump, 5);
    };
    const send = line => {
        const bytes = new TextEncoder().encode(line + "\n");
        for(let i = 0; i < bytes.length; i += 64) pending.push(bytes.subarray(i, i + 64));
        pump();
    };
    let seq = 0;
    return {
        wait: (re, ms = SHORT) => new Promise((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error("timeout waiting for " + re + "; console: " + JSON.stringify(text.slice(-500)))), ms);
            waiters.push({ re, resolve, timer });
            const m = text.match(re);
            if(m) { waiters.pop(); clearTimeout(timer); text = text.slice(m.index + m[0].length); resolve(m); }
        }),
        /** Run a shell command; resolves with its output and the time it took. */
        run: async function(cmd, ms)
        {
            const id = ++seq;
            const t = performance.now();
            send(`echo "<<${id}"; ${cmd}; echo ">>${id}"`);
            const m = await this.wait(new RegExp(`<<${id}\\r?\\n([\\s\\S]*?)>>${id}\\r?\\n`), ms);
            return { out: m[1].replace(/\r/g, ""), secs: (performance.now() - t) / 1000 };
        },
    };
}

let failures = 0;
const results = [];
let last_insns = 0;
function insns_since_last()
{
    if(!process.env.ARMLESS_STATS || !emulator) return "";
    const e = emulator.v86.arm_cpu.exports;
    const n = Number(e["armless_insn_count"]());
    const d = n - last_insns;
    last_insns = n;
    return `, ${(d / 1e6).toFixed(0)}M insns, ${(e["armless_jit_stat"](5) / Math.max(n, 1) * 100).toFixed(0)}% in JIT so far`;
}
function check(name, ok, detail, secs, bytes)
{
    const rate = (bytes && secs ? ` (${(bytes / 1048576 / secs).toFixed(2)} MiB/s)` : "") + insns_since_last();
    const line = `${ok ? "ok  " : "FAIL"} ${name}: ${secs !== undefined ? secs.toFixed(2) + "s" : ""}${rate}${ok ? "" : " — " + detail}`;
    console.log(line);
    results.push(line);
    if(!ok) failures++;
}

process.on("uncaughtException", e => { console.log("FAIL: " + e.message); process.exit(1); });

// ------------------------------------------------------------------ boot
const t0 = performance.now();
let emulator = make({
    bzimage: { url: path.join(dir, "vmlinuz") },
    cmdline: `rw root=host9p rootfstype=9p rootflags=${process.env.NETFS_ROOTFLAGS || "trans=virtio,cache=loose"} init=/init console=ttyS0 quiet`,
});
let con = console_of(emulator);
await con.wait(/READY/);
console.log(`booted in ${((performance.now() - t0) / 1000).toFixed(1)}s`);

// Probe mode: NETFS_EXEC="cmd1 ;; cmd2" runs the commands, prints output,
// time and guest instructions for each, and exits.
if(process.env.NETFS_EXEC)
{
    for(const cmd of process.env.NETFS_EXEC.split(";;"))
    {
        const e = emulator.v86.arm_cpu.exports;
        const stat = () => [Number(e["armless_insn_count"]()), e["armless_jit_stat"](5), e["armless_jit_stat"](6), e["armless_jit_stat"](2)];
        const before = stat();
        const { out, secs } = await con.run(cmd, LONG);
        const d = stat().map((v, i) => (v - before[i]) / 1e6);
        console.log(`$ ${cmd.trim()}\n${out}[${secs.toFixed(2)}s, ${d[0].toFixed(1)}M insns, ` +
            `${d[1].toFixed(1)}M in JIT blocks (${d[2].toFixed(2)}M via helper), ${d[3].toFixed(2)}M block runs]`);
    }
    process.exit(0);
}

// ------------------------------------------------------------------- 9p
let r;
const only = process.env.NETFS_ONLY || "";   // "net" or "fs" to run one part
if(only !== "net")
{
r = await con.run("md5sum /data/big.bin");
check("9p read 8 MiB (zstd blob)", r.out.startsWith(BIG_MD5), r.out, r.secs, big.length);

r = await con.run("md5sum /data/big.bin");
check("9p read 8 MiB again (cached)", r.out.startsWith(BIG_MD5), r.out, r.secs, big.length);

let small_total = 0;
for(let i = 0; i < 300; i++) small_total += `small file ${i}\n`.length * (i % 7 + 1);
r = await con.run("cat /data/small/* | wc -c");
check("9p read 300 small files", r.out.trim() === String(small_total), r.out, r.secs);

r = await con.run("dd if=/data/big.bin of=/tmp/copy.bin bs=65536 2>/dev/null; sync; md5sum /tmp/copy.bin");
check("9p write 8 MiB", r.out.startsWith(BIG_MD5), r.out, r.secs, big.length);
const host_copy = await emulator.read_file("/tmp/copy.bin");
check("9p written file seen by the host", md5(host_copy) === BIG_MD5, `host md5 ${md5(host_copy)} len ${host_copy.length}`);

r = await con.run("mkdir /tmp/m && for i in $(seq 300); do echo $i > /tmp/m/$i; done; ls /tmp/m | wc -l; cat /tmp/m/* | wc -l; rm -r /tmp/m; ls /tmp/m 2>&1 | head -1");
check("9p create/list/remove 300 files", r.out === "300\n300\nls: /tmp/m: No such file or directory\n", JSON.stringify(r.out), r.secs);

r = await con.run("cd /tmp && echo abc > a && echo def >> a && ln -s a b && mv a c; cat b 2>/dev/null || echo dangling; " +
    "cat c; ln -sf c b; cat b; truncate -s 2 c; wc -c < c; ln c d; stat -c %h c; echo xy >> d; cat c; " +
    "chmod 640 c; stat -c %a d; mkdir -p x/y/z && mv x w && ls w/y | cat; rm -r w b c d; ls /tmp | cat");
check("9p semantics (append, symlink, rename, truncate, hard link, chmod, dirs)",
    r.out === "dangling\nabc\ndef\nabc\ndef\n2\n2\nabxy\n640\nz\ncopy.bin\n", JSON.stringify(r.out), r.secs);

r = await con.run("head -c 1048576 /data/big.bin > /tmp/sparse; dd if=/dev/zero of=/tmp/sparse bs=1 count=1 seek=3000000 conv=notrunc 2>/dev/null; " +
    "wc -c < /tmp/sparse; dd if=/tmp/sparse bs=4096 skip=256 count=1 2>/dev/null | md5sum; rm /tmp/sparse");
check("9p write past EOF and read the hole", r.out.startsWith("3000001\n" + md5(Buffer.alloc(4096))), JSON.stringify(r.out), r.secs);

}

// ------------------------------------------------------------- network
if(only !== "fs")
{
const uploads = [];
const server = http.createServer((req, res) => {
    if(req.url === "/big")
    {
        res.end(big);
    }
    else if(req.url.startsWith("/part/"))
    {
        const n = Number(req.url.slice(6));
        res.end(big.subarray(n << 20, (n + 1) << 20));
    }
    else if(req.url === "/upload")
    {
        const chunks = [];
        req.on("data", c => chunks.push(c));
        req.on("end", () => {
            const body = Buffer.concat(chunks);
            uploads.push(body.length);
            res.end(`upload ${md5(body)} ${body.length}\n`);
        });
    }
    else
    {
        res.end("pong " + req.url + "\n");
    }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
const host = `http://${server.address().port}.external`;

r = await con.run("ip addr add 192.168.86.100/24 dev eth0; ip link set eth0 up; ip route add default via 192.168.86.1; echo nameserver 192.168.86.1 > /etc/resolv.conf; echo up");
check("network up", r.out.includes("up"), r.out);

r = await con.run(`wget -q -O - ${host}/ping`);
check("HTTP GET small", r.out === "pong /ping\n", JSON.stringify(r.out), r.secs);

r = await con.run(`wget -q -O - ${host}/big | md5sum`, LONG);
check("HTTP GET 8 MiB", r.out.startsWith(BIG_MD5), r.out, r.secs, big.length);

r = await con.run(`for i in 0 1 2 3; do (wget -q -O - ${host}/part/$i | md5sum > /tmp/p$i) & done; wait; cat /tmp/p0 /tmp/p1 /tmp/p2 /tmp/p3; rm /tmp/p?`, LONG);
const parts = [0, 1, 2, 3].map(i => md5(big.subarray(i << 20, (i + 1) << 20)));
check("HTTP 4 parallel GETs of 1 MiB", parts.every(p => r.out.includes(p)), r.out, r.secs, 4 << 20);

// busybox wget sends --post-file as a C string: no NUL bytes in the body
const upload = Buffer.from(big.subarray(0, 2 << 20)).map(b => b || 1);
r = await con.run(`head -c 2097152 /data/big.bin | tr '\\000' '\\001' > /tmp/up.bin; wget -q -O - --post-file /tmp/up.bin ${host}/upload; rm /tmp/up.bin`, LONG);
check("HTTP POST 2 MiB", r.out === `upload ${md5(upload)} ${2 << 20}\n`, JSON.stringify(r.out), r.secs, 2 << 20);

r = await con.run(`for i in $(seq 20); do wget -q -O - ${host}/n$i; done | wc -l`, LONG);
check("HTTP 20 sequential requests", r.out.trim() === "20", r.out, r.secs);
server.close();
}

// ------------------------------------------- snapshot with written files
r = await con.run("echo persisted > /tmp/note; sync; echo ok");
const state = await emulator.save_state();
emulator.destroy();
emulator = make({ initial_state: { buffer: state } });
con = console_of(emulator);
await new Promise(resolve => emulator.add_listener("emulator-started", resolve));
r = await con.run("cat /tmp/note; md5sum /tmp/copy.bin 2>/dev/null | cut -d' ' -f1");
check("files written before save_state survive restore",
    r.out === `persisted\n${only === "net" ? "" : BIG_MD5 + "\n"}`, JSON.stringify(r.out), r.secs);

emulator.destroy();
console.log(failures ? `\n${failures} check(s) FAILED` : "\nall checks passed");
if(process.env.GITHUB_STEP_SUMMARY)
{
    fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, "### armless network and 9p\n```\n" + results.join("\n") + "\n```\n");
}
process.exit(failures ? 1 : 0);
