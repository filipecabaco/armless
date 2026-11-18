// The user-mode API of armless.js (`user` and its options, `user_state`, `user_asleep_ms`,
// restore_state), driving build/armless.wasm the way a page does. Self-contained: the programs
// are machine code made here (prog.mjs) and the filesystem is in memory, so it needs no
// network, toolchain or snowglobe site. Build the module first (sh build-wasm.sh), then:
//
//   node --test 'tests/js/*.test.mjs'
//
// (tests/js/user.mjs is the same API against a real program: a snowglobe site, run by hand.)
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { Prog, I, D, W, X, filesystem } from "./prog.mjs";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const { Armless } = require(path.join(root, "js/armless.js"));
const wasm = path.join(root, "build/armless.wasm");
if (!fs.existsSync(wasm)) throw new Error(`${wasm} is missing: build it with sh build-wasm.sh`);

const AT_FDCWD = 0xffff_ff9c;
const TCGETS = 0x5401;
const WNOHANG = 1;
const ENOTTY = 25;
const ECHILD = 10;
const CLOCK_MONOTONIC = 1;
/** How long the clock program sleeps between its two readings of the monotonic clock */
const NAP_MS = 1000;

// ------------------------------------------------------------------ the programs

/**
 * Writes its argv and env from its initial stack, each item with its NUL, the two lists apart by
 * "|": a loop over each NULL-terminated array of pointers, a write(1) of each string
 */
function args_prog() {
  const p = new Prog();
  p.put(0x240, "|");
  p.code.push(0x910003f3, 0x91002273); // mov x19, sp; add x19, x19, #8: argv[0]'s slot
  for (const list of ["argv", "env"]) {
    const next = p.code.length;
    p.code.push(0xf8408661); // ldr x1, [x19], #8
    const end = p.code.length;
    p.code.push(0); // cbz x1, past this list, once it is known
    p.li(2, 0);
    p.code.push(0x38626823, 0x91000442, 0x35ffffc3); // ldrb w3, [x1, x2]; add x2, x2, #1; cbnz w3, back 2
    p.li(0, 1);
    p.sys(64); // write(1, x1, x2): the string and its NUL
    p.code.push((0x14000000 | ((next - p.code.length) & 0x3ffffff)) >>> 0); // b next
    p.code[end] = (0xb4000000 | ((p.code.length - end) << 5) | 1) >>> 0;
    if (list === "argv") p.sys(64, [I(1), D(0x240), I(1)]);
  }
  return p.elf();
}

/** Writes uname's nodename: the 65 bytes of its field */
function uname_prog() {
  return new Prog().sys(160, [D(0x400)]).sys(64, [I(1), D(0x400 + 65), I(65)]).elf();
}

/** Exits with what TCGETS on stdout gives: 0 for a terminal, ENOTTY's code without one */
function tty_prog() {
  return new Prog().sys(29, [I(1), I(TCGETS), D(0x300)]).exit_x0().elf();
}

/**
 * The program (pid 1000) forks A (1001), which forks B (1002) and ends at once; B sleeps 100 ms
 * and ends, an orphan. The program waits for A, then for the end of file of a pipe whose write
 * end B held (B has ended), and exits with what wait4(-1, WNOHANG) gives: B's pid when B is its
 * zombie, ECHILD when it reaps orphans.
 */
function orphans_prog() {
  const p = new Prog();
  p.timespec(0x200, 100);
  p.sys(59, [D(0x100), I(0)]); // pipe2: read end at 0x100, write end at 0x104
  const a = p.fork();
  const b = p.fork();
  p.sys(101, [D(0x200), I(0)]);
  p.parent(b);
  p.parent(a);
  p.sys(260, [I(-1), D(0x280), I(0), I(0)]);
  p.sys(57, [W(0x104)]);
  p.sys(63, [W(0x100), D(0x300), I(1)]);
  p.sys(260, [I(-1), D(0x280), I(WNOHANG), I(0)]).exit_x0();
  return p.elf();
}

/** Reads the monotonic clock, sleeps NAP_MS, reads it again and writes both timespecs (32 bytes) */
function clock_prog() {
  const p = new Prog();
  p.timespec(0x220, NAP_MS);
  p.sys(113, [I(CLOCK_MONOTONIC), D(0x200)]);
  p.sys(101, [D(0x220), I(0)]);
  p.sys(113, [I(CLOCK_MONOTONIC), D(0x210)]);
  p.sys(64, [I(1), D(0x200), I(32)]);
  return p.elf();
}

