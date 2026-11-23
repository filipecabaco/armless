// armless-gpu.js — WebGPU compute for armless guests, on the page's GPU.
//
//   const vm = new Armless({ ..., network: true });
//   ArmlessGPU.attach(vm);       // { gpu: navigator.gpu, host: "gpu.internal", port: 7070 }
//
// A guest program connects to gpu.internal:7070 and speaks armless-gpu's protocol (gpu/src/wire.rs,
// docs/gpu.md): buffers, WGSL shaders, compute pipelines, bind groups, dispatches, copies and
// reads. Each connection gets its own GPUDevice and the commands run on it as they come, one at a
// time. The native host (gpu/src/server.rs, on wgpu) does the same; the two answer alike.
//
// Errors: a command that fails without a reply of its own (a shader that doesn't compile, an
// unknown buffer) is reported at the next reply. WebGPU says so asynchronously, so each command
// runs inside an error scope and a reply waits for the scopes before it.
(function (root) {
  "use strict";

  const VERSION = 2;
  const FEATURE_F16 = 1, FEATURE_SUBGROUPS = 2, FEATURE_PACKED_DOT = 4;
  // The limits Hello reports, by their WebGPU names
  const LIMITS = ["maxTextureDimension1D", "maxTextureDimension2D", "maxTextureDimension3D", "maxTextureArrayLayers",
    "maxBindGroups", "maxBindingsPerBindGroup", "maxDynamicUniformBuffersPerPipelineLayout",
    "maxDynamicStorageBuffersPerPipelineLayout", "maxSampledTexturesPerShaderStage", "maxSamplersPerShaderStage",
    "maxStorageBuffersPerShaderStage", "maxStorageTexturesPerShaderStage", "maxUniformBuffersPerShaderStage",
    "maxUniformBufferBindingSize", "maxStorageBufferBindingSize", "minUniformBufferOffsetAlignment",
    "minStorageBufferOffsetAlignment", "maxVertexBuffers", "maxBufferSize", "maxVertexAttributes",
    "maxVertexBufferArrayStride", "maxInterStageShaderVariables", "maxColorAttachments",
    "maxColorAttachmentBytesPerSample", "maxComputeWorkgroupStorageSize", "maxComputeInvocationsPerWorkgroup",
    "maxComputeWorkgroupSizeX", "maxComputeWorkgroupSizeY", "maxComputeWorkgroupSizeZ",
    "maxComputeWorkgroupsPerDimension"];
  const MAX_FRAME = 256 << 20;
  const OK = 0, ERR = 1;
  const Op = {
    Hello: 1, CreateBuffer: 2, WriteBuffer: 3, CreateShader: 4, CreatePipeline: 5,
    CreateBindGroup: 6, Dispatch: 7, CopyBuffer: 8, ReadBuffer: 9, Destroy: 10, Finish: 11,
  };
  const OP_NAMES = Object.fromEntries(Object.entries(Op).map(([k, v]) => [v, k]));
  // GPUBufferUsage, in case the page's realm has none (the bits are the protocol's too)
  const MAP_READ = 0x1, MAP_WRITE = 0x2, COPY_SRC = 0x4, COPY_DST = 0x8;
  // A connection's limits unless attach says otherwise, and how much may wait to run before the
  // guest is made to wait
  const DEFAULT_MAX_MEMORY = 2 * 2 ** 30;
  const DEFAULT_MAX_OBJECTS = 1 << 20;
  const DEFAULT_MAX_CONNECTIONS = 16;
  const HIGH_WATER = 32 << 20;

  /**
   * What a guest may take of the GPU and holds now, for all of its connections together: a guest
   * that opens more of them gets no more.
   */
  function budget(options = {}) {
    return {
      max_memory: options.max_memory ?? DEFAULT_MAX_MEMORY,
      max_objects: options.max_objects ?? DEFAULT_MAX_OBJECTS,
      max_connections: options.max_connections ?? DEFAULT_MAX_CONNECTIONS,
      memory: 0,
      objects: 0,
      connections: 0,
    };
  }

  const mib = (n) => (n / 2 ** 20).toFixed(1);

  /** A guest's buffer usage as the host makes it: map usages become the copies the host uses. */
  function host_usage(u) {
    if (u & MAP_READ) u = (u & ~MAP_READ) | COPY_SRC;
    if (u & MAP_WRITE) u = (u & ~MAP_WRITE) | COPY_DST;
    return u;
  }

  const utf8d = new TextDecoder();
  const utf8e = new TextEncoder();

  /** Bytes in, frames out: a frame is a u32 length and that many bytes. */
  class Frames {
    constructor() {
      this.chunks = [];
      this.size = 0;
    }
    push(bytes) {
      this.chunks.push(bytes);
      this.size += bytes.length;
    }
    /** The first `n` bytes, taken out. */
    take(n) {
      const out = new Uint8Array(n);
      let at = 0;
      while (at < n) {
        const c = this.chunks[0];
        const k = Math.min(c.length, n - at);
        out.set(c.subarray(0, k), at);
        at += k;
        if (k === c.length) this.chunks.shift();
        else this.chunks[0] = c.subarray(k);
      }
      this.size -= n;
      return out;
    }
    peek_u32() {
      const b = this.size >= 4 ? this.take(4) : null;
      if (!b) return null;
      this.chunks.unshift(b);
      this.size += 4;
      return new DataView(b.buffer).getUint32(0, true);
    }
    /** The next whole frame, or null. Throws on one over MAX_FRAME. */
    next() {
      const n = this.peek_u32();
      if (n === null) return null;
      if (n > MAX_FRAME) throw new Error("frame too large");
      if (this.size < 4 + n) return null;
      this.take(4);
      return this.take(n);
    }
  }

  /** Reads a frame's body. */
  class Dec {
    constructor(b) {
      this.b = b;
      this.dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
      this.at = 0;
    }
    need(n) {
      if (this.at + n > this.b.length) throw new Error("message too short");
    }
    u8() { this.need(1); return this.b[this.at++]; }
    u32() { this.need(4); const v = this.dv.getUint32(this.at, true); this.at += 4; return v; }
    f64() { this.need(8); const v = this.dv.getFloat64(this.at, true); this.at += 8; return v; }
    empty() { return this.at >= this.b.length; }
    u64() {
      this.need(8);
      const lo = this.dv.getUint32(this.at, true), hi = this.dv.getUint32(this.at + 4, true);
      this.at += 8;
      if (hi >= 0x200000) throw new Error("number too large");
      return hi * 0x100000000 + lo;
    }
    str() { const n = this.u32(); this.need(n); const s = utf8d.decode(this.b.subarray(this.at, this.at + n)); this.at += n; return s; }
    rest() { const r = this.b.subarray(this.at); this.at = this.b.length; return r; }
  }

  /** Builds a frame's body. */
  class Enc {
    constructor() { this.parts = []; this.size = 0; }
    bytes(b) { this.parts.push(b); this.size += b.length; return this; }
    u8(v) { return this.bytes(Uint8Array.of(v)); }
    u32(v) { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, v >>> 0, true); return this.bytes(b); }
    u64(v) {
      const b = new Uint8Array(8), dv = new DataView(b.buffer);
      dv.setUint32(0, v % 0x100000000, true);
      dv.setUint32(4, Math.floor(v / 0x100000000), true);
      return this.bytes(b);
    }
    str(s) { const b = utf8e.encode(s); return this.u32(b.length).bytes(b); }
    /** The whole frame, length first. */
    frame() {
      const out = new Uint8Array(4 + this.size);
      new DataView(out.buffer).setUint32(0, this.size, true);
      let at = 4;
      for (const p of this.parts) { out.set(p, at); at += p.length; }
      return out;
    }
  }

  /** One guest connection and its device. */
  class Session {
    constructor(conn, adapter, gpu_options) {
      this.conn = conn;
      this.adapter = adapter;
      this.options = gpu_options;
      this.device = null;
      this.objects = new Map();
      this.encoder = null;
      this.error = null; // the first error since the last reply
      this.scope = false; // an error scope is open (from the first command after the last reply)
      this.frames = new Frames();
      this.queue = []; // whole frames not run yet
      this.queued = 0; // their bytes
      this.running = false;
      this.idle = Promise.resolve(); // settles when the queue has run
      this.closed = false;
      // Limits: the guest's (attach shares one among its connections), or this connection's own
      this.budget = gpu_options.budget || budget(gpu_options);
      // Flow control: the guest's bytes are said taken (`consume`) only while what waits to run is
      // under HIGH_WATER, so a guest sending faster than the GPU works waits in its own socket.
      // Bytes of a frame still coming are taken regardless once the queue drains, so even the
      // largest frame gets through.
      this.consume = gpu_options.consume || null;
      this.unconsumed = 0;
    }

    /** Says the guest's bytes taken while the queue is short enough. */
    flow() {
      if (this.consume && this.unconsumed && this.queued < HIGH_WATER) {
        const n = this.unconsumed;
        this.unconsumed = 0;
        this.consume(n);
      }
    }

    /** Takes `n` bytes of the guest's memory, or throws. */
    take_memory(n) {
      const b = this.budget;
      if (b.memory + n > b.max_memory) {
        throw new Error(`out of GPU memory: ${mib(n)} MiB more, with ${mib(b.memory)} of this guest's ${mib(b.max_memory)} MiB in use`);
      }
      b.memory += n;
    }

    /**
     * Keeps an object under the guest's object limit. A buffer's bytes were taken when it was
     * made, and are given back when it goes.
     */
    keep(id, o) {
      const b = this.budget;
      if (!this.objects.has(id)) {
        if (b.objects >= b.max_objects) throw new Error(`too many objects (this guest may have ${b.max_objects})`);
        b.objects++;
      }
      const old = this.objects.get(id);
      this.objects.set(id, o);
      if (old) this.forget(old);
    }

    /** Drops the object `id`, giving back what it held. */
    remove(id) {
      const o = this.objects.get(id);
      if (!o) return;
      this.objects.delete(id);
      this.budget.objects--;
      this.forget(o);
    }

    forget(o) {
      if (o.kind === "buffer") {
        this.budget.memory -= o.v.size;
        o.v.destroy();
      }
    }

    fail(msg) {
      if (this.error === null) this.error = String(msg);
    }

    /** Bytes from the guest: whole frames run in order, one after another. */
    data(bytes) {
      this.unconsumed += bytes.length;
      this.frames.push(bytes);
      let f;
      try {
        while ((f = this.frames.next())) {
          this.queue.push(f);
          this.queued += 4 + f.length;
        }
      } catch (e) {
        return this.close(e);
      }
      this.flow();
      if (!this.running) this.idle = this.pump();
    }

    /**
     * Runs the queue. Commands without a reply run at once, in this turn; only opening the device
     * and the commands with a reply wait (for the GPU), and the queue goes on after them.
     */
    async pump() {
      this.running = true;
      try {
        while (this.queue.length && !this.closed) {
          const frame = this.queue.shift();
          this.queued -= 4 + frame.length;
          const r = this.handle(frame);
          if (r) await r;
          this.flow();
        }
      } finally {
        this.running = false;
      }
    }

    /**
     * Ends the connection (aborted, or closed in order once the guest has ended its side) and
     * gives the guest back what it held.
     */
    close(why, orderly = false) {
      if (this.closed) return;
      this.closed = true;
      if (why) this.options.log?.(`armless-gpu: closing a guest connection: ${why}`);
      for (const id of [...this.objects.keys()]) this.remove(id);
      this.device?.destroy();
      if (orderly) this.conn.close?.();
      else this.conn.abort?.();
      this.options.on_close?.();
    }

    async open() {
      const a = await this.adapter;
      if (!a) throw new Error("no WebGPU adapter");
      const want = LIMITS;
      const requiredLimits = {};
      for (const k of want) if (a.limits[k] !== undefined) requiredLimits[k] = a.limits[k];
      const requiredFeatures = ["shader-f16", "subgroups"].filter((f) => a.features.has(f));
      this.device = await a.requestDevice({ requiredLimits, requiredFeatures, label: "armless guest" });
      this.device.addEventListener("uncapturederror", (e) => this.fail(e.error?.message || e.error));
      this.device.lost.then((info) => { if (!this.closed) this.fail(`device lost: ${info.message}`); });
      this.info = a.info || {};
    }

    reply(payload) {
      const e = new Enc();
      if (this.error !== null) {
        e.u8(ERR).bytes(utf8e.encode(this.error));
        this.error = null;
      } else {
        e.u8(OK);
        for (const p of payload || []) e.bytes(p);
      }
      if (!this.closed) this.conn.write(e.frame());
    }

    /** Closes the error scope of the commands since the last reply: a promise of its error. */
    close_scope() {
      if (!this.scope) return Promise.resolve(null);
      this.scope = false;
      return this.device.popErrorScope().catch((e) => e);
    }

    submit() {
      if (this.encoder) {
        this.device.queue.submit([this.encoder.finish()]);
        this.encoder = null;
      }
    }

    enc() {
      return this.encoder || (this.encoder = this.device.createCommandEncoder());
    }

    get(id, kind) {
      const o = this.objects.get(id);
      if (!o || o.kind !== kind) throw new Error(`no such ${kind}`);
      return o.v;
    }

    /** Runs one command; a promise when it has to wait (the device to open, or a reply). */
    handle(frame) {
      const d = new Dec(frame);
      const op = frame.length ? d.u8() : 0;
      const name = OP_NAMES[op];
      const replies = op === Op.Hello || op === Op.ReadBuffer || op === Op.Finish;
      if (!name) return this.fail("unknown command");
      if (!this.device) {
        return this.open().then(() => this.handle(frame), (e) => {
          this.fail(`${name}: ${e.message || e}`);
          if (replies) this.reply();
        });
      }
      // One error scope for every command between two replies: a reply reports the first error
      // since the last, which is all a guest is told
      if (!this.scope) {
        this.device.pushErrorScope("validation");
        this.scope = true;
      }
      let payload;
      try {
        payload = this.run(op, d);
      } catch (e) {
        this.fail(`${name}: ${e.message || e}`);
        payload = null;
      }
      if (!replies) return;
      const scope = this.close_scope();
      return Promise.resolve(payload)
        .catch((e) => { this.fail(`${name}: ${e.message || e}`); return null; })
        .then(async (p) => {
          const err = await scope;
          if (err) this.fail(err.message || String(err));
          this.reply(p || undefined);
        });
    }

    /** Runs one command; those with a reply return its payload (a promise of an array of bytes). */
    run(op, d) {
      const dev = this.device;
      switch (op) {
        case Op.Hello: {
          d.u32();
          const l = dev.limits, i = this.info;
          const name = [i.vendor, i.architecture, i.device, i.description].filter(Boolean).join(" ") || "WebGPU adapter";
          let features = 0;
          if (dev.features.has("shader-f16")) features |= FEATURE_F16;
          if (dev.features.has("subgroups")) features |= FEATURE_SUBGROUPS;
          const wgsl = this.options.wgsl_features;
          if (wgsl && wgsl.has("packed_4x8_integer_dot_product")) features |= FEATURE_PACKED_DOT;
          // No buffer larger than the guest's budget
          const limit = (k) => (k === "maxBufferSize" ? Math.min(l[k], this.budget.max_memory) : l[k]);
          const p = new Enc().u32(VERSION).str(name).str("WebGPU").u64(limit("maxBufferSize"))
            .u64(l.maxStorageBufferBindingSize).u32(l.maxComputeWorkgroupsPerDimension)
            .u32(features).u32(i.subgroupMinSize || 0).u32(i.subgroupMaxSize || 0)
            .str(i.vendor || "").str(i.architecture || "").str(i.device || "").str(i.description || "");
          const known = LIMITS.filter((k) => l[k] !== undefined);
          p.u32(known.length);
          for (const k of known) p.str(k).u64(limit(k));
          return Promise.resolve(p.parts);
        }
        case Op.CreateBuffer: {
          const id = d.u32(), size = d.u64(), usage = d.u32();
          this.take_memory(size);
          const v = dev.createBuffer({ size, usage: host_usage(usage) });
          try {
            this.keep(id, { kind: "buffer", v });
          } catch (e) {
            this.budget.memory -= size;
            v.destroy();
            throw e;
          }
          return;
        }
        case Op.WriteBuffer: {
          const id = d.u32(), offset = d.u64(), data = d.rest();
          const b = this.get(id, "buffer");
          // queue writes run before the next submission: what was recorded goes first
          this.submit();
          dev.queue.writeBuffer(b, offset, data);
          return;
        }
        case Op.CreateShader: {
          const id = d.u32(), code = d.str();
          this.keep(id, { kind: "shader", v: dev.createShaderModule({ code }) });
          return;
        }
        case Op.CreatePipeline: {
          const id = d.u32(), shader = d.u32(), entryPoint = d.str();
          // Pipeline-overridable constants (version 2)
          const constants = {};
          if (!d.empty()) for (let k = d.u32(); k > 0; k--) constants[d.str()] = d.f64();
          const module = this.get(shader, "shader");
          const v = dev.createComputePipeline({ layout: "auto", compute: { module, entryPoint, constants } });
          this.keep(id, { kind: "pipeline", v });
          return;
        }
        case Op.CreateBindGroup: {
          const id = d.u32(), pipeline = this.get(d.u32(), "pipeline"), group = d.u32(), n = d.u32();
          const entries = [];
          for (let k = 0; k < n; k++) {
            const binding = d.u32(), buffer = this.get(d.u32(), "buffer"), offset = d.u64(), size = d.u64();
            entries.push({ binding, resource: size ? { buffer, offset, size } : { buffer, offset } });
          }
          const v = dev.createBindGroup({ layout: pipeline.getBindGroupLayout(group), entries });
          this.keep(id, { kind: "bind group", v });
          return;
        }
        case Op.Dispatch: {
          const pipeline = this.get(d.u32(), "pipeline"), n = d.u32();
          const groups = [];
          for (let k = 0; k < n; k++) groups.push([d.u32(), this.get(d.u32(), "bind group")]);
          const x = d.u32(), y = d.u32(), z = d.u32();
          const pass = this.enc().beginComputePass();
          pass.setPipeline(pipeline);
          for (const [g, bg] of groups) pass.setBindGroup(g, bg);
          pass.dispatchWorkgroups(x, y, z);
          pass.end();
          return;
        }
        case Op.CopyBuffer: {
          const src = this.get(d.u32(), "buffer"), so = d.u64(), dst = this.get(d.u32(), "buffer"), dof = d.u64(), size = d.u64();
          this.enc().copyBufferToBuffer(src, so, dst, dof, size);
          return;
        }
        case Op.ReadBuffer: {
          const src = this.get(d.u32(), "buffer"), offset = d.u64(), size = d.u64();
          if (offset % 4 || size % 4) throw new Error("reads must be 4-byte aligned");
          // The reply carries the bytes after its status byte, in one frame
          if (size >= MAX_FRAME) throw new Error("a read must be smaller than a frame (256 MiB)");
          // The staging buffer is the guest's while the read lasts
          this.take_memory(size);
          let staging = null;
          const done = () => {
            staging?.destroy();
            this.budget.memory -= size;
          };
          let mapped;
          try {
            staging = dev.createBuffer({ size, usage: MAP_READ | COPY_DST });
            this.enc().copyBufferToBuffer(src, offset, staging, 0, size);
            this.submit();
            mapped = staging.mapAsync(1 /* GPUMapMode.READ */);
          } catch (e) {
            done();
            throw e;
          }
          return mapped.then(() => [new Uint8Array(staging.getMappedRange()).slice()]).finally(done);
        }
        case Op.Destroy: {
          this.remove(d.u32());
          return;
        }
        case Op.Finish:
          this.submit();
          return dev.queue.onSubmittedWorkDone().then(() => []);
      }
    }
  }

  /**
   * Answers `vm`'s guest connections to host:port (gpu.internal:7070) with the page's GPU. The guest
   * needs a network device (`network` on; `{ allow: () => false }` lets nothing else out). The
   * guest, all its connections together, may have `max_memory` bytes of buffers (default 2 GiB),
   * `max_objects` objects (2^20) and `max_connections` connections open (16) at once; one
   * connection more is refused. Returns a function that stops answering new ones.
   */
  function attach(vm, options = {}) {
    const gpu = options.gpu || (typeof navigator !== "undefined" ? navigator.gpu : undefined);
    const host = (options.host || "gpu.internal").toLowerCase();
    const port = options.port || 7070;
    // One budget for all of this guest's connections
    const shared = budget(options);
    const o = { log: options.log || ((m) => console.warn(m)), wgsl_features: gpu && gpu.wgslLanguageFeatures, budget: shared };
    const on = (conn) => {
      if (conn.port !== port || String(conn.host).toLowerCase() !== host) return;
      if (!gpu) {
        o.log("armless-gpu: this page has no WebGPU (navigator.gpu)");
        return conn.refuse();
      }
      if (shared.connections >= shared.max_connections) {
        o.log(`armless-gpu: too many GPU connections (this guest may have ${shared.max_connections} open)`);
        return conn.refuse();
      }
      shared.connections++;
      // An adapter of its own: WebGPU lets an adapter make one device, and each connection has one
      const adapter = gpu.requestAdapter(options.adapter || { powerPreference: "high-performance" }).catch(() => null);
      conn.accept();
      // armless.js's flow control: the guest's bytes are said taken once the session can take more
      let consume = null;
      if (typeof vm._call === "function") {
        conn.consume_later = true;
        consume = (n) => {
          if (conn.state === "closed") return;
          vm._call("armless_tcp_consumed", conn.id, n);
          vm._poke?.();
        };
      }
      const s = new Session(conn, adapter, { ...o, consume, on_close: () => shared.connections-- });
      conn.on("data", (b) => s.data(b));
      // The guest ended its side: what it sent runs, then the connection closes in order
      conn.on("end", () => { s.idle.then(() => s.close(null, true)); });
      conn.on("close", () => s.close());
    };
    vm.on("tcp-connection", on);
    return () => vm.off("tcp-connection", on);
  }

  const ArmlessGPU = { attach, VERSION, _internal: { Frames, Dec, Enc, Session, Op, budget, MAX_FRAME } };
  root.ArmlessGPU = ArmlessGPU;
  if (typeof module === "object" && module.exports) module.exports = ArmlessGPU;
})(typeof globalThis !== "undefined" ? globalThis : this);
