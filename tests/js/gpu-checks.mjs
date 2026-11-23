// armless-gpu.js's protocol checks, against whatever WebGPU they are given: tests/js/gpu.html runs
// them in Chromium (navigator.gpu), tests/js/gpu.test.mjs in Node on Dawn (the webgpu package).
//
//   await runChecks({ gpu, ArmlessGPU, check })   check(name, ok, detail)

export async function runChecks({ gpu, ArmlessGPU, check }) {
const { Frames, Enc, Dec, Session, Op, budget, MAX_FRAME } = ArmlessGPU._internal;

/** A stand-in for a guest connection: frames in, replies out. */
function fake(opts = {}) {
  const replies = [];
  let wake = null;
  const conn = { write: (b) => { replies.push(b); wake?.(); }, abort() {}, close() {} };
  const adapter = gpu.requestAdapter();
  const s = new Session(conn, adapter, { log: console.warn, ...opts });
  const send = (e) => s.data(e.frame());
  const reply = async () => {
    while (!replies.length) await new Promise((r) => (wake = r));
    const f = new Frames();
    f.push(replies.shift());
    const d = new Dec(f.next());
    return { ok: d.u8() === 0, d };
  };
  return { send, reply, session: s };
}
const op = (o) => new Enc().u8(o);
const f32s = (a) => new Uint8Array(new Float32Array(a).buffer);

async function protocol() {
  {
    const g = fake();
    g.send(op(Op.Hello).u32(2));
    const r = await g.reply();
    const version = r.d.u32(), name = r.d.str(), backend = r.d.str(), maxbuf = r.d.u64();
    r.d.u64(); r.d.u32();
    const features = r.d.u32();
    r.d.u32(); r.d.u32(); r.d.str(); r.d.str(); r.d.str(); r.d.str();
    const limits = {};
    for (let k = r.d.u32(); k > 0; k--) limits[r.d.str()] = r.d.u64();
    check("hello says what the adapter is", r.ok && version === 2 && backend === "WebGPU" && maxbuf >= 1 << 20, `${name}, max buffer ${maxbuf}`);
    check("hello reports features and limits by name", limits.maxBufferSize === maxbuf && limits.minStorageBufferOffsetAlignment > 0,
      `features ${features}, ${Object.keys(limits).length} limits`);
  }
  {
    const g = fake(), n = 256;
    const ST = 0x80 | 0x8 | 0x4;
    g.send(op(Op.CreateBuffer).u32(1).u64(n * 4).u32(ST));
    g.send(op(Op.CreateBuffer).u32(2).u64(n * 4).u32(ST));
    g.send(op(Op.WriteBuffer).u32(1).u64(0).bytes(f32s(new Array(n).fill(1))));
    g.send(op(Op.CreateShader).u32(3).str(`@group(0) @binding(0) var<storage, read_write> v: array<f32>;
      @compute @workgroup_size(64) fn double(@builtin(global_invocation_id) id: vec3<u32>) { v[id.x] = v[id.x] * 2.0; }`));
    g.send(op(Op.CreatePipeline).u32(4).u32(3).str("double"));
    g.send(op(Op.CreateBindGroup).u32(5).u32(4).u32(0).u32(1).u32(0).u32(1).u64(0).u64(0));
    g.send(op(Op.Dispatch).u32(4).u32(1).u32(0).u32(5).u32(4).u32(1).u32(1));
    g.send(op(Op.CopyBuffer).u32(1).u64(0).u32(2).u64(0).u64(n * 4));
    g.send(op(Op.WriteBuffer).u32(1).u64(0).bytes(f32s(new Array(n).fill(9))));
    g.send(op(Op.ReadBuffer).u32(2).u64(0).u64(n * 4));
    const r = await g.reply();
    const out = new Float32Array(r.d.rest().slice().buffer);
    g.send(op(Op.ReadBuffer).u32(1).u64(0).u64(8));
    const r2 = await g.reply();
    const src = new Float32Array(r2.d.rest().slice().buffer);
    check("a dispatch then a copy then a write keep their order", r.ok && out.every((v) => v === 2) && src[0] === 9, `${out[0]} ${src[0]}`);
  }
  {
    const g = fake();
    g.send(op(Op.CreateBuffer).u32(1).u64(16).u32(0x80 | 0x4));
    g.send(op(Op.CreateShader).u32(2).str(`override scale: f32 = 1.0;
      override wg_size: u32 = 1;
      @group(0) @binding(0) var<storage, read_write> o: array<f32>;
      @compute @workgroup_size(wg_size) fn main(@builtin(local_invocation_index) i: u32) { o[i] = scale * f32(i); }`));
    const c = op(Op.CreatePipeline).u32(3).u32(2).str("main").u32(2).str("scale");
    const f = new Uint8Array(8); new DataView(f.buffer).setFloat64(0, 2.5, true); c.bytes(f);
    const f2 = new Uint8Array(8); new DataView(f2.buffer).setFloat64(0, 4, true); c.str("wg_size").bytes(f2);
    g.send(c);
    g.send(op(Op.CreateBindGroup).u32(4).u32(3).u32(0).u32(1).u32(0).u32(1).u64(0).u64(0));
    g.send(op(Op.Dispatch).u32(3).u32(1).u32(0).u32(4).u32(1).u32(1).u32(1));
    g.send(op(Op.ReadBuffer).u32(1).u64(0).u64(16));
    const r = await g.reply();
    const v = r.ok ? [...new Float32Array(r.d.rest().slice().buffer)] : [new TextDecoder().decode(r.d.rest())];
    check("pipeline constants reach the shader", v.join() === "0,2.5,5,7.5", v.join());
  }
  {
    const g = fake();
    g.send(op(Op.CreateBuffer).u32(1).u64(8).u32(0x80 | 0x8 | 0x4));
    g.send(op(Op.CreateBuffer).u32(2).u64(8).u32(0x1 | 0x8)); // MapRead | CopyDst
    g.send(op(Op.WriteBuffer).u32(1).u64(0).bytes(Uint8Array.of(1, 2, 3, 4, 5, 6, 7, 8)));
    g.send(op(Op.CopyBuffer).u32(1).u64(0).u32(2).u64(0).u64(8));
    g.send(op(Op.ReadBuffer).u32(2).u64(0).u64(8));
    const r = await g.reply();
    const got = r.ok ? [...r.d.rest()].join() : new TextDecoder().decode(r.d.rest());
    check("a buffer the guest maps for reading is read through the host", got === "1,2,3,4,5,6,7,8", got);
  }
  {
    const g = fake({ max_memory: 1 << 20 });
    g.send(op(Op.Hello).u32(2));
    const h = await g.reply();
    h.d.u32(); h.d.str(); h.d.str();
    const maxbuf = h.d.u64();
    g.send(op(Op.CreateBuffer).u32(1).u64(768 << 10).u32(0x80));
    g.send(op(Op.CreateBuffer).u32(2).u64(512 << 10).u32(0x80));
    g.send(op(Op.Finish));
    const r = await g.reply();
    const msg = new TextDecoder().decode(r.d.rest());
    g.send(op(Op.Destroy).u32(1));
    g.send(op(Op.CreateBuffer).u32(3).u64(512 << 10).u32(0x80));
    g.send(op(Op.Finish));
    const r2 = await g.reply();
    check("a guest gets no more GPU memory than its budget, and destroying gives it back",
      maxbuf <= 1 << 20 && !r.ok && msg.includes("out of GPU memory") && r2.ok, `${maxbuf}; ${msg}`);
  }
  {
    // Two connections of one guest share its budget, and one that closes gives back what it held
    const shared = budget({ max_memory: 1 << 20, max_objects: 4 });
    const a = fake({ budget: shared }), b = fake({ budget: shared });
    a.send(op(Op.CreateBuffer).u32(1).u64(768 << 10).u32(0x80));
    a.send(op(Op.Finish));
    const ra = await a.reply();
    b.send(op(Op.CreateBuffer).u32(1).u64(512 << 10).u32(0x80));
    b.send(op(Op.Finish));
    const rb = await b.reply();
    const mem = new TextDecoder().decode(rb.d.rest());
    for (let id = 2; id < 6; id++) b.send(op(Op.CreateBuffer).u32(id).u64(16).u32(0x80));
    b.send(op(Op.Finish));
    const rb2 = await b.reply();
    const objs = new TextDecoder().decode(rb2.d.rest());
    a.session.close();
    b.send(op(Op.CreateBuffer).u32(9).u64(768 << 10).u32(0x80));
    b.send(op(Op.Finish));
    const rb3 = await b.reply();
    check("a guest gets no more GPU memory or objects for opening more connections",
      ra.ok && !rb.ok && mem.includes("out of GPU memory") && !rb2.ok && objs.includes("too many objects") && rb3.ok,
      `${mem}; ${objs}; after the first closed: ${rb3.ok ? "ok" : new TextDecoder().decode(rb3.d.rest())}`);
  }
  {
    const g = fake({ max_memory: 1 << 20 });
    g.send(op(Op.CreateBuffer).u32(1).u64(768 << 10).u32(0x80 | 0x4));
    g.send(op(Op.ReadBuffer).u32(1).u64(0).u64(768 << 10));
    const r = await g.reply();
    const msg = new TextDecoder().decode(r.d.rest());
    g.send(op(Op.ReadBuffer).u32(1).u64(0).u64(256 << 10));
    const r2 = await g.reply();
    g.send(op(Op.ReadBuffer).u32(1).u64(0).u64(256 << 10));
    const r3 = await g.reply();
    g.send(op(Op.ReadBuffer).u32(1).u64(0).u64(MAX_FRAME));
    const r4 = await g.reply();
    const msg4 = new TextDecoder().decode(r4.d.rest());
    check("a read counts against the budget, is given back, and fits in a frame",
      !r.ok && msg.includes("out of GPU memory") && r2.ok && r3.ok && g.session.budget.memory === 768 << 10 &&
        !r4.ok && msg4.includes("smaller than a frame"),
      `${msg}; ${msg4}; ${g.session.budget.memory} bytes held`);
  }
  {
    // Writes queued behind a read that waits for the GPU: what is taken from the guest stays behind
    // what it sent until the read is answered (the guest waits), then catches up exactly
    let consumed = 0;
    const g = fake({ consume: (n) => { consumed += n; } });
    let sent = 0;
    const send = (e) => { const f = e.frame(); sent += f.length; g.session.data(f); };
    send(op(Op.CreateBuffer).u32(1).u64(4 << 20).u32(0x80 | 0x8 | 0x4));
    send(op(Op.ReadBuffer).u32(1).u64(0).u64(16));
    const chunk = new Uint8Array(4 << 20);
    for (let k = 0; k < 10; k++) send(op(Op.WriteBuffer).u32(1).u64(0).bytes(chunk));
    const held = consumed;
    await g.reply();
    await g.session.idle;
    check("the guest is held back while 32 MiB wait to run, and every byte is taken in the end",
      held < sent && held <= (32 << 20) + chunk.length && consumed === sent, `took ${held >> 20} MiB of ${sent >> 20} while the read waited, then ${consumed >> 20}`);
  }
  {
    const g = fake();
    g.send(op(Op.CreateShader).u32(1).str("this is not wgsl"));
    g.send(op(Op.Finish));
    const r = await g.reply();
    const msg = new TextDecoder().decode(r.d.rest());
    g.send(op(Op.Finish));
    const r2 = await g.reply();
    check("a bad shader is reported at the next reply and the connection goes on", !r.ok && msg && r2.ok, msg.slice(0, 80));
  }
  {
    const g = fake();
    g.send(op(Op.WriteBuffer).u32(999).u64(0).bytes(new Uint8Array(4)));
    g.send(op(Op.Finish));
    const r = await g.reply();
    const msg = new TextDecoder().decode(r.d.rest());
    g.send(op(Op.ReadBuffer).u32(999).u64(0).u64(4));
    const r2 = await g.reply();
    const msg2 = new TextDecoder().decode(r2.d.rest());
    check("an unknown buffer is an error, not a crash", !r.ok && msg.includes("no such buffer") && !r2.ok && msg2.includes("no such buffer"), msg);
  }
}

// Each guest connection gets a device of its own (an adapter makes one): a second connection
// after a first must work too, as a program that checks the GPU and then uses it makes two
async function two_connections() {
  const vm = new (class extends EventTarget { on(n, f) { this.addEventListener(n, (e) => f(e.detail)); } off() {} })();
  ArmlessGPU.attach(vm, { gpu, log: console.warn });
  const hello = () => new Promise((resolve) => {
    const replies = [];
    const conn = {
      host: "gpu.internal", port: 7070, accept() {}, refuse() { resolve("refused"); }, abort() {}, close() {},
      _h: {}, on(n, f) { this._h[n] = f; },
      write(b) { const f = new Frames(); f.push(b); const d = new Dec(f.next()); resolve(d.u8() === 0 ? "ok" : new TextDecoder().decode(d.rest())); },
    };
    vm.dispatchEvent(new CustomEvent("tcp-connection", { detail: conn }));
    conn._h.data(op(Op.Hello).u32(2).frame());
  });
  const first = await hello(), second = await hello();
  check("a second connection gets a device of its own", first === "ok" && second === "ok", `${first}, ${second}`);
}

// A guest may have so many connections open; one more is refused, and a closed one's place comes back
async function connection_limit() {
  const vm = new (class extends EventTarget { on(n, f) { this.addEventListener(n, (e) => f(e.detail)); } off() {} })();
  ArmlessGPU.attach(vm, { gpu, log: () => {}, max_connections: 1 });
  const open = () => {
    const conn = { host: "gpu.internal", port: 7070, state: "new", _h: {}, on(n, f) { this._h[n] = f; },
      accept() { this.state = "open"; }, refuse() { this.state = "refused"; }, abort() {}, close() {}, write() {} };
    vm.dispatchEvent(new CustomEvent("tcp-connection", { detail: conn }));
    return conn;
  };
  const a = open(), b = open();
  a._h.close();
  const c = open();
  check("a guest may have so many GPU connections open", a.state === "open" && b.state === "refused" && c.state === "open",
    `${a.state}, ${b.state}, then ${c.state}`);
}

await protocol();
await two_connections();
await connection_limit();
}
