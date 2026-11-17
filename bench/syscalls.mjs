// What system calls cost user mode in the browser's engine: the cases of
// user/benches/syscalls.rs that unkernel's README also reports, run through armless.js on
// build/armless.wasm under Node. Each case is a program that makes its calls in a loop; it runs
// with n and 2n rounds, and the difference over n is the cost of one round (the machine's start
// and the program's load cancel out). Build the module first (sh build-wasm.sh), then:
//
//   node bench/syscalls.mjs [ROUNDS]
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { Prog, I, D, X, filesystem } from "../tests/js/prog.mjs";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const { Armless } = require(path.join(root, "js/armless.js"));
const wasm = path.join(root, "build/armless.wasm");
const ROUNDS = Number(process.argv[2] || 200_000);

/** `body` `n` times (x19 counts), as the Rust benchmark's `repeat` */
function repeat(p, n, body) {
  p.li(19, n);
  const top = p.code.length;
  body(p);
  p.code.push(0xf100_0673); // subs x19, x19, #1
  const off = top - p.code.length;
  p.code.push((0x5400_0001 | ((off & 0x7ffff) << 5)) >>> 0); // b.ne top
}

const cases = {
  getppid: (p, n) => repeat(p, n, (p) => p.sys(173)),
  "read 64 bytes of /dev/zero": (p, n) => {
    p.put(0x200, "/dev/zero\0");
    p.sys(56, [I(-100n & 0xffff_ffff_ffff_ffffn), D(0x200), I(0)]).save(0x10);
    repeat(p, n, (p) => p.sys(63, [X(0x10), D(0x400), I(64)]));
  },
  "pipe: 1-byte write and read": (p, n) => {
    p.sys(59, [D(0), I(0)]); // pipe2: read end at 0, write end at 4
    repeat(p, n, (p) => {
      p.sys(64, [{ k: "W", off: 4 }, D(0x400), I(1)]);
      p.sys(63, [{ k: "W", off: 0 }, D(0x400), I(1)]);
    });
  },
  "fork, exit, wait": (p, n) => repeat(p, n, (p) => {
    const at = p.fork();
    p.parent(at);
    p.sys(260, [I(-1n & 0xffff_ffff_ffff_ffffn), I(0), I(0), I(0)]);
  }),
};

/** Milliseconds from run() to the program's exit. */
async function timed(make, n) {
  const p = new Prog();
  make(p, n);
  const vm = new Armless({
    wasm, memory_mb: 64, autostart: false,
    filesystem: filesystem({ bench: p.elf() }),
    user: { argv: ["/bin/bench"], env: [], cwd: "/" },
  });
  await vm.ready;
  return new Promise((resolve, reject) => {
    let t0;
    vm.on("stop", (s) => {
      const ms = performance.now() - t0;
      vm.destroy?.();
      if (s.reason !== "exit" || s.code !== 0) reject(new Error(JSON.stringify(s)));
      else resolve(ms);
    });
    t0 = performance.now();
    vm.run();
  });
}

for (const [name, make] of Object.entries(cases)) {
  const n = name.startsWith("fork") ? Math.max(1, Math.floor(ROUNDS / 100)) : ROUNDS;
  await timed(make, n); // warm the engine's compile of the module
  const best = async (k) => Math.min(...(await sequential(3, () => timed(make, k))));
  const one = await best(n);
  const two = await best(2 * n);
  const per = ((two - one) / n) * 1e6; // ns
  console.log(`${name.padEnd(30)} ${per >= 10_000 ? (per / 1000).toFixed(1) + " µs" : per.toFixed(0) + " ns"}`);
}

async function sequential(k, f) {
  const out = [];
  for (let i = 0; i < k; i++) out.push(await f());
  return out;
}