/** Writes "out\n" to stdout and "err\n" to stderr, and exits with status 3 */
function printer_prog() {
  const p = new Prog().put(0x200, "out\nerr\n");
  p.sys(64, [I(1), D(0x200), I(4)]).sys(64, [I(2), D(0x204), I(4)]);
  return p.sys(94, [I(3)]).elf();
}

/** Reads its stdin, writes what it got, reads again (end of file: 0) and writes "EOF\n" */
function stdin_prog() {
  const p = new Prog().put(0x200, "EOF\n");
  p.sys(63, [I(0), D(0x300), I(64)]).save(0x280);
  p.sys(64, [I(1), D(0x300), X(0x280)]);
  p.sys(63, [I(0), D(0x300), I(64)]).expect(0);
  return p.sys(64, [I(1), D(0x200), I(4)]).elf();
}

/** Sleeps 60 s; its child sleeps 400 ms and writes "child\n" (a child that outlived a kill) */
function sleeper_prog() {
  const p = new Prog().put(0x200, "child\n").timespec(0x220, 400).timespec(0x240, 60000);
  const child = p.fork();
  p.sys(101, [D(0x220), I(0)]).sys(64, [I(1), D(0x200), I(6)]);
  p.parent(child);
  return p.sys(101, [D(0x240), I(0)]).elf();
}

/** Never ends, never waits: a branch to itself */
function spin_prog() {
  const p = new Prog();
  p.code.push(0x1400_0000);
  return p.elf();
}

/** Sleeps a minute: a program for a machine to run its jobs beside */
function idle_prog() {
  return new Prog().timespec(0x200, 60000).sys(101, [D(0x200), I(0)]).elf();
}

const programs = {
  printer: printer_prog(),
  stdin: stdin_prog(),
  sleeper: sleeper_prog(),
  spin: spin_prog(),
  idle: idle_prog(),
  args: args_prog(),
  uname: uname_prog(),
  tty: tty_prog(),
  orphans: orphans_prog(),
  clock: clock_prog(),
  // Not an executable: its exec fails once its blob has come
  junk: new TextEncoder().encode("not an ELF\n"),
};

// ------------------------------------------------------------------ helpers

const decode = (b) => new TextDecoder().decode(b);

/** A machine for these programs; `user` (or `user_state`...) among `extra`. Not running yet. */
function machine(extra) {
  return new Armless({ wasm, memory_mb: 64, autostart: false, filesystem: filesystem(programs), ...extra });
}

/** Everything `vm` writes until it stops; runs it. Resolves to {stop, out} (out: bytes). */
function run_to_stop(vm, ms = 15000) {
  const chunks = [];
  const on = (b) => chunks.push(b.slice());
  vm.on("console", on);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`no stop after ${ms} ms; got ${JSON.stringify(decode(join(chunks)))}`)), ms);
    vm.on("stop", (stop) => {
      clearTimeout(timer);
      vm.off("console", on);
      resolve({ stop, out: join(chunks) });
    });
    vm.run();
  });
}

function join(chunks) {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const c of chunks) { out.set(c, at); at += c.length; }
  return out;
}

/** Runs `user` (the options of armless.js's `user`) to its end: {code, out} */
async function exec(user, extra = {}) {
  const vm = machine({ user, ...extra });
  try {
    await vm.ready;
    const { stop, out } = await run_to_stop(vm);
    assert.equal(stop.reason, "exit", JSON.stringify(stop));
    return { code: stop.code, out };
  } finally {
    vm.destroy();
  }
}

/** The second exec of a machine whose first one threw (the public API execs once a machine) */
async function exec_again(vm, user) {
  await vm._user_exec(user);
  const { stop, out } = await run_to_stop(vm);
  assert.equal(stop.reason, "exit", JSON.stringify(stop));
  return { code: stop.code, out };
}

const nodename = (out) => decode(out).replace(/\0+$/, "");

/**
 * WebAssembly.instantiate, for the machines made in `fn`, gives an instance whose exports are a
 * copy edited by `edit(exports, memory)`: a build without an export, or one that behaves otherwise.
 */
