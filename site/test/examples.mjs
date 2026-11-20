#!/usr/bin/env node
// Runs what each example page does, with the page's assets (site/static/demo) and armless.js from
// the release, and checks what a visitor would see:
//
// - native-snapshot: the native snapshot restores and says where it was saved
// - fresh-randomness: two restores of it read different bytes from /dev/urandom
// - no-kernel: sh in user mode, ps and /proc/self/status; and the race, the same python3 with no
//   kernel and under Linux (demo/py), each answering a line with its own release
// - time-asleep: a saved process's pending sleep comes due at once when restored told it slept
//   past it, and keeps its time left when told nothing
// - egress: allow() is asked for each request, and a refused one is a 502 in the guest (a private
//   address, so nothing leaves this machine)
//
//   node site/test/examples.mjs [DEMO_DIR]
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.resolve(process.argv[2] || path.join(here, "../static/demo"));
const config = JSON.parse(fs.readFileSync(path.join(dir, "config.json"), "utf8"));
const { Armless } = createRequire(import.meta.url)(path.join(dir, "armless.js"));
const at = (f) => path.join(dir, f);
const plain = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");
const fsopt = { json: at("fs/filesystem.json"), baseurl: at("fs/blobs") + "/" };
const env = ["PATH=/bin", "HOME=/root", "TERM=xterm", "PS1=~ # "];
const user = { memory_mb: 64, filesystem: fsopt, user: { argv: ["sh"], env, cwd: "/root" } };

const t0 = performance.now();
const secs = () => ((performance.now() - t0) / 1000).toFixed(1) + "s";
let failed = false;

async function machine(options) {
  const vm = new Armless({ wasm: at("armless.wasm"), memory_mb: config.memory_mb, cpus: 1, ...options });
  const decoder = new TextDecoder();
  let out = "";
  vm.on("console", (b) => { out += decoder.decode(b, { stream: true }); });
  await vm.ready;
  return {
    vm,
    type: (text) => vm.console_input(text),
    // Wait for the console to match
    async wait(re, what, timeout = 60_000) {
      const end = performance.now() + timeout;
      while (performance.now() < end) {
        const m = re.exec(plain(out));
        if (m) return m;
        await new Promise((r) => setTimeout(r, 50));
      }
      throw new Error(`no ${what}\n--- console ---\n${plain(out).slice(-800)}`);
    },
    text: () => plain(out),
  };
}

async function example(name, fn) {
  try {
    await fn();
    console.log(`ok ${secs()}: ${name}`);
  } catch (e) {
    failed = true;
    console.error(`FAIL ${secs()}: ${name}: ${e.message}`);
  }
}

if (config.native) {
  await example("native-snapshot: restores and says where it was saved", async () => {
    const m = await machine({ initial_state: at("native.bin.zst") });
    m.type("cat /root/origin\n");
    await m.wait(/^Booted and saved by a native armless-run on \S+/m, "the origin line");
    m.vm.destroy();
  });

  await example("fresh-randomness: two copies read different bytes", async () => {
    const reads = [];
    for (let i = 0; i < 2; i++) {
      const m = await machine({ initial_state: at("native.bin.zst") });
      m.type("head -c 16 /dev/urandom | od -An -tx1\n");
      reads.push((await m.wait(/^((?: [0-9a-f]{2}){16})\s*$/m, "16 random bytes"))[1]);
      m.vm.destroy();
    }
    if (reads[0] === reads[1]) throw new Error(`both copies read ${reads[0]}`);
  });
} else {
  console.log("skip: no native snapshot in this build (native-snapshot, fresh-randomness)");
}

await example("no-kernel: sh in user mode lists its processes and its memory", async () => {
  const m = await machine(user);
  await m.wait(/# $/m, "a prompt");
  m.type("uname -s; ps; grep VmRSS /proc/self/status\n");
  await m.wait(/PID\s+USER[\s\S]*\bsh\b[\s\S]*VmRSS:\s+\d+ kB/, "ps and VmRSS");
  m.vm.destroy();
});

await example("time-asleep: a pending sleep comes due on a restore told it slept, not on one told nothing", async () => {
  const m = await machine(user);
  await m.wait(/# $/m, "a prompt");
  m.type("(sleep 20; echo TIMER-FIRED) & echo STARTED\n");
  await m.wait(/^STARTED/m, "the timer set");
  await m.vm.stop();
  const state = await m.vm.save_state();
  const process = await m.vm.save_process();
  m.vm.destroy();

  const told = await machine({ ...user, user: undefined, initial_state: state, user_state: process, user_asleep_ms: 60_000 });
  await told.wait(/TIMER-FIRED\r?\n/, "the timer, at once", 5_000);
  told.vm.destroy();

  const untold = await machine({ ...user, user: undefined, initial_state: state, user_state: process, user_asleep_ms: 0 });
  await new Promise((r) => setTimeout(r, 4_000));
  if (/TIMER-FIRED\r?\n/.test(untold.text())) throw new Error("the timer fired with time still left");
  untold.vm.destroy();
});

await example("egress: allow() is asked, and a refused request is a 502 in the guest", async () => {
  const asked = [];
  const m = await machine({
    boot: { kernel: at("Image.gz"), initrd: at("initrd.cpio"), cmdline: config.cmdline },
    network: { allow: ({ host }) => { asked.push(host); return !Armless.private_host(host); } },
  });
  await m.wait(/# $/m, "a prompt");
  m.type("wget -qO- http://10.0.0.1/; echo RC=$?\n");
  await m.wait(/502 Bad Gateway[\s\S]*^RC=1/m, "a 502 for the private address");
  if (!asked.includes("10.0.0.1")) throw new Error(`allow() was asked about ${JSON.stringify(asked)}`);
  m.vm.destroy();
});

if (config.python) {
  await example("no-kernel: the same python3 with no kernel and under Linux, os.stat and a child in both", async () => {
    const py = { json: at("py/filesystem.json"), baseurl: at("py/blobs") + "/" };
    const line = 'import os, subprocess; print("RACE", os.uname().release, os.stat("/usr/bin/python3").st_size > 0, subprocess.run(["sh", "-c", "exit 7"]).returncode)\n';
    const sides = [
      [{ user: { argv: ["python3"], env: ["PATH=/usr/bin:/bin", "HOME=/root", "TERM=xterm"], cwd: "/root" } }, "6.6.0-armless-user"],
      [{ boot: { kernel: at("Image.gz"), initrd: at("initrd.cpio"), cmdline: `${config.cmdline} python` } }, config.linux],
    ];
    for (const [options, release] of sides) {
      const m = await machine({ filesystem: py, ...options });
      await m.wait(/>>> $/, "python3's prompt");
      m.type(line);
      const [, got, ok, code] = await m.wait(/^RACE (\S+) (\S+) (\d+)\s*$/m, "python3's answer");
      if (!got.startsWith(release) || ok !== "True" || code !== "7") throw new Error(`said ${got} ${ok} ${code}, wanted ${release} True 7`);
      m.vm.destroy();
    }
  });
} else {
  console.log("skip: no-kernel's race (no python filesystem in this build)");
}

console.log(failed ? `examples.mjs: failed (${secs()})` : `examples.mjs: passed in ${secs()}`);
process.exit(failed ? 1 : 0);
