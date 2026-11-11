#!/usr/bin/env node
// User mode (no guest kernel) through armless.js, in Node.
//
//   node tests/js/user.mjs SITE [EXPR]
//       SITE: a site snowglobe built with `--kernel none` (SITE/system: state.bin.zst, user.bin,
//       filesystem.json, filesystem/), its program a REPL that answers a line (python3, say).
//       Restores the machine and the process a native host saved, types EXPR (default 6*7) and
//       waits for the answer; saves the pair again, restores it into a new machine, asks again
//       and has the program exit (with 3).
//   node tests/js/user.mjs --exec FSDIR PROGRAM [ARG...]
//       FSDIR: filesystem.json and filesystem/ (tests/fs/make-9pfs.sh, or a site's system/).
//       Starts PROGRAM from it with no kernel and prints its output and exit status.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));
const wasm = path.join(root, "build/armless.wasm");
const decode = (b) => new TextDecoder().decode(b);
const plain = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]|\x1b[=>]|\r/g, "");

function fail(msg) {
  console.error("FAIL: " + msg);
  process.exit(1);
}

/** The next `name` event of `vm`. */
function once(vm, name) {
  return new Promise((resolve) => {
    const on = (v) => {
      vm.off(name, on);
      resolve(v);
    };
    vm.on(name, on);
  });
}

/** Console text from `vm` until `done(text)`, within `ms`. */
function until(vm, done, ms, what) {
  return new Promise((resolve) => {
    let text = "";
    const timer = setTimeout(() => fail(`${what}: nothing after ${ms} ms; got ${JSON.stringify(plain(text))}`), ms);
    const on = (b) => {
      text += decode(b);
      if (done(plain(text))) {
        clearTimeout(timer);
        vm.off("console", on);
        resolve(plain(text));
      }
    };
    vm.on("console", on);
  });
}

if (process.argv[2] === "--exec") {
  const dir = path.resolve(process.argv[3]);
  const argv = process.argv.slice(4);
  const t0 = performance.now();
  const vm = new Armless({
    wasm, memory_mb: 256, autostart: false,
    filesystem: { json: path.join(dir, "filesystem.json"), baseurl: path.join(dir, "filesystem") + "/" },
    user: { argv, env: ["PATH=/usr/local/bin:/usr/bin:/bin", "HOME=/root", "TERM=xterm"], cwd: "/", cols: 100, rows: 30 },
  });
  let out = "";
  vm.on("console", (b) => process.stdout.write(b) && (out += decode(b)));
  await vm.ready;
  vm.close_input();
  const stopped = new Promise((r) => vm.on("stop", r));
  vm.run();
  const s = await stopped;
  console.log(`\n[${argv.join(" ")}: ${JSON.stringify(s)} after ${(performance.now() - t0).toFixed(0)} ms]`);
  vm.destroy();
  process.exit(s.reason === "exit" ? s.code : 1);
}

const site = path.resolve(process.argv[2] || fail("usage: user.mjs SITE [EXPR] | --exec FSDIR PROGRAM [ARG...]"));
const expr = process.argv[3] || "6*7";
const want = String(eval(expr));
const sys = path.join(site, "system");
const run = JSON.parse(fs.readFileSync(path.join(site, "run.json"), "utf8"));
const config = {
  wasm, memory_mb: run.memory_mb, cpus: 1, jit: run.jit !== false, autostart: false,
  filesystem: { json: path.join(sys, "filesystem.json"), baseurl: path.join(sys, "filesystem") + "/" },
};

/** Ask the REPL `expr` and wait for its answer and next prompt. */
async function ask(vm, label) {
  const t = performance.now();
  const answered = until(vm, (s) => s.includes("\n" + want + "\n") && /\n.*>>> ?$/.test(s.trimEnd() + " "), 60000, label);
  const idle = once(vm, "idle");
  vm.console_input(expr + "\r");
  const text = await answered;
  await idle;
  console.log(`${label}: ${expr} = ${want} in ${(performance.now() - t).toFixed(0)} ms; status ${JSON.stringify(vm.user_status)}`);
  return text;
}

let t0 = performance.now();
let blobs = 0;
const vm = new Armless({ ...config, initial_state: path.join(sys, "state.bin.zst"), user_state: path.join(sys, "user.bin") });
vm.on("blob", () => blobs++);
vm.on("stop", (s) => fail("stopped: " + JSON.stringify(s)));
await vm.ready;
console.log(`restored ${run.title} (user mode) in ${(performance.now() - t0).toFixed(0)} ms`);
vm.console_resize(100, 30);
vm.run();
await ask(vm, "restored");
console.log(`  ${blobs} blobs fetched, ${vm.instruction_count.toFixed(0)} instructions`);

// The pair again, from this machine: the machine's state and the process's
await vm.stop();
const state = await vm.save_state();
const proc = await vm.save_process();
vm.destroy();
console.log(`saved: machine ${(state.length / 1048576).toFixed(1)} MiB, process ${proc.length} bytes`);

t0 = performance.now();
const vm2 = new Armless({ ...config, initial_state: state, user_state: proc });
const stop2 = (s) => fail("stopped after the restore: " + JSON.stringify(s));
vm2.on("stop", stop2);
await vm2.ready;
console.log(`restored the saved pair in ${(performance.now() - t0).toFixed(0)} ms`);
vm2.run();
await ask(vm2, "re-restored");

// Ctrl-C: typed, through the terminal, then sent with kill(); each interrupts a loop that never
// ends by itself, and the REPL answers after
for (const [how, interrupt] of [["^C", () => vm2.console_input("\x03")], ["kill()", () => vm2.kill()]]) {
  const t = performance.now();
  const interrupted = until(vm2, (s) => s.includes("KeyboardInterrupt") && /\n.*>>> ?$/.test(s.trimEnd() + " "), 60000, how);
  vm2.console_input("while True: pass\r");
  await new Promise((r) => setTimeout(r, 500));
  interrupt();
  await interrupted;
  console.log(`${how}: KeyboardInterrupt in ${(performance.now() - t - 500).toFixed(0)} ms`);
  await ask(vm2, `after ${how}`);
}

// Its end: the program exits, and "stop" says with what
vm2.off("stop", stop2);
const stopped = once(vm2, "stop");
vm2.console_input("raise SystemExit(3)\r");
const s = await stopped;
console.log(`exit: ${JSON.stringify(s)}`);
if (s.reason !== "exit" || s.code !== 3) fail("did not exit with 3");
vm2.destroy();
console.log("PASS");