async function with_exports(edit, fn) {
  const instantiate = WebAssembly.instantiate;
  WebAssembly.instantiate = async (...args) => {
    const instance = await instantiate.apply(WebAssembly, args);
    const exports = { ...instance.exports };
    edit(exports, instance.exports.memory);
    return { exports };
  };
  try {
    return await fn();
  } finally {
    WebAssembly.instantiate = instantiate;
  }
}

/**
 * The clock program, saved while it sleeps: {state, process}, the machine's and the process's,
 * and the filesystem it ran from.
 */
async function saved_while_sleeping() {
  const vm = machine({ user: { argv: ["/bin/clock"], plain_std: true } });
  try {
    await vm.ready;
    vm.on("stop", (s) => assert.fail("stopped before it was saved: " + JSON.stringify(s)));
    vm.run();
    const begun = performance.now();
    for (;;) {
      const s = vm.user_status;
      if (s.state === "idle" && !s.waiting_on_blobs && s.wake_ms >= 0) break;
      assert.ok(performance.now() - begun < 5000, "not asleep: " + JSON.stringify(s));
      await new Promise((r) => setTimeout(r, 5));
    }
    await vm.stop();
    return { state: await vm.save_state(), process: await vm.save_process() };
  } finally {
    vm.destroy();
  }
}

/** The milliseconds between the clock program's two readings, from what it wrote */
function nap_of(out) {
  assert.equal(out.length, 32, "two timespecs");
  const v = new DataView(out.buffer, out.byteOffset, out.byteLength);
  const ms = (at) => Number(v.getBigUint64(at, true)) * 1e3 + Number(v.getBigUint64(at + 8, true)) / 1e6;
  return ms(16) - ms(0);
}

// ------------------------------------------------------------------ argv and env

test("argv and env reach the program, empty items kept", async () => {
  const { code, out } = await exec({ argv: ["/bin/args", "", "x", ""], env: ["A=1", "", "B=2"] });
  assert.equal(code, 0);
  assert.equal(decode(out), "/bin/args\0\0x\0\0|A=1\0\0B=2\0");
});

test("a NUL in an argument or a variable is refused, not split", async () => {
  for (const user of [{ argv: ["/bin/args", "a\0b"] }, { argv: ["/bin/args"], env: ["A=1\0B=2"] }]) {
    const vm = machine({ user });
    await assert.rejects(vm.ready, { message: "armless: user mode: a NUL in an argument or variable" });
    vm.destroy();
  }
});

// ------------------------------------------------------------------ plain_std and hostname

test("plain_std: stdout is no terminal; without it, it is one", async () => {
  assert.equal((await exec({ argv: ["/bin/tty"] })).code, 0);
  assert.equal((await exec({ argv: ["/bin/tty"], plain_std: true })).code, -ENOTTY);
});

test("hostname: what uname says, UTF-8, up to 64 bytes; armless by default", async () => {
  assert.equal(nodename((await exec({ argv: ["/bin/uname"] })).out), "armless");
  // plain_std: the field's bytes as they are
  for (const name of ["pod-7a", "pod-ü-ñ", "h".repeat(64)]) {
    assert.equal(nodename((await exec({ argv: ["/bin/uname"], hostname: name, plain_std: true })).out), name);
  }
});

test("hostname: one longer than 64 bytes is refused", async () => {
  // 33 characters, 66 bytes: the limit is in bytes
  for (const [name, bytes] of [["x".repeat(65), 65], ["é".repeat(33), 66]]) {
    const vm = machine({ user: { argv: ["/bin/uname"], hostname: name } });
    await assert.rejects(vm.ready, { message: `armless: user mode: hostname: ${bytes} bytes is longer than 64` });
    vm.destroy();
  }
});

test("hostname: written into the memory as it is after the call, which may have grown it", async () => {
  // A module that grows its memory in armless_user_hostname: the buffer from before is detached
  const { out } = await with_exports((e, memory) => {
    const hostname = e.armless_user_hostname;
    e.armless_user_hostname = (len) => {
      const at = hostname(len);
      memory.grow(1);
      return at;
    };
  }, () => exec({ argv: ["/bin/uname"], hostname: "grown", plain_std: true }));
  assert.equal(nodename(out), "grown");
});

