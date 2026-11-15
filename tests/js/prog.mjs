// Programs for the user-mode tests of armless.js, written as aarch64 machine code: a few
// instructions a system call, in a static ELF of one segment. The JavaScript twin of `Prog` in
// user/tests/prog/mod.rs (same layout, same encodings), so a test needs no toolchain and no
// guest filesystem image: the ELF goes to the machine as a blob of an in-memory filesystem.

export const BASE = 0x40_0000n;
/** ELF header and one program header: the code starts right after them */
export const CODE = 64n + 56n;
/** The data starts a page in */
export const DATA = BASE + 0x1000n;

/** A number in a register */
export const I = (v) => ({ k: "I", v: BigInt(v) });
/** The address of this offset of the data */
export const D = (off) => ({ k: "D", off });
/** The 32-bit value at this offset of the data */
export const W = (off) => ({ k: "W", off });
/** The 64-bit value at this offset of the data */
export const X = (off) => ({ k: "X", off });

export class Prog {
  constructor() {
    this.code = [];
    this.data = new Uint8Array(0);
    this.checks = 0;
  }

  /** movz, and a movk for each higher halfword that isn't zero */
  li(rd, v) {
    v = BigInt.asUintN(64, BigInt(v));
    const half = (hw) => Number((v >> BigInt(16 * hw)) & 0xffffn);
    this.code.push((0xd280_0000 | (half(0) << 5) | rd) >>> 0);
    for (let hw = 1; hw < 4; hw++) {
      if (half(hw)) this.code.push((0xf280_0000 | (hw << 21) | (half(hw) << 5) | rd) >>> 0);
    }
  }

  arg(rd, a) {
    if (a.k === "I") return this.li(rd, a.v);
    if (a.k === "D") return this.li(rd, DATA + BigInt(a.off));
    this.li(9, DATA);
    if (a.k === "W") return this.code.push((0xb940_0000 | ((a.off / 4) << 10) | (9 << 5) | rd) >>> 0); // ldr wd, [x9, #off]
    this.code.push((0xf940_0000 | ((a.off / 8) << 10) | (9 << 5) | rd) >>> 0); // ldr xd, [x9, #off]
  }

  /** The system call `nr` with these arguments (none: x0 as it is); its result is in x0 */
  sys(nr, args = []) {
    args.forEach((a, i) => this.arg(i, a));
    this.li(8, nr);
    this.code.push(0xd400_0001); // svc #0
    return this;
  }

  /** Keeps x0 at this offset of the data */
  save(off) {
    this.li(9, DATA);
    this.code.push((0xf900_0000 | ((off / 8) << 10) | (9 << 5)) >>> 0); // str x0, [x9, #off]
    return this;
  }

  /** Exits with 100 + the check's number unless x0 is `v` (-4095 < v < 4096) */
  expect(v) {
    this.checks++;
    const imm = Math.abs(v);
    // cmp x0, #v, or cmn for a negative one
    this.code.push(((v < 0 ? 0xb100_001f : 0xf100_001f) | (imm << 10)) >>> 0);
    this.code.push(0x5400_0080); // b.eq +16
    this.li(0, 100 + this.checks);
    this.li(8, 94);
    this.code.push(0xd400_0001);
    return this;
  }

  /** exit_group with x0 as it is (user mode's exit code is the whole of it) */
  exit_x0() {
    return this.sys(94);
  }

  put(off, bytes) {
    if (typeof bytes === "string") bytes = new TextEncoder().encode(bytes);
    const end = off + bytes.length;
    if (this.data.length < end) {
      const grown = new Uint8Array(end);
      grown.set(this.data);
      this.data = grown;
    }
    this.data.set(bytes, off);
    return this;
  }

  /** A struct timespec of `ms` milliseconds at this offset */
  timespec(off, ms) {
    const b = new DataView(new ArrayBuffer(16));
    b.setBigUint64(0, BigInt(Math.floor(ms / 1000)), true);
    b.setBigUint64(8, BigInt((ms % 1000) * 1e6), true);
    return this.put(off, new Uint8Array(b.buffer));
  }

  /** clone(SIGCHLD), a fork: what follows is the child's, up to `parent(at)` */
  fork() {
    this.sys(220, [I(17), I(0), I(0), I(0), I(0)]);
    this.code.push(0); // cbnz x0 over the child's part, once it is known
    return this.code.length - 1;
  }

  /** The child exits (0) and the parent's part begins */
  parent(at) {
    this.sys(94, [I(0)]);
    this.code[at] = (0xb500_0000 | ((this.code.length - at) << 5)) >>> 0;
    return this;
  }

  /** The executable: exit_group(0) after what it does */
  elf() {
    this.sys(94, [I(0)]);
    const code = new Uint8Array(new Uint32Array(this.code).buffer);
    if (code.length + Number(CODE) > 0x1000) throw new Error("code past its page");
    const body = new Uint8Array(0x1000 - Number(CODE) + this.data.length);
    body.set(code);
    body.set(this.data, 0x1000 - Number(CODE));
    const filesz = CODE + BigInt(body.length);
    const out = new Uint8Array(Number(CODE) + body.length);
    const v = new DataView(out.buffer);
    out.set([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]);
    v.setUint16(16, 2, true); // ET_EXEC
    v.setUint16(18, 183, true); // EM_AARCH64
    v.setUint32(20, 1, true);
    v.setBigUint64(24, BASE + CODE, true); // entry
    v.setBigUint64(32, 64n, true); // phoff
    [64, 56, 1, 64, 0, 0].forEach((x, i) => v.setUint16(52 + 2 * i, x, true));
    v.setUint32(64, 1, true); // PT_LOAD
    v.setUint32(68, 7, true); // R W X
    [0n, BASE, BASE, filesz, filesz + 0x4000n, 0x1000n].forEach((x, i) => v.setBigUint64(72 + 8 * i, x, true));
    out.set(body, Number(CODE));
    return out;
  }
}

/**
 * A filesystem of these programs, all in memory: `json` for armless.js's `filesystem.json`
 * (v86's format 3, each program a file in /bin of its own blob) and `fetch_blob` to answer the
 * machine's requests for them.
 */
export function filesystem(programs) {
  const blobs = new Map();
  const bin = [];
  for (const [name, elf] of Object.entries(programs)) {
    blobs.set(name + ".elf", elf);
    bin.push([name, elf.length, 0, 0o100755, 0, 0, name + ".elf"]);
  }
  const fetched = [];
  return {
    json: { version: 3, fsroot: [["bin", 0, 0, 0o40755, 0, 0, bin], ["tmp", 0, 0, 0o41777, 0, 0, []]] },
    fetched,
    fetch_blob: async (name) => {
      fetched.push(name);
      if (!blobs.has(name)) throw new Error(`no blob ${name}`);
      return blobs.get(name);
    },
  };
}
