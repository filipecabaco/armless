// armless.js — run the armless AArch64 machine (armless.wasm) in a page or in Node.
//
//   const vm = new Armless({
//     wasm: "armless.wasm", wasm_smp: "armless-smp.wasm",   // the second for cpus > 1
//     memory_mb: 512, cpus: 1,
//     filesystem: { json: "filesystem.json", baseurl: "filesystem/" },
//     initial_state: "state.bin.zst",       // or boot: { kernel, initrd, cmdline }, or
//                                           // boot: { cmdline } (kernel from /boot in the filesystem)
//     network: true,
//   });
//   vm.on("console", bytes => term.write(bytes));
//   term.onData(text => vm.console_input(text));
//
// Everything the machine does runs in WebAssembly, devices included. This file only
// loads things, runs the CPUs (here, or one Web Worker per CPU) and answers the
// machine's requests: files of the 9p filesystem (fetched from `baseurl` the first
// time the guest reads them) and the guest's TCP connections ("tcp-connection"
// events; port 80 goes out through fetch() unless a listener takes it).
//
// Events: "ready", "console", "serial", "progress" ({file, loaded, total}),
// "blob" (name), "tcp-connection" (Connection), "stop" ({reason}), "error".
(function (root) {
  "use strict";

  const IS_NODE = typeof process !== "undefined" && !!(process.versions && process.versions.node) &&
    typeof window === "undefined";
  const JIT_SLOT_BASE = 1024;
  const JIT_SLOTS = 4096;
  const TABLE_SIZE = JIT_SLOT_BASE + JIT_SLOTS;
  const STACK_SIZE = 1 << 20;
  const BUDGET = 200000;
  const DEV_CONSOLE = 1, DEV_FS = 2, DEV_NET = 4;
  const STOP_REASONS = { 2: "poweroff", 3: "reset", 4: "halt", 5: "fault" };
  // SMP control block (Int32Array over a SharedArrayBuffer)
  const CTRL_PAUSE = 0, CTRL_PARKED = 1, CTRL_QUIT = 2, CTRL_EXITED = 3, CTRL_WORDS = 4;
  // Bytes buffered towards the guest before a sender waits
  const TCP_BACKLOG = 1 << 20;

  const utf8 = new TextEncoder();
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  class Emitter {
    constructor() { this._listeners = new Map(); }
    on(name, fn) {
      if (!this._listeners.has(name)) this._listeners.set(name, new Set());
      this._listeners.get(name).add(fn);
      return this;
    }
    off(name, fn) { this._listeners.get(name)?.delete(fn); return this; }
    // v86-style names
    add_listener(name, fn) { return this.on(name, fn); }
    remove_listener(name, fn) { return this.off(name, fn); }
    emit(name, ...args) {
      for (const fn of [...(this._listeners.get(name) || [])]) {
        try { fn(...args); } catch (e) { console.error(e); }
      }
    }
  }

  // ---------------------------------------------------------------- loading

  async function node_import(name) {
    return typeof require === "function" ? require(name) : import(name);
  }

  function is_url(s) { return /^(https?|blob|data):/i.test(s); }

  /** Bytes of a URL (or, in Node, a path), reporting progress. */
  async function load(src, on_progress) {
    if (src instanceof ArrayBuffer) return new Uint8Array(src);
    if (ArrayBuffer.isView(src)) return new Uint8Array(src.buffer, src.byteOffset, src.byteLength);
    if (IS_NODE && !is_url(src)) {
      const fs = await node_import("node:fs/promises");
      const data = new Uint8Array(await fs.readFile(src));
      on_progress?.(data.length, data.length);
      return data;
    }
    const response = await fetch(src);
    if (!response.ok) throw new Error(`${src}: ${response.status} ${response.statusText}`);
    const total = Number(response.headers.get("content-length")) || 0;
    if (!response.body?.getReader || !on_progress) return new Uint8Array(await response.arrayBuffer());
    const reader = response.body.getReader();
    const parts = [];
    let loaded = 0;
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      parts.push(value);
      loaded += value.length;
      on_progress(loaded, total);
    }
    const out = new Uint8Array(loaded);
    let at = 0;
    for (const p of parts) { out.set(p, at); at += p.length; }
    return out;
  }

  /** Stream a URL's (or path's) chunks to `on_chunk`. */
  async function stream(src, on_chunk, on_progress) {
    if (typeof src !== "string" || (IS_NODE && !is_url(src))) {
      const data = await load(src, on_progress);
      for (let i = 0; i < data.length; i += 1 << 20) on_chunk(data.subarray(i, i + (1 << 20)));
      return;
    }
    const response = await fetch(src);
    if (!response.ok) throw new Error(`${src}: ${response.status} ${response.statusText}`);
    const total = Number(response.headers.get("content-length")) || 0;
    if (!response.body?.getReader) {
      const data = new Uint8Array(await response.arrayBuffer());
      on_progress?.(data.length, data.length);
      on_chunk(data);
      return;
    }
    const reader = response.body.getReader();
    let loaded = 0;
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      loaded += value.length;
      on_progress?.(loaded, total);
      on_chunk(value);
    }
  }

  async function compile(src) {
    if (src instanceof WebAssembly.Module) return src;
    if (typeof src === "string" && !IS_NODE && WebAssembly.compileStreaming) {
      try { return await WebAssembly.compileStreaming(fetch(src)); } catch { /* wrong MIME type: below */ }
    }
    return WebAssembly.compile(await load(src));
  }

  // ------------------------------------------------------------ vCPU worker

  // Runs in a Worker (or a Node worker thread). Stringified: must not use anything
  // from outside itself.
  function vcpu_worker(node_port) {
    const post = (msg) => node_port ? node_port.postMessage(msg) : self.postMessage(msg);
    const listen = (fn) => node_port ? node_port.on("message", fn) : (self.onmessage = (e) => fn(e.data));
    const yield_now = typeof setImmediate === "function" ?
      () => new Promise((r) => setImmediate(r)) :
      () => new Promise((r) => { const ch = new MessageChannel(); ch.port1.onmessage = () => r(); ch.port2.postMessage(0); });

    listen(async (msg) => {
      if (!msg || !msg.module) return;
      const { memory, index } = msg;
      const ctrl = new Int32Array(msg.ctrl);
      const offset = performance.timeOrigin - msg.time_origin;
      const now = () => performance.now() + offset;
      const table = new WebAssembly.Table({ element: "anyfunc", initial: msg.table_size });
      let exports = null;
      const jit_imports = { m: memory };
      const env = {
        memory,
        __indirect_function_table: table,
        microtick: now,
        wall_clock_ms: () => Date.now(),
        host_notify: () => post({ t: "events" }),
        jit_compile: (slot, ptr, len) => {
          const code = new Uint8Array(memory.buffer, ptr >>> 0, len >>> 0).slice();
          WebAssembly.instantiate(code, { e: jit_imports }).then((r) => {
            table.set(slot, r.instance.exports.f);
            exports.armless_jit_ready(slot);
          }, (err) => post({ t: "log", msg: `armless: JIT module failed on CPU ${index}: ${err}` }));
          return 1;
        },
      };
      const instance = new WebAssembly.Instance(msg.module, { env });
      exports = instance.exports;
      exports.__stack_pointer.value = msg.stack_top;
      exports.__wasm_init_tls(msg.tls);
      exports.armless_smp_attach(index);
      jit_imports.armless_jit_read = exports.armless_jit_read;
      jit_imports.armless_jit_write = exports.armless_jit_write;
      jit_imports.armless_jit_interp = exports.armless_jit_interp;
      exports.armless_jit_config(msg.jit ? 1 : 0, msg.jit_threshold | 0);
      const mem32 = new Int32Array(memory.buffer);
      const wake = exports.armless_smp_wake_addr(index) >>> 2;
      post({ t: "ready" });

      let last_yield = now();
      for (;;) {
        if (Atomics.load(ctrl, 2)) break;
        if (Atomics.load(ctrl, 0)) {
          exports.armless_smp_parked();
          Atomics.add(ctrl, 1, 1);
          Atomics.notify(ctrl, 1);
          while (Atomics.load(ctrl, 0) && !Atomics.load(ctrl, 2)) Atomics.wait(ctrl, 0, 1, 50);
          Atomics.sub(ctrl, 1, 1);
          continue;
        }
        const woken = Atomics.load(mem32, wake);
        const result = exports.armless_run(msg.budget);
        if (result === 0 || result === 1) {
          if (result === 1) {
            const w = exports.armless_wake_ms();
            const timeout = w < 0 ? 20 : Math.min(20, Math.max(0, w - now()));
            if (timeout > 0) Atomics.wait(mem32, wake, woken, timeout);
          }
          // Let JIT compilations complete now and then.
          if (now() - last_yield > 4) {
            await yield_now();
            last_yield = now();
          }
          continue;
        }
        post({ t: "stop", result, pc: exports.armless_pc() });
        break;
      }
      Atomics.add(ctrl, 3, 1);
      Atomics.notify(ctrl, 3);
      post({ t: "exit" });
    });
  }

  function worker_source() {
    return "var node_port = typeof self === 'undefined' ? require('worker_threads').parentPort : null;\n" +
      "(" + vcpu_worker.toString() + ")(node_port);\n";
  }

  async function spawn_worker(on_message) {
    if (IS_NODE) {
      const { Worker } = await node_import("node:worker_threads");
      const w = new Worker(worker_source(), { eval: true });
      w.on("message", on_message);
      w.on("error", (e) => on_message({ t: "error", msg: String(e && e.stack || e) }));
      return { post: (m) => w.postMessage(m), terminate: () => w.terminate() };
    }
    const url = URL.createObjectURL(new Blob([worker_source()], { type: "text/javascript" }));
    const w = new Worker(url);
    w.onmessage = (e) => on_message(e.data);
    w.onerror = (e) => on_message({ t: "error", msg: String(e && e.message || e) });
    return { post: (m) => w.postMessage(m), terminate: () => w.terminate() };
  }

  // ------------------------------------------------------------ connections

  /** A TCP connection between the guest and this page. */
  class Connection extends Emitter {
    constructor(vm, id, host, port) {
      super();
      this.vm = vm;
      this.id = id;
      /** The name (or address) the guest connected to, and the port */
      this.host = host;
      this.port = port;
      this.sport = port; // v86 name
      this.state = "new"; // new | open | closed
      this.decided = false;
    }
    accept() {
      if (this.decided) return;
      this.decided = true;
      this.state = "open";
      this.vm._call("armless_tcp_accept", this.id);
    }
    refuse() {
      if (this.decided) return;
      this.decided = true;
      this.state = "closed";
      this.vm._call("armless_tcp_refuse", this.id);
      this.vm._conns.delete(this.id);
    }
    write(data) {
      if (this.state === "closed") return;
      const bytes = typeof data === "string" ? utf8.encode(data) : new Uint8Array(data);
      for (let i = 0; i < bytes.length; i += 1 << 20) {
        const part = bytes.subarray(i, i + (1 << 20));
        this.vm._with_bytes(part, (p, n) => this.vm._call("armless_tcp_send", this.id, p, n));
      }
    }
    /** Resolves once the guest has taken most of what was written. */
    async drain() {
      while (this.state !== "closed" && this.vm._call("armless_tcp_buffered", this.id) > TCP_BACKLOG) await sleep(5);
    }
    /** Close our side (after what was written). */
    close() {
      if (this.state === "closed") return;
      this.vm._call("armless_tcp_close", this.id);
    }
    abort() {
      if (this.state === "closed") return;
      this.state = "closed";
      this.vm._call("armless_tcp_abort", this.id);
      this.vm._conns.delete(this.id);
      this.emit("close");
    }
  }

  // -------------------------------------------------------------- web relay

  // The guest's connections to ports 80 and 443 end inside the machine (HTTPS terminated with
  // the site's CA); what comes out is a request to make or a WebSocket to open, done here with
  // the page's own fetch() and WebSocket.

  const HTTP_SKIP = /^(content-encoding|content-length|transfer-encoding|keep-alive|connection)$/i;

  function web_options(network) {
    const o = typeof network === "object" && network ? network : {};
    return {
      fetch: o.fetch || ((...a) => fetch(...a)),
      WebSocket: o.WebSocket || globalThis.WebSocket,
      // Pages on https:// may not fetch http:// (nor open ws://): ask for the same over TLS
      upgrade_https: o.upgrade_https ?? (typeof location !== "undefined" && location.protocol === "https:"),
      tls: o.tls,
    };
  }

  function read_str(dv, bytes, at) {
    const n = dv.getUint32(at, true);
    return [new TextDecoder().decode(bytes.subarray(at + 4, at + 4 + n)), at + 4 + n];
  }

  // ---------------------------------------------------------------- Armless

  class Armless extends Emitter {
    constructor(options) {
      super();
      this.options = options = { memory_mb: 512, cpus: 1, console: true, jit: true, autostart: true, ...options };
      this.cpus = options.cpus | 0 || 1;
      this.running = false;
      this.stopped = false;
      this._conns = new Map();
      this._blobs_in_flight = new Set();
      this._workers = [];
      this._timer = null;
      this._interval = null;
      this._draining = false;
      this.ready = this._init().catch((e) => {
        this.emit("error", e);
        throw e;
      });
    }

    // -------------------------------------------------------------- set up

    async _init() {
      const o = this.options;
      const smp = this.cpus > 1;
      if (smp && typeof SharedArrayBuffer === "undefined") {
        throw new Error("armless: several CPUs need SharedArrayBuffer (a cross-origin isolated page)");
      }
      const src = smp ? (o.wasm_smp || String(o.wasm || "armless.wasm").replace(/armless\.wasm$/, "armless-smp.wasm")) :
        (o.wasm || "armless.wasm");
      this.module = await compile(src);
      this.table = new WebAssembly.Table({ element: "anyfunc", initial: TABLE_SIZE });
      const env = {
        __indirect_function_table: this.table,
        microtick: () => performance.now(),
        wall_clock_ms: () => Date.now(),
        host_notify: () => this._notified(),
        jit_compile: (slot, ptr, len) => this._jit_compile(slot, ptr, len),
      };
      if (smp) {
        // Room above guest RAM for the machine's own data (cached files, JIT, ...)
        const pages = Math.min(65536, Math.ceil((o.memory_mb + 1024) * 16));
        this.memory = new WebAssembly.Memory({ initial: 256, maximum: pages, shared: true });
        env.memory = this.memory;
      }
      this.instance = await WebAssembly.instantiate(this.module, { env });
      const e = this.exports = this.instance.exports;
      if (!smp) this.memory = e.memory;
      if (e.__wasm_init_tls) {
        e.__wasm_init_tls(e.armless_alloc_aligned(e.__tls_size.value, e.__tls_align.value));
      }
      this._jit_imports = {
        m: this.memory,
        armless_jit_read: e.armless_jit_read,
        armless_jit_write: e.armless_jit_write,
        armless_jit_interp: e.armless_jit_interp,
      };

      const flags = (o.console !== false ? DEV_CONSOLE : 0) | (o.filesystem ? DEV_FS : 0) | (o.network ? DEV_NET : 0);
      let mac = 0;
      if (o.mac) mac = this._copy_in(Uint8Array.from(o.mac.split(":").map((x) => parseInt(x, 16))));
      if (e.armless_create(o.memory_mb, this.cpus, flags, mac) !== 0) throw new Error("armless: " + this._error());
      e.armless_jit_config(o.jit ? 1 : 0, o.jit_threshold | 0);
      // Testing aid: every block is compiled before it first runs
      if (o.jit_sync) e.armless_jit_sync(1);
      this._jit_pending = new Set();
      if (smp) e.armless_smp_host_cores(typeof navigator !== "undefined" && navigator.hardwareConcurrency || 0);

      if (o.filesystem) {
        const fsj = o.filesystem.json ?? o.filesystem.basefs;
        const json = typeof fsj === "object" && !(fsj instanceof ArrayBuffer) && !ArrayBuffer.isView(fsj) && fsj.url === undefined ?
          utf8.encode(JSON.stringify(fsj)) : await load(fsj?.url ?? fsj);
        this._with_bytes(json, (p, n) => {
          if (e.armless_fs_load(p, n) !== 0) throw new Error("armless: filesystem: " + this._error());
        });
      }

      if (o.initial_state) {
        await this._restore(o.initial_state.url ?? o.initial_state);
      } else if (o.boot) {
        await this._boot(o.boot);
      }
      if (o.network) {
        this._web_options = web_options(o.network);
        this._web = new Map();
        this._seed();
        if (this._web_options.tls) {
          const tls = this._web_options.tls;
          const json = typeof tls === "object" && !(tls instanceof ArrayBuffer) && !ArrayBuffer.isView(tls) ?
            utf8.encode(JSON.stringify(tls)) : await load(tls);
          this._with_bytes(json, (p, n) => {
            if (e.armless_tls(p, n) !== 0) throw new Error("armless: tls: " + this._error());
          });
        }
        this._interval = setInterval(() => this._call("armless_tick", performance.now()), 250);
      }
      this._drain();
      this.emit("ready");
      this.emit("emulator-loaded");
      if (o.autostart) this.run();
      return this;
    }

    async _restore(src) {
      const e = this.exports;
      let begun = false;
      const file = typeof src === "string" ? src : "initial_state";
      await stream(src, (chunk) => {
        if (!begun) {
          begun = true;
          const zstd = chunk[0] === 0x28 && chunk[1] === 0xb5 && chunk[2] === 0x2f && chunk[3] === 0xfd;
          e.armless_state_begin(zstd ? 1 : 0);
        }
        this._with_bytes(chunk, (p, n) => {
          if (e.armless_state_feed(p, n) !== 0) throw new Error("armless: snapshot: " + this._error());
        });
      }, (loaded, total) => {
        this.emit("progress", { file, loaded, total });
        this.emit("download-progress", { file_name: file, loaded, total, lengthComputable: total > 0 });
      });
      if (e.armless_state_finish() !== 0) throw new Error("armless: snapshot: " + this._error());
    }

    async _boot(boot) {
      const e = this.exports;
      const cmdline = boot.cmdline || "console=ttyS0";
      if (boot.raw) {
        // A bare-metal binary at `addr` (tests)
        const bin = await load(boot.raw.url ?? boot.raw);
        if (this._with_bytes(bin, (p, n) => e.armless_boot_raw(p, n, boot.addr ?? 0x200000)) !== 0) {
          throw new Error("armless: boot: " + this._error());
        }
        return;
      }
      if (boot.kernel) {
        const kernel = await load(boot.kernel.url ?? boot.kernel);
        const initrd = boot.initrd ? await load(boot.initrd.url ?? boot.initrd) : new Uint8Array(0);
        const k = this._copy_in(kernel), rd = this._copy_in(initrd), c = this._copy_in(utf8.encode(cmdline));
        const r = e.armless_boot_linux(k, kernel.length, rd, initrd.length, c, utf8.encode(cmdline).length);
        e.armless_free(k, kernel.length); e.armless_free(rd, initrd.length); e.armless_free(c, utf8.encode(cmdline).length);
        if (r !== 0) throw new Error("armless: boot: " + this._error());
        return;
      }
      // The kernel and initrd under /boot in the filesystem
      for (;;) {
        const r = this._with_bytes(utf8.encode(cmdline), (p, n) => e.armless_boot_from_fs(p, n));
        if (r === 0) return;
        if (r < 0) throw new Error("armless: boot: " + this._error());
        this._drain();
        await this._blobs_settled();
      }
    }

    async _blobs_settled() {
      while (this._blobs_in_flight.size) await sleep(5);
    }

    // ------------------------------------------------------------ running

    /** Start (or continue) running the machine. */
    run() {
      if (this.running || this.stopped) return;
      this.running = true;
      if (this.cpus > 1) this._start_workers();
      else this._schedule(0);
      this.emit("emulator-started");
      this.emit("started");
    }

    /** Pause the machine (it can be continued with run()). */
    async stop() {
      if (!this.running) return;
      this.running = false;
      if (this.cpus > 1) await this._pause();
      this._cancel();
    }

    _cancel() {
      if (this._timer === null) return;
      // A posted message can't be taken back: _on_message ignores it once _timer moved on
      if (this._immediate === "message") {}
      else if (this._immediate) clearImmediate(this._timer);
      else clearTimeout(this._timer);
      this._timer = null;
    }

    // Browsers clamp nested setTimeout(0) to 4 ms, which left one CPU idle about a third of
    // the time between 10 ms slices. A posted message runs as soon as the page has handled
    // input and painted, with no clamp.
    _post_slice() {
      if (!this._channel) {
        this._channel = new MessageChannel();
        this._channel.port1.onmessage = ({ data }) => {
          if (this._timer === "message" && data === this._posted) this._slice();
        };
      }
      this._posted = (this._posted | 0) + 1;
      this._timer = "message";
      this._immediate = "message";
      this._channel.port2.postMessage(this._posted);
    }

    _schedule(delay) {
      if (!this.running || this.cpus > 1) return;
      this._cancel();
      if (delay <= 0 && typeof setImmediate === "function") {
        this._timer = setImmediate(() => this._slice());
        this._immediate = true;
      } else if (delay <= 0 && typeof MessageChannel === "function") {
        this._post_slice();
      } else {
        this._timer = setTimeout(() => this._slice(), Math.max(0, delay));
        this._immediate = false;
      }
      this._wake_at = performance.now() + Math.max(0, delay);
    }

    /** Something happened that the idle guest should see now. */
    _poke() {
      if (this.cpus === 1 && this.running && this._wake_at > performance.now() + 1) this._schedule(0);
    }

    _slice() {
      this._timer = null;
      if (!this.running) return;
      const e = this.exports;
      const t0 = performance.now();
      let result;
      do {
        result = e.armless_run(BUDGET);
      } while (result === 0 && performance.now() - t0 < 10);
      this._drain();
      if (result === 0 && this.options.jit_sync && this._jit_pending.size) {
        Promise.all([...this._jit_pending]).then(() => this._schedule(0));
        return;
      }
      if (result === 0) return this._schedule(0);
      if (result === 1) {
        const w = e.armless_wake_ms();
        return this._schedule(w < 0 ? 100 : Math.min(100, w - performance.now()));
      }
      this._stopped(result);
    }

    _stopped(result) {
      this.running = false;
      this.stopped = true;
      const reason = STOP_REASONS[result] || "fault";
      // Let the last output out first
      this._drain();
      this.emit("stop", { reason, pc: this.exports.armless_pc() });
    }

    async _start_workers() {
      const e = this.exports;
      if (this._ctrl) {
        // Paused: continue
        Atomics.store(this._ctrl, CTRL_PAUSE, 0);
        Atomics.notify(this._ctrl, CTRL_PAUSE);
        return;
      }
      this._ctrl = new Int32Array(new SharedArrayBuffer(CTRL_WORDS * 4));
      for (let i = 0; i < this.cpus; i++) {
        const stack = e.armless_alloc_aligned(STACK_SIZE, 16);
        const tls = e.armless_alloc_aligned(e.__tls_size.value, e.__tls_align.value);
        const w = await spawn_worker((msg) => this._worker_message(i, msg));
        this._workers.push(w);
        w.post({
          module: this.module, memory: this.memory, index: i,
          stack_top: stack + STACK_SIZE, tls, ctrl: this._ctrl.buffer,
          time_origin: performance.timeOrigin, table_size: TABLE_SIZE,
          jit: this.options.jit, jit_threshold: this.options.jit_threshold | 0, budget: BUDGET,
        });
      }
    }

    _worker_message(i, msg) {
      switch (msg.t) {
        case "events": this._drain(); break;
        case "log": console.warn(msg.msg); break;
        case "error": this.emit("error", new Error(msg.msg)); break;
        case "stop":
          if (!this.stopped) {
            this.exports.armless_smp_stop();
            this._stopped(msg.result);
          }
          break;
      }
    }

    /** Park every vCPU worker between instruction slices. */
    async _pause() {
      const ctrl = this._ctrl;
      if (!ctrl) return;
      Atomics.store(ctrl, CTRL_PAUSE, 1);
      const mem32 = new Int32Array(this.memory.buffer);
      for (let i = 0; i < this.cpus; i++) Atomics.notify(mem32, this.exports.armless_smp_wake_addr(i) >>> 2);
      while (Atomics.load(ctrl, CTRL_PARKED) + Atomics.load(ctrl, CTRL_EXITED) < this.cpus) await sleep(1);
    }

    // --------------------------------------------------------- host input

    /** Type into the virtio console (hvc0): a string or bytes. Nothing is dropped. */
    console_input(data) {
      const bytes = typeof data === "string" ? utf8.encode(data) : new Uint8Array(data);
      this._with_bytes(bytes, (p, n) => this._call("armless_console_input", p, n));
      this._poke();
    }

    /** Tell the guest its console size. */
    console_resize(cols, rows) {
      this._call("armless_console_resize", cols, rows);
      this._poke();
    }

    serial_input(data) {
      const bytes = typeof data === "string" ? utf8.encode(data) : new Uint8Array(data);
      this._with_bytes(bytes, (p, n) => this._call("armless_serial_input", p, n));
      this._poke();
    }

    /** Hand the filesystem a blob before the guest asks (zstd or raw contents). */
    provide_blob(name, data) {
      const n = utf8.encode(name);
      const np = this._copy_in(n);
      try {
        this._with_bytes(new Uint8Array(data), (p, len) => this._call("armless_blob", np, n.length, p, len));
      } finally {
        this.exports.armless_free(np, n.length);
      }
      this._drain();
      this._poke();
    }

    has_blob(name) {
      const n = utf8.encode(name);
      return this._with_bytes(n, (p, len) => this._call("armless_has_blob", p, len)) !== 0;
    }

    /** Open a TCP connection to the guest's `port`; "connect" or "close" follows. */
    connect(port) {
      const id = this._call("armless_tcp_connect", port);
      if (!id) throw new Error("armless: no network device");
      const conn = new Connection(this, id, "guest", port);
      conn.decided = true;
      this._conns.set(id, conn);
      this._poke();
      return conn;
    }

    /** A snapshot of the machine (uncompressed). */
    async save_state() {
      await this.ready;
      const was_running = this.running;
      if (this.cpus > 1) await this._pause();
      const n = this.exports.armless_save_state();
      const state = new Uint8Array(this.memory.buffer, this.exports.armless_buf(), n).slice();
      if (this.cpus > 1 && was_running) {
        Atomics.store(this._ctrl, CTRL_PAUSE, 0);
        Atomics.notify(this._ctrl, CTRL_PAUSE);
      }
      return state;
    }

    /** Replace the machine's state with a snapshot (ArrayBuffer, bytes or URL). */
    async restore_state(src) {
      await this.ready;
      const was_running = this.running;
      await this.stop();
      await this._restore(src);
      if (this.options.network) this._seed();
      this._drain();
      if (was_running) this.run();
    }

    destroy() {
      this.running = false;
      this.stopped = true;
      this._cancel();
      this._channel?.port1.close();
      clearInterval(this._interval);
      if (this._ctrl) {
        Atomics.store(this._ctrl, CTRL_QUIT, 1);
        Atomics.notify(this._ctrl, CTRL_PAUSE);
        this.exports.armless_smp_stop();
      }
      for (const w of this._workers) w.terminate();
      this._workers = [];
    }

    /** Instructions executed so far. */
    get instruction_count() {
      return this.exports.armless_insn_count();
    }

    // ------------------------------------------------------------ plumbing

    _call(name, ...args) {
      return this.exports[name](...args);
    }

    _error() {
      const e = this.exports;
      return new TextDecoder().decode(new Uint8Array(this.memory.buffer, e.armless_error(), e.armless_error_len()).slice());
    }

    _copy_in(bytes) {
      const p = this.exports.armless_alloc(bytes.length);
      new Uint8Array(this.memory.buffer, p, bytes.length).set(bytes);
      return p;
    }

    _with_bytes(bytes, fn) {
      const p = this._copy_in(bytes);
      try {
        return fn(p, bytes.length);
      } finally {
        this.exports.armless_free(p, bytes.length);
      }
    }

    _jit_compile(slot, ptr, len) {
      const code = new Uint8Array(this.memory.buffer, ptr >>> 0, len >>> 0).slice();
      const p = WebAssembly.instantiate(code, { e: this._jit_imports }).then((r) => {
        this.table.set(slot, r.instance.exports.f);
        this.exports.armless_jit_ready(slot);
      }, (err) => console.warn("armless: JIT module failed to compile: " + err))
        .finally(() => this._jit_pending.delete(p));
      this._jit_pending.add(p);
      return 1;
    }

    _notified() {
      // Called from inside the machine: drain once the call returns.
      if (this._drain_queued) return;
      this._drain_queued = true;
      queueMicrotask(() => {
        this._drain_queued = false;
        this._drain();
      });
    }

    _drain() {
      if (this._draining || !this.exports) return;
      this._draining = true;
      try {
        for (;;) {
          const n = this.exports.armless_events();
          if (!n) break;
          const buf = new Uint8Array(this.memory.buffer, this.exports.armless_buf(), n).slice();
          const dv = new DataView(buf.buffer);
          for (let at = 0; at < n;) {
            const type = buf[at];
            const id = dv.getUint32(at + 1, true);
            const port = dv.getUint32(at + 5, true);
            const len = dv.getUint32(at + 9, true);
            const data = buf.subarray(at + 13, at + 13 + len);
            at += 13 + len;
            this._event(type, id, port, data);
          }
        }
      } finally {
        this._draining = false;
      }
    }

    _event(type, id, port, data) {
      const text = () => new TextDecoder().decode(data);
      switch (type) {
        case 1: this.emit("serial", data); this.emit("serial0-output-bytes", data); break;
        case 2: this.emit("console", data); this.emit("virtio-console0-output-bytes", data); break;
        case 3: this._fetch_blob(text()); break;
        case 4: {
          const conn = new Connection(this, id, text(), port);
          this._conns.set(id, conn);
          this.emit("tcp-connection", conn);
          // Ports other than 80 and 443: raw TCP, which a page can't make
          if (!conn.decided) conn.refuse();
          break;
        }
        case 5: {
          const c = this._conns.get(id);
          if (c) { c.state = "open"; c.emit("connect"); }
          break;
        }
        case 6: this._conns.get(id)?.emit("data", data); break;
        case 7: this._conns.get(id)?.emit("end"); break;
        case 8: {
          const c = this._conns.get(id);
          this._conns.delete(id);
          if (c && c.state !== "closed") { c.state = "closed"; c.emit("close"); }
          const w = this._web?.get(id);
          if (w) {
            this._web.delete(id);
            w.abort?.abort();
            if (w.socket && w.socket.readyState <= 1) w.socket.close();
          }
          break;
        }
        case 9: {
          const dv = new DataView(data.buffer, data.byteOffset, data.byteLength);
          let at = 0, method, url, k, v;
          [method, at] = read_str(dv, data, at);
          [url, at] = read_str(dv, data, at);
          const n = dv.getUint32(at, true);
          at += 4;
          const headers = [];
          for (let i = 0; i < n; i++) {
            [k, at] = read_str(dv, data, at);
            [v, at] = read_str(dv, data, at);
            headers.push([k, v]);
          }
          this._http(id, method, url, headers, data.slice(at));
          break;
        }
        case 10: {
          const dv = new DataView(data.buffer, data.byteOffset, data.byteLength);
          const [url, at] = read_str(dv, data, 0);
          const protocols = new TextDecoder().decode(data.subarray(at)).split(",").filter(Boolean);
          this._websocket(id, url, protocols);
          break;
        }
        case 11: {
          const socket = this._web?.get(id)?.socket;
          if (socket?.readyState === 1) socket.send(port ? new TextDecoder().decode(data) : data.slice());
          break;
        }
        case 12: {
          const socket = this._web?.get(id)?.socket;
          this._web?.delete(id);
          if (socket && socket.readyState <= 1) socket.close(port === 1000 || (port >= 3000 && port < 5000) ? port : 1000);
          break;
        }
      }
    }

    _seed() {
      const r = new Uint8Array(32);
      globalThis.crypto.getRandomValues(r);
      this._with_bytes(r, (p, n) => this._call("armless_seed", p, n));
    }

    _str_call(name, id, s, ...rest) {
      this._with_bytes(utf8.encode(s), (p, n) => this._call(name, id, ...rest, p, n));
    }

    /** One of the guest's HTTP(S) requests, made with fetch(). */
    async _http(id, method, url, headers, body) {
      const o = this._web_options;
      const w = { abort: typeof AbortController === "function" ? new AbortController() : null };
      this._web.set(id, w);
      const fail = (message) => {
        if (this._web.get(id) !== w) return;
        this._web.delete(id);
        this._str_call("armless_http_fail", id, message);
        this._drain();
        this._poke();
      };
      let target = url;
      if (o.upgrade_https && target.startsWith("http:")) target = "https:" + target.slice(5);
      const h = new Headers();
      for (const [k, v] of headers) {
        try { h.append(k, v); } catch { /* a header the browser won't send */ }
      }
      let response;
      try {
        response = await o.fetch(target, {
          method,
          headers: h,
          body: /^(GET|HEAD)$/i.test(method) ? undefined : body,
          signal: w.abort?.signal,
          redirect: "follow",
        });
      } catch (err) {
        return fail(`the browser could not fetch ${target} (the server must allow CORS): ${err}`);
      }
      if (this._web.get(id) !== w) return;
      const pairs = [];
      response.headers.forEach((v, k) => { if (!HTTP_SKIP.test(k)) pairs.push(k, v); });
      const reason = utf8.encode(response.statusText || "OK");
      const hb = utf8.encode(pairs.join("\0"));
      const rp = this._copy_in(reason);
      this._with_bytes(hb, (p, n) => this._call("armless_http_respond", id, response.status || 200, rp, reason.length, p, n));
      this.exports.armless_free(rp, reason.length);
      this._drain();
      this._poke();
      if (response.body) {
        const reader = response.body.getReader();
        try {
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            if (this._web.get(id) !== w) return reader.cancel();
            this._with_bytes(value, (p, n) => this._call("armless_http_body", id, p, n));
            this._poke();
            while (this._web.get(id) === w && this._call("armless_tcp_buffered", id) > TCP_BACKLOG) await sleep(5);
          }
        } catch {
          return fail("the transfer broke");
        }
      }
      if (this._web.get(id) !== w) return;
      this._web.delete(id);
      this._call("armless_http_end", id);
      this._drain();
      this._poke();
    }

    /** One of the guest's WebSockets, opened with the page's WebSocket. */
    _websocket(id, url, protocols) {
      const o = this._web_options;
      let target = url;
      if (o.upgrade_https && target.startsWith("ws:")) target = "wss:" + target.slice(3);
      let socket;
      try {
        socket = new o.WebSocket(target, protocols);
      } catch (err) {
        this._str_call("armless_http_fail", id, `the browser could not open ${target}: ${err}`);
        return this._drain();
      }
      socket.binaryType = "arraybuffer";
      const w = { socket };
      this._web.set(id, w);
      let opened = false;
      socket.onopen = () => {
        opened = true;
        this._str_call("armless_ws_accept", id, socket.protocol || "");
        this._drain();
        this._poke();
      };
      socket.onmessage = ({ data }) => {
        const text = typeof data === "string";
        const bytes = text ? utf8.encode(data) : new Uint8Array(data);
        this._with_bytes(bytes, (p, n) => this._call("armless_ws_send", id, text ? 1 : 0, p, n));
        this._drain();
        this._poke();
      };
      socket.onclose = ({ code }) => {
        if (this._web.get(id) !== w) return;
        this._web.delete(id);
        if (opened) this._call("armless_ws_close", id, code || 1005);
        else this._str_call("armless_http_fail", id, `the browser could not open ${target}`);
        this._drain();
        this._poke();
      };
    }

    async _fetch_blob(name) {
      if (this._blobs_in_flight.has(name)) return;
      this._blobs_in_flight.add(name);
      this.emit("blob", name);
      const fsopt = this.options.filesystem || {};
      try {
        const base = fsopt.baseurl || "";
        const url = IS_NODE && !is_url(base) ? base + name :
          String(new URL(name, typeof location !== "undefined" ? new URL(base, location.href) : base));
        const data = fsopt.fetch_blob ? await fsopt.fetch_blob(name) : await load(url);
        this.provide_blob(name, data);
      } catch (err) {
        console.warn(`armless: could not load ${name}: ${err}`);
        const n = utf8.encode(name);
        this._with_bytes(n, (p, len) => this._call("armless_blob_failed", p, len));
        this._drain();
      } finally {
        this._blobs_in_flight.delete(name);
        this._poke();
      }
    }
  }

  Armless.Connection = Connection;
  Armless.load = load;

  root.Armless = Armless;
  if (typeof module === "object" && module.exports) module.exports = { Armless };
})(typeof globalThis !== "undefined" ? globalThis : this);