test("exec options go back to the defaults after an exec that threw", async () => {
  // plain_std is set before the hostname is refused
  let vm = machine({ user: { argv: ["/bin/tty"], plain_std: true, hostname: "x".repeat(65) } });
  await assert.rejects(vm.ready, /longer than 64/);
  assert.equal((await exec_again(vm, { argv: ["/bin/tty"] })).code, 0, "stdout a terminal again");
  vm.destroy();

  // Both set while the executable is fetched, which then fails
  vm = machine({ user: { argv: ["/bin/junk"], plain_std: true, hostname: "pod-7a" } });
  await assert.rejects(vm.ready, /armless: user mode: \/bin\/junk/);
  assert.equal((await exec_again(vm, { argv: ["/bin/tty"] })).code, 0, "stdout a terminal again");
  vm.destroy();
  vm = machine({ user: { argv: ["/bin/junk"], plain_std: true, hostname: "pod-7a" } });
  await assert.rejects(vm.ready, /armless: user mode: \/bin\/junk/);
  assert.equal(nodename((await exec_again(vm, { argv: ["/bin/uname"] })).out), "armless");
  vm.destroy();
});

test("plain_std or hostname on a build without exec options is an error", async () => {
  for (const [missing, user] of [
    ["armless_user_plain_std", { argv: ["/bin/tty"], plain_std: true }],
    ["armless_user_hostname", { argv: ["/bin/uname"], hostname: "pod-7a" }],
  ]) {
    await with_exports((e) => delete e[missing], async () => {
      const vm = machine({ user });
      await assert.rejects(vm.ready, { message: "armless: this build has no exec options" });
      vm.destroy();
    });
  }
  // Without the options, such a build runs programs
  await with_exports((e) => { delete e.armless_user_plain_std; delete e.armless_user_hostname; }, async () => {
    assert.equal((await exec({ argv: ["/bin/tty"] })).code, 0);
  });
});

// ------------------------------------------------------------------ reap_orphans

test("reap_orphans: an orphan that ended is gone, not left a zombie of the first process", async () => {
  assert.equal((await exec({ argv: ["/bin/orphans"], reap_orphans: true })).code, -ECHILD);
  // Without it, the orphan (pid 1002) waits as a zombie of the first process
  assert.equal((await exec({ argv: ["/bin/orphans"] })).code, 1002);
});

test("reap_orphans on a build that cannot is an error", async () => {
  await with_exports((e) => delete e.armless_user_reap_orphans, async () => {
    const vm = machine({ user: { argv: ["/bin/orphans"], reap_orphans: true } });
    await assert.rejects(vm.ready, { message: "armless: this build cannot reap orphans" });
    vm.destroy();
    // Not asked for: it runs
    assert.equal((await exec({ argv: ["/bin/orphans"] })).code, 1002);
  });
});

// ------------------------------------------------------------------ user_asleep_ms

test("user_asleep_ms on boot: the restored process's clocks go on by it, its sleep is over", async () => {
  const { state, process } = await saved_while_sleeping();
  const vm = machine({ initial_state: state, user_state: process, user_asleep_ms: 60_000 });
  try {
    await vm.ready;
    const t = performance.now();
    const { stop, out } = await run_to_stop(vm);
    assert.deepEqual(stop.reason, "exit");
    const nap = nap_of(out);
    assert.ok(nap >= 60_000 && nap < 60_000 + NAP_MS + 5000, `slept ${nap} ms`);
    assert.ok(performance.now() - t < NAP_MS, "its sleep was due at once");
  } finally {
    vm.destroy();
  }
});

test("user_asleep_ms on restore_state, once: a restore without it resumes where it stopped", async () => {
  const { state, process } = await saved_while_sleeping();
  // Through restore_state
  let vm = machine({ initial_state: state, user_state: process });
  try {
    await vm.ready;
    await vm.restore_state(state, { user_state: process, user_asleep_ms: 3_600_000 });
    const nap = nap_of((await run_to_stop(vm)).out);
    assert.ok(nap >= 3_600_000 && nap < 3_600_000 + NAP_MS + 5000, `slept ${nap} ms`);
  } finally {
    vm.destroy();
  }
  // The boot's user_asleep_ms was for its own restore: the next one, without, is 0 and the sleep
  // runs its course
  vm = machine({ initial_state: state, user_state: process, user_asleep_ms: 60_000 });
  try {
    await vm.ready;
    await vm.restore_state(state, { user_state: process });
    const nap = nap_of((await run_to_stop(vm)).out);
    assert.ok(nap >= NAP_MS && nap < 60_000, `slept ${nap} ms`);
  } finally {
    vm.destroy();
  }
});

test("armless_user_asleep_ms is called before each restore of a process", async () => {
  const { state, process } = await saved_while_sleeping();
  const calls = [];
  await with_exports((e) => {
    const asleep = e.armless_user_asleep_ms, restore = e.armless_user_restore;
    e.armless_user_asleep_ms = (ms) => { calls.push(["asleep", ms]); return asleep(ms); };
    e.armless_user_restore = (p, n) => { calls.push(["restore"]); return restore(p, n); };
  }, async () => {
    const vm = machine({ initial_state: state, user_state: process, user_asleep_ms: 1500 });
    await vm.ready;
    await vm.restore_state(state, { user_state: process });
    await vm.restore_state(state, { user_state: process, user_asleep_ms: 42 });
    vm.destroy();
  });
  assert.deepEqual(calls, [["asleep", 1500], ["restore"], ["asleep", 0], ["restore"], ["asleep", 42], ["restore"]]);
});

// ------------------------------------------------------------------ spawn

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A machine with a program that stays, running, to spawn beside */
async function host_machine() {
  const vm = machine({ user: { argv: ["/bin/idle"], plain_std: true } });
  await vm.ready;
  vm.on("stop", (s) => assert.fail("the main program stopped: " + JSON.stringify(s)));
  vm.run();
  return vm;
}

/** Polls `job` until it has ended: {out, err, exit} (the bytes it printed, as text) */
async function finish(job, ms = 8000) {
  let out = "", err = "";
  const begun = performance.now();
  for (;;) {
    const r = job.poll();
    out += decode(r.stdout);
    err += decode(r.stderr);
    if (r.exit !== null) return { out, err, exit: r.exit };
    assert.ok(performance.now() - begun < ms, `still running: ${JSON.stringify({ out, err })}`);
    await sleep(5);
  }
}

test("spawn: stdout, stderr and the exit status come back", async () => {
  const vm = await host_machine();
  try {
    const job = await vm.spawn({ argv: ["/bin/printer"] });
    assert.equal(typeof job.pid, "number");
    assert.deepEqual(await finish(job), { out: "out\n", err: "err\n", exit: 3 });
    job.release();
  } finally {
    vm.destroy();
  }
});

test("spawn: input written and then closed reaches the process, which sees the end of file", async () => {
  const vm = await host_machine();
  try {
    const job = await vm.spawn({ argv: ["stdin"], env: ["PATH=/bin"], cwd: "/" });
    assert.equal(job.write("pi"), true);
    assert.equal(job.write(new TextEncoder().encode("ng")), true);
    job.closeInput();
    assert.equal(job.write("late"), false, "its stdin is gone");
    assert.deepEqual(await finish(job), { out: "ping" + "EOF\n", err: "", exit: 0 });
    job.release();
  } finally {
    vm.destroy();
  }
});

test("spawn: kill ends the process and what it started, as 128 + the signal", async () => {
  const vm = await host_machine();
  try {
    const job = await vm.spawn({ argv: ["/bin/sleeper"] });
    await sleep(60);
    assert.equal(job.poll().exit, null, "still running");
    job.kill();
    const { out, exit } = await finish(job);
    assert.equal(exit, 128 + 15);
    // Its child would have written by now, if the kill had left it
    await sleep(700);
    assert.equal(out + decode(job.poll().stdout), "");
    job.release();
  } finally {
    vm.destroy();
  }
});

test("spawn: a released job is gone", async () => {
  const vm = await host_machine();
  try {
    const job = await vm.spawn({ argv: ["/bin/printer"] });
    await finish(job);
    job.release();
    assert.throws(() => job.poll(), /released/);
    assert.equal(job.write("x"), false);
    job.release();
  } finally {
    vm.destroy();
  }
});

test("spawn: a program that isn't there, or no program, is refused", async () => {
  const vm = await host_machine();
  try {
    await assert.rejects(vm.spawn({ argv: ["/bin/nothing"] }), /spawn: .*not found/);
    await assert.rejects(vm.spawn({ argv: [] }), /needs a program/);
    await assert.rejects(vm.spawn({ argv: ["/bin/printer", "a\0b"] }), /NUL/);
  } finally {
    vm.destroy();
  }
});

test("spawn: needs user mode, and a build that has it", async () => {
  const vm = new Armless({ wasm, memory_mb: 64, autostart: false, filesystem: filesystem(programs) });
  await vm.ready.catch(() => {});
  await assert.rejects(vm.spawn({ argv: ["/bin/printer"] }), /needs user mode/);
  vm.destroy();
  await with_exports((e) => { delete e.armless_user_spawn; }, async () => {
    const vm = await host_machine();
    await assert.rejects(vm.spawn({ argv: ["/bin/printer"] }), /this build has no spawn/);
    vm.destroy();
  });
});

test("spawn: the program beside it goes on while a job spins", async () => {
  const vm = machine({ user: { argv: ["/bin/clock"], plain_std: true } });
  try {
    await vm.ready;
    const job = await vm.spawn({ argv: ["/bin/spin"] });
    const { stop, out } = await run_to_stop(vm);
    assert.equal(stop.reason, "exit");
    const nap = nap_of(out);
    assert.ok(nap >= NAP_MS && nap < 3 * NAP_MS, `slept ${nap} ms`);
    assert.equal(job.poll().exit, null, "the spinner ran on");
  } finally {
    vm.destroy();
  }
});

test("spawn: a held job, waiting or ended, keeps a snapshot back until it is released", async () => {
  const vm = await host_machine();
  try {
    const job = await vm.spawn({ argv: ["/bin/stdin"] });
    await sleep(50);
    assert.equal(job.poll().exit, null, "waiting for input");
    await vm.stop();
    await assert.rejects(vm.save_process(), /not released/);
    job.closeInput();
    vm.run();
    await finish(job);
    // Ended, but held until released
    await vm.stop();
    await assert.rejects(vm.save_process(), /not released/);
    job.release();
    const state = await vm.save_process();
    assert.ok(state.length > 0);
  } finally {
    vm.destroy();
  }
});

test("spawn: the machine says once when a job has output or has ended, until it is polled", async () => {
  const vm = await host_machine();
  try {
    const events = [];
    vm.on("spawn-output", (e) => events.push(e.pid));
    const job = await vm.spawn({ argv: ["/bin/printer"] });
    const begun = performance.now();
    while (!events.length) {
      assert.ok(performance.now() - begun < 5000, "no event");
      await sleep(5);
    }
    assert.deepEqual(events, [job.pid]);
    // Said once, whatever the slices that follow: nothing is polled
    await sleep(100);
    assert.deepEqual(events, [job.pid]);
    const r = job.poll();
    assert.ok(r.stdout.length > 0 || r.exit !== null);
    // Polled and taken: quiet, even when its end comes after (the next poll says it, or an event does)
    const seen = events.length;
    const done = await finish(job);
    assert.equal(done.exit, 3);
    assert.ok(events.length >= seen);
    await sleep(100);
    const quiet = events.length;
    await sleep(100);
    assert.equal(events.length, quiet, "nothing more once it is all taken");
    job.release();
  } finally {
    vm.destroy();
  }
});

test("spawn: a job at work isn't the program going quiet, one waiting for its stdin is", async () => {
  // The program waits on the terminal for input: idle, and waiting for input
  const vm = machine({ user: { argv: ["/bin/stdin"] } });
  try {
    await vm.ready;
    vm.run();
    const until = async (fn, what) => {
      const begun = performance.now();
      while (!fn()) {
        assert.ok(performance.now() - begun < 5000, what);
        await sleep(5);
      }
    };
    await until(() => vm.user_status.waiting_for_input, "the program never waited for input");
    // A job that sleeps is work: not quiet
    const sleeper = await vm.spawn({ argv: ["/bin/sleeper"] });
    await until(() => !vm.user_status.waiting_for_input, "a sleeping job counted as quiet");
    sleeper.kill();
    await finish(sleeper);
    sleeper.release();
    await until(() => vm.user_status.waiting_for_input, "quiet again once it is gone");
    // One that only waits for the host's input to its stdin is not
    const reader = await vm.spawn({ argv: ["/bin/stdin"] });
    await sleep(100);
    assert.equal(vm.user_status.waiting_for_input, true);
    reader.closeInput();
    await finish(reader);
    reader.release();
  } finally {
    vm.destroy();
  }
});
