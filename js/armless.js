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
// User mode (no guest kernel): a Linux program from the filesystem runs on the machine's CPU
// and its system calls are answered in armless, with the console as its terminal. Instead of
// `boot`, `user: { argv: ["python3"], env: ["PATH=/usr/bin", ...], cwd: "/", cols, rows, reap_orphans }`
// starts one; `user_state` (URL or bytes, a process saved with save_process() or natively,
// system/user.bin of a snowglobe site) restores one next to `initial_state`, with `user_asleep_ms`
// (default 0) the time it was saved for, which its clocks and deadlines go on by. The events and
// console_input/console_resize are the same; "stop" says {reason: "exit", code} when it exits,
// and "idle" when it has gone quiet waiting for input.
//
// Everything the machine does runs in WebAssembly, devices included. This file only
// loads things, runs the CPUs (here, or one Web Worker per CPU) and answers the
// machine's requests: files of the 9p filesystem (fetched from `baseurl` the first
// time the guest reads them) and the guest's TCP connections ("tcp-connection"
// events; port 80 goes out through fetch() unless a listener takes it).
//
// Events: "ready", "console", "serial", "progress" ({file, loaded, total}),
// "blob" (name), "tcp-connection" (Connection), "stop" ({reason}), "error", and in user mode
// "idle" and "spawn-output" ({pid}: a job started with spawn() has output or ended, to poll).
(function (root) {
  "use strict";

  const IS_NODE = typeof process !== "undefined" && !!(process.versions && process.versions.node) &&
    typeof window === "undefined";
  const JIT_SLOT_BASE = 1024;
  const JIT_SLOTS = 4096;
  const TABLE_SIZE = JIT_SLOT_BASE + JIT_SLOTS;
  const STACK_SIZE = 1 << 20;
  const BUDGET = 200000;
  const DEV_CONSOLE = 1, DEV_FS = 2, DEV_NET = 4, DEV_BALLOON = 8;
  const STOP_REASONS = { 2: "poweroff", 3: "reset", 4: "halt", 5: "fault" };
  // User mode: how long a program runs in one go (ms), and the longest it sleeps idle before it
  // is looked at again (input, a resize and blobs wake it at once)
  const USER_SLICE_MS = 10;
  const USER_IDLE_MS = 1000;
  // SMP control block (Int32Array over a SharedArrayBuffer)
  const CTRL_PAUSE = 0, CTRL_PARKED = 1, CTRL_QUIT = 2, CTRL_EXITED = 3, CTRL_WORDS = 4;
  // What each vCPU worker's time went on (a Float64Array over a SharedArrayBuffer, STAT_WORDS a
  // CPU, each written by its worker alone): running the guest, waiting idle, letting the worker's
  // other tasks run (JIT compilations), slices run, idle waits, events posted to the page, times
  // it let them run, JIT compilations it asked for
  const STAT_RUN_MS = 0, STAT_IDLE_MS = 1, STAT_YIELD_MS = 2, STAT_SLICES = 3, STAT_IDLE_WAITS = 4,
    STAT_EVENTS = 5, STAT_YIELDS = 6, STAT_COMPILES = 7, STAT_WORDS = 8;
  const SMP_POLICIES = { adaptive: 0, "spin-count": 1 };
  // Bytes buffered towards the guest before a sender waits
  const TCP_BACKLOG = 1 << 20;

  const utf8 = new TextEncoder();
  // With the `debug` option: how often the memory is sampled, and how many samples are kept
  const DEBUG_SAMPLE_MS = 5000;
  const DEBUG_SAMPLES = 400;
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

  // Milliseconds to wait before each new try of a blob fetch that failed
  const BLOB_RETRIES = [500, 2000, 5000];

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
      // This CPU's words of the shared statistics (same layout as STAT_*: this is stringified)
      const stat = new Float64Array(msg.stats, index * 8 * 8, 8);
      const offset = performance.timeOrigin - msg.time_origin;
      const now = () => performance.now() + offset;
      // JIT compilations under way: they finish only when the worker lets its event loop run
      let compiling = 0;
      // Blocks compiled at once, to be marked ready once armless_run returns (not within it)
      const compiled = [];
      const table = new WebAssembly.Table({ element: "anyfunc", initial: msg.table_size });
      let exports = null;
      const jit_imports = { m: memory };
      const env = {
        memory,
        __indirect_function_table: table,
        microtick: now,
        wall_clock_ms: () => Date.now(),
        // When it was posted: the page counts how long such a message takes to reach it
        host_notify: () => { stat[5]++; post({ t: "events", at: now() }); },
        // A browser cannot give back the pages of a shared memory: the heap keeps them.
        heap_release: () => {},
        jit_compile: (slot, ptr, len) => {
          const code = new Uint8Array(memory.buffer, ptr >>> 0, len >>> 0).slice();
          stat[7]++;
          // A worker may compile and instantiate at once (a page's own thread only small modules):
          // the block is ready as soon as armless_run returns, with no turn of the event loop,
          // which costs tens of ms in Safari
          try {
            const instance = new WebAssembly.Instance(new WebAssembly.Module(code), { e: jit_imports });
            table.set(slot, instance.exports.f);
            compiled.push(slot);
            return 1;
          } catch {
            // Refused (or failed): compiled as a page's are, on the event loop
          }
          compiling++;
          WebAssembly.instantiate(code, { e: jit_imports }).then((r) => {
            compiling--;
            table.set(slot, r.instance.exports.f);
            exports.armless_jit_ready(slot);
          }, (err) => {
            compiling--;
            post({ t: "log", msg: `armless: JIT module failed on CPU ${index}: ${err}` });
          });
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
      jit_imports.armless_jit_fp3 = exports.armless_jit_fp3;
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
        const t0 = now();
        const result = exports.armless_run(msg.budget);
        while (compiled.length) exports.armless_jit_ready(compiled.pop());
        const t1 = now();
        stat[0] += t1 - t0;
        stat[3]++;
        if (result === 0 || result === 1) {
          if (result === 1) {
            const w = exports.armless_wake_ms();
            const timeout = w < 0 ? 20 : Math.min(20, Math.max(0, w - t1));
            if (timeout > 0) {
              Atomics.wait(mem32, wake, woken, timeout);
              stat[1] += now() - t1;
              stat[4]++;
            }
          }
          // Let JIT compilations under way complete now and then. Only then: a turn of the
          // worker's event loop costs well under a millisecond in Chrome but tens in Safari,
          // where a CPU yielding every few ms spent most of its time there (and the other CPU
          // waited for it)
          if (compiling > 0 && now() - last_yield > 4) {
            const t2 = now();
            await yield_now();
            stat[2] += now() - t2;
            stat[6]++;
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
      this.held = false;
    }
    /** Decide later: the guest's SYN waits until accept() or refuse(). */
    hold() {
      if (this.decided) return;
      this.decided = true;
      this.held = true;
    }
    accept() {
      if (this.decided && !this.held) return;
      this.decided = true;
      this.held = false;
      this.state = "open";
      this.vm._call("armless_tcp_accept", this.id);
    }
    refuse() {
      if (this.decided && !this.held) return;
      this.decided = true;
      this.held = false;
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
      // A guest waiting for this (in user mode, asleep until its next deadline) runs now
      this.vm._poke();
    }
    /** Resolves once the guest has taken most of what was written. */
    async drain() {
      while (this.state !== "closed" && this.vm._call("armless_tcp_buffered", this.id) > TCP_BACKLOG) await sleep(5);
    }
    /** Close our side (after what was written). */
    close() {
      if (this.state === "closed") return;
      this.vm._call("armless_tcp_close", this.id);
      this.vm._poke();
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
      // Guest TCP destinations carried over a WebSocket: [{ host?, port, url }]
      tunnels: (o.tunnels || []).map((t) => ({ ...t, host: t.host?.toLowerCase() })),
      // What the guest's requests and WebSockets may reach: `allow({ url, host, port, kind })`
      // (true, false or a promise of one) decides alone; otherwise private, loopback and
      // link-local destinations are refused unless `allow_private`
      allow: typeof o.allow === "function" ? o.allow : null,
      allow_private: !!o.allow_private,
    };
  }

  // ------------------------------------------------- what the guest may reach

  // A server-side runtime (Node, Bun, Deno): no browser stands between the guest and the host's
  // network, and fetch() there hands back redirects instead of following them out of sight
  const SERVER_SIDE = !!globalThis.process?.versions?.node || typeof globalThis.Deno !== "undefined";

  function v4_private(a, b, c) {
    return a === 0 || a === 10 || a === 127 || a >= 224 // this network, private, loopback, multicast, reserved
      || a === 100 && (b & 0xc0) === 64 // carrier-grade NAT
      || a === 169 && b === 254 // link-local, cloud metadata
      || a === 172 && (b & 0xf0) === 16
      || a === 192 && b === 168
      || a === 192 && b === 0 && c === 0
      || a === 198 && (b & 0xfe) === 18; // benchmarking: local proxies' fake addresses
  }

  /** The eight 16-bit groups of an IPv6 address (no brackets, no zone), or null. */
  function v6_groups(h) {
    let tail = null;
    const v4 = h.match(/^(.*:)(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
    if (v4) {
      const b = v4.slice(2).map(Number);
      if (b.some((x) => x > 255)) return null;
      h = v4[1] + "0:0";
      tail = [b[0] << 8 | b[1], b[2] << 8 | b[3]];
    }
    const halves = h.split("::");
    if (halves.length > 2) return null;
    const part = (x) => (x === "" ? [] : x.split(":").map((g) => (/^[0-9a-f]{1,4}$/i.test(g) ? parseInt(g, 16) : NaN)));
    const head = part(halves[0]);
    const rest = halves.length === 2 ? part(halves[1]) : [];
    const fill = halves.length === 2 ? 8 - head.length - rest.length : 0;
    if (fill < 0) return null;
    const g = [...head, ...new Array(fill).fill(0), ...rest];
    if (g.length !== 8 || g.some(Number.isNaN)) return null;
    if (tail) g.splice(6, 2, ...tail);
    return g;
  }

  /**
   * Whether a host is private: loopback, private, link-local or multicast, `localhost`, or an
   * IPv6 form that carries such an IPv4 address. `host` is a URL's hostname (IPv4 in dotted
   * form whatever shorthand the guest wrote, IPv6 in brackets) or an address a lookup gave.
   */
  function private_host(host) {
    let h = String(host).toLowerCase().replace(/\.$/, "");
    if (h.startsWith("[") && h.endsWith("]")) h = h.slice(1, -1);
    h = h.replace(/%.*$/, "");
    if (h === "" || h === "localhost" || h.endsWith(".localhost")) return true;
    const v4 = h.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
    if (v4) {
      const b = v4.slice(1).map(Number);
      return b.some((x) => x > 255) || v4_private(b[0], b[1], b[2]);
    }
    if (!h.includes(":")) return false; // a name
    const g = v6_groups(h);
    if (!g) return true; // no address anyone should be dialling
    const low = () => v4_private(g[6] >> 8, g[6] & 0xff, g[7] >> 8);
    const zero = (from, to) => g.slice(from, to).every((x) => x === 0);
    return zero(0, 8) // unspecified
      || zero(0, 7) && g[7] === 1 // loopback
      || (g[0] & 0xfe00) === 0xfc00 // unique local
      || (g[0] & 0xffc0) === 0xfe80 || (g[0] & 0xffc0) === 0xfec0 // link- and site-local
      || (g[0] & 0xff00) === 0xff00 // multicast
      || zero(0, 5) && (g[5] === 0xffff || g[5] === 0) && low() // IPv4-mapped and -compatible
      || g[0] === 0x64 && g[1] === 0xff9b && (g[2] === 1 || zero(2, 6) && low()) // NAT64
      || g[0] === 0x2002 && v4_private(g[1] >> 8, g[1] & 0xff, g[2] >> 8) // 6to4
      || g[0] === 0x2001 && g[1] === 0; // Teredo
  }

  /**
   * Why the guest may not reach `url` (a request's or WebSocket's, as it will be made), or null
   * if it may. `kind` is "http" or "websocket".
   */
  async function egress_refusal(o, url, kind) {
    let u;
    try {
      u = new URL(url);
    } catch {
      return `${url} is not a URL`;
    }
    const host = u.hostname.replace(/^\[|\]$/g, "");
    const secure = u.protocol === "https:" || u.protocol === "wss:";
    const port = u.port ? Number(u.port) : secure ? 443 : 80;
    if (o.allow) {
      let ok = false;
      try {
        ok = await o.allow({ url: u.href, host, port, kind });
      } catch { /* refused */ }
      return ok ? null : `${u.host} is not allowed for the guest`;
    }
    if (o.allow_private) return null;
    // The page's own origin: what the page serves, the guest may have too (without the
    // visitor's cookies, which are never sent)
    if (!SERVER_SIDE && typeof location !== "undefined"
      && location.origin === `${secure ? "https" : "http"}://${u.host}`) return null;
    if (private_host(u.hostname)) return `${host} is a private address: not reachable from the guest`;
    // A name, where no browser stands in the way: what it resolves to. (fetch() looks it up
    // again, so a name whose answer changes in between gets past this: a host that must be sure
    // runs the native host, whose Egress dials only the addresses it checked, or sets `allow`.)
    const dns = SERVER_SIDE && !host.includes(":") && !/^[\d.]+$/.test(host)
      ? globalThis.process?.getBuiltinModule?.("node:dns") : null;
    if (dns) {
      try {
        const found = await dns.promises.lookup(host, { all: true });
        const bad = found.find((a) => private_host(a.address));
        if (bad) return `${host} is a private address (${bad.address}): not reachable from the guest`;
      } catch { /* fetch() reports a name that doesn't resolve */ }
    }
    return null;
  }

  function tunnel_for(tunnels, host, port) {
    host = host.toLowerCase();
    return tunnels.find((t) => t.port === port && (t.host == null || t.host === host));
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
      // Running a program with no kernel (the `user` or `user_state` option)
      this.user = false;
      this._conns = new Map();
      this._blobs_in_flight = new Set();
      this._workers = [];
      // With several CPUs: what each worker's time went on, and how late their events reached this thread
      this._stats = null;
      this._event_delay = { count: 0, ms: 0, max_ms: 0 };
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
      const user = !!(o.user || o.user_state);
      if (user && smp) throw new Error("armless: user mode runs on one CPU (cpus: 1)");
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
        heap_release: () => {},
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
        armless_jit_fp3: e.armless_jit_fp3,
      };

      const flags = (o.console !== false ? DEV_CONSOLE : 0) | (o.filesystem ? DEV_FS : 0) | (o.network ? DEV_NET : 0)
        | (o.free_page_reporting ? DEV_BALLOON : 0);
      let mac = 0;
      if (o.mac) mac = this._copy_in(Uint8Array.from(o.mac.split(":").map((x) => parseInt(x, 16))));
      if (e.armless_create(o.memory_mb, this.cpus, flags, mac) !== 0) throw new Error("armless: " + this._error());
      e.armless_jit_config(o.jit ? 1 : 0, o.jit_threshold | 0);
      if (o.debug) {
        // Off by default: samples of the memory, every few seconds and where it moves (see memory_report)
        this.debug = { samples: [] };
        this._debug_sample("created");
        this._debug_timer = setInterval(() => this._debug_sample("tick"), DEBUG_SAMPLE_MS);
      }
      // Decompressed file contents kept in this page (the guest caches what it uses itself)
      if (o.fs_cache_mb != null) e.armless_set_fs_cache(o.fs_cache_mb >>> 0);
      // Pre-decoded instructions each CPU keeps (24 MiB by default): what the interpreter runs from
      if (o.predecode_mb != null && e.armless_set_predecode) e.armless_set_predecode(o.predecode_mb >>> 0);
      // Testing aid: every block is compiled before it first runs
      if (o.jit_sync) e.armless_jit_sync(1);
      this._jit_pending = new Set();
      if (smp) e.armless_smp_host_cores(typeof navigator !== "undefined" && navigator.hardwareConcurrency || 0);
      if (smp && o.smp_policy != null) this.set_smp_policy(o.smp_policy);

      if (o.filesystem) {
        const fsj = o.filesystem.json ?? o.filesystem.basefs;
        const json = typeof fsj === "object" && !(fsj instanceof ArrayBuffer) && !ArrayBuffer.isView(fsj) && fsj.url === undefined ?
          utf8.encode(JSON.stringify(fsj)) : await load(fsj?.url ?? fsj);
        this._with_bytes(json, (p, n) => {
          if (e.armless_fs_load(p, n) !== 0) throw new Error("armless: filesystem: " + this._error());
        });
      }

      // Fresh randomness for the guest (a new VM generation ID) and for TLS: after a restore,
      // whose state it replaces, and before a boot, which hands it to Linux as /chosen/rng-seed
      if (user && !e.armless_user_exec) throw new Error("armless: this build has no user mode");
      if (o.initial_state) {
        await this._restore(o.initial_state.url ?? o.initial_state);
        this._seed();
        if (o.user_state) await this._user_restore(o.user_state.url ?? o.user_state, o.user_asleep_ms);
      } else {
        this._seed();
        if (o.user) await this._user_exec(o.user);
        else if (o.boot) await this._boot(o.boot);
      }
      if (o.network) {
        this._web_options = web_options(o.network);
        this._web = new Map();
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

    /**
     * What this page's memory holds, as far as armless can tell it apart. The linear memory of a machine never shrinks
     * and the browser reports one number for the page, so a host that wants to know why it is big needs the pieces:
     *
     *   memory      bytes of the whole linear memory (what `memory.buffer.byteLength` says)
     *   guest_ram   the guest's RAM, and `guest_ram_at` where it starts in the memory
     *   heap        what the host side of the machine allocated: `live` bytes now, the `peak`, how much of `live` is in
     *               big blocks (`large`) and how much of it is freed blocks kept to be handed out again (`kept`)
     *   fs          what the 9p file system holds: the blobs it fetched (`cached_bytes` of at most `cache_limit`) and
     *               the files the guest wrote (`written_bytes`, in buffers made at `written_capacity` bytes)
     *   jit         blocks compiled and how many times the cache was flushed
     *   pages       with `scan` (the default), the 4 KiB pages of the memory that hold a non-zero byte, for the parts
     *               before the RAM, the RAM and after it. A page that was never written, or was given back (the big
     *               blocks that are freed are discarded to zeros), is zero, so this is the memory the machine really
     *               uses, as opposed to `memory`, which is only as far as it has grown. It reads the memory (a few ms
     *               for 100 MB).
     */
    memory_report({ scan = true } = {}) {
      const e = this.exports;
      if (!e.armless_mem_report) throw new Error("armless: this build has no memory report");
      const n = e.armless_mem_report();
      const r = JSON.parse(new TextDecoder().decode(new Uint8Array(this.memory.buffer, e.armless_buf(), n).slice()));
      // The memory may have grown since the report was made: its size now
      r.memory = this.memory.buffer.byteLength;
      if (scan) {
        const buf = this.memory.buffer;
        const words = new Uint32Array(buf);
        const count = (from, to) => {
          let pages = 0, used = 0;
          for (let w = from >>> 2, end = to >>> 2; w < end; w += 1024, pages++) {
            const stop = Math.min(w + 1024, end);
            for (let i = w; i < stop; i++) {
              if (words[i] !== 0) { used++; break; }
            }
          }
          return { bytes: pages * 4096, used: used * 4096 };
        };
        const at = Math.min(r.guest_ram_at, r.memory), end = Math.min(r.guest_ram_at + r.guest_ram, r.memory);
        r.pages = { before_ram: count(0, at), guest_ram: count(at, end), after_ram: count(end, r.memory) };
      }
      if (r.smp) Object.assign(r.smp, this._smp_js());
      return r;
    }

    /**
     * With several CPUs, what they did and what their waits for one another cost: the CPUs'
     * instructions, each kind of wait (a TLB invalidation, the guest's WFE, a lock), how waiting
     * CPUs behave (`policy`) and what was learned (`slow_waits`), and, from the workers, where their
     * time went and how late their device events reached this thread. null with one CPU.
     */
    smp_report() {
      if (this.cpus < 2) return null;
      return this.memory_report({ scan: false }).smp ?? null;
    }

    _smp_js() {
      const s = this._stats;
      const workers = [];
      for (let i = 0; s && i < this.cpus; i++) {
        const w = s.subarray(i * STAT_WORDS, (i + 1) * STAT_WORDS);
        const round = (x) => Math.round(x * 10) / 10;
        workers.push({
          run_ms: round(w[STAT_RUN_MS]), idle_ms: round(w[STAT_IDLE_MS]), yield_ms: round(w[STAT_YIELD_MS]),
          slices: w[STAT_SLICES], idle_waits: w[STAT_IDLE_WAITS], events: w[STAT_EVENTS],
          yields: w[STAT_YIELDS], compiles: w[STAT_COMPILES],
        });
      }
      const d = this._event_delay;
      return {
        workers,
        event_delay: { count: d.count, ms: Math.round(d.ms * 10) / 10, max_ms: Math.round(d.max_ms * 10) / 10 },
      };
    }

    /**
     * How waiting CPUs behave: "adaptive" (the default: they spin a little, then give up their
     * thread now and then, and learn when the host has fewer cores free than it says) or
     * "spin-count" (as armless 0.12: spin a set number of times, more when the host reports more
     * cores than CPUs). Returns the policy before. Also the `smp_policy` option.
     */
    set_smp_policy(policy) {
      const e = this.exports;
      const n = SMP_POLICIES[policy];
      if (n === undefined) throw new Error(`armless: smp_policy is "adaptive" or "spin-count", not ${JSON.stringify(policy)}`);
      if (!e.armless_smp_policy || this.cpus < 2) return null;
      const before = e.armless_smp_policy(n);
      return Object.keys(SMP_POLICIES).find((k) => SMP_POLICIES[k] === before) ?? null;
    }

    /** A sample of the memory without the scan (cheap), when `debug` is on: kept in `this.debug.samples`, the last 400. */
    _debug_sample(label) {
      if (!this.debug) return;
      let r;
      try { r = this.memory_report({ scan: false }); } catch { return; }
      const s = { t: Math.round(performance.now()), label, memory: r.memory, heap: r.heap, fs: r.fs };
      const samples = this.debug.samples;
      samples.push(s);
      if (samples.length > DEBUG_SAMPLES) samples.splice(0, samples.length - DEBUG_SAMPLES);
      this.emit("memory", s);
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
      this._debug_sample("restored");
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

    // ------------------------------------------------------------ user mode

    /**
     * Start a program from the filesystem with no kernel: { argv, env, cwd, cols, rows, plain_std, hostname,
     * reap_orphans }. `plain_std` gives it the standard streams of a container without a terminal (stdin reads end of
     * file, stdout and stderr are no terminal); `hostname` is what uname says (default "armless").
     */
    async _user_exec(user) {
      const e = this.exports;
      const argv = typeof user.argv === "string" ? [user.argv] : user.argv || [];
      if (!argv.length) throw new Error("armless: user mode needs a program (user.argv)");
      if (user.reap_orphans && !e.armless_user_reap_orphans) throw new Error("armless: this build cannot reap orphans");
      // argv and env as lists of NUL-terminated items (one with a NUL in it would be two: execve
      // can't pass one either)
      const list = (items) => {
        const s = items.map(String);
        if (s.some((x) => x.includes("\0"))) throw new Error("armless: user mode: a NUL in an argument or variable");
        return utf8.encode(s.map((x) => x + "\0").join(""));
      };
      const parts = [list(argv), list(user.env || []), utf8.encode(user.cwd || "/")];
      const name = utf8.encode(user.hostname || "");
      if ((user.plain_std && !e.armless_user_plain_std) || (name.length && !e.armless_user_hostname)) {
        throw new Error("armless: this build has no exec options");
      }
      const ptrs = parts.map((b) => this._copy_in(b));
      try {
        // The exec options stay set while its executable and loader are fetched
        if (user.plain_std) e.armless_user_plain_std(1);
        if (name.length) {
          const at = e.armless_user_hostname(name.length);
          if (!at) throw new Error("armless: user mode: " + this._error());
          // read the memory after the call: it may have grown
          new Uint8Array(this.memory.buffer, at, name.length).set(name);
        }
        for (;;) {
          const r = e.armless_user_exec(ptrs[0], parts[0].length, ptrs[1], parts[1].length, ptrs[2], parts[2].length);
          if (r === 0) break;
          if (r < 0) throw new Error("armless: user mode: " + this._error());
          this._drain();
          await this._blobs_settled();
        }
      } finally {
        ptrs.forEach((p, i) => e.armless_free(p, parts[i].length));
        // Back to the defaults however it ended (a fetch that failed leaves them set otherwise)
        if (e.armless_user_plain_std) e.armless_user_plain_std(0);
        if (e.armless_user_hostname) e.armless_user_hostname(0);
      }
      this.user = true;
      if (user.reap_orphans) e.armless_user_reap_orphans(1);
      e.armless_user_resize(user.cols || 80, user.rows || 24);
    }

    /**
     * The process of a snapshot, on the machine `initial_state` restored (URL or bytes). `asleep_ms`
     * is how long it was saved for: its clocks go on by that much, and what was due in it is due now.
     */
    async _user_restore(src, asleep_ms = 0) {
      const file = typeof src === "string" ? src : "user_state";
      const bytes = await load(src, (loaded, total) => this.emit("progress", { file, loaded, total }));
      this.exports.armless_user_asleep_ms(asleep_ms ?? 0);
      if (this._with_bytes(bytes, (p, n) => this.exports.armless_user_restore(p, n)) !== 0) {
        throw new Error("armless: user state: " + this._error());
      }
      this.user = true;
    }

    /** User mode: run the program a slice, and come back when it has more to do. */
    _user_slice() {
      const e = this.exports;
      const result = e.armless_user_run(USER_SLICE_MS);
      this._user_waiting = result === 1;
      this._drain();
      this._spawn_events();
      if (result === 0) {
        this._user_idle = false;
        return this._schedule(0);
      }
      if (result === 1) {
        const w = e.armless_user_wake_ms();
        // Waiting for input, not sleeping or waiting for files on their way: said once until it
        // runs again
        if (!this._user_idle && e.armless_user_waiting_for_input()) {
          this._user_idle = true;
          this.emit("idle");
        }
        return this._schedule(w < 0 ? USER_IDLE_MS : Math.min(USER_IDLE_MS, w - performance.now()));
      }
      if (result === 2) return this._stopped(2, { reason: "exit", code: e.armless_user_exit_code() });
      this._stopped(5, { reason: "fault", message: this._error() });
    }

    /**
     * User mode: what the program is doing. state: "running", "idle" (every thread waits:
     * `waiting_for_input` says whether for input (a REPL at its prompt) rather than only for time,
     * `waiting_on_blobs` whether on files being fetched, `wake_ms` is the performance.now() time
     * of its next deadline or -1), "exited" (with `code`) or "fault".
     */
    get user_status() {
      if (!this.user) return null;
      const e = this.exports;
      const code = e.armless_user_exit_code();
      if (this.stopped) return code >= 0 ? { state: "exited", code } : { state: "fault" };
      return {
        state: this._user_waiting ? "idle" : "running",
        waiting_for_input: e.armless_user_waiting_for_input() !== 0,
        waiting_on_blobs: e.armless_user_waiting_on_blobs() !== 0,
        wake_ms: this._user_waiting ? e.armless_user_wake_ms() : -1,
      };
    }

    /** User mode: end the program's input (its reads past what is left return 0). */
    close_input() {
      if (!this.user) return;
      this.exports.armless_user_close_input();
      this._poke();
    }

    /** User mode: a signal for the program, as kill(2) sends it (default 2, SIGINT: what Ctrl-C
     *  raises from the terminal, for a stop button that doesn't go through it). */
    kill(sig = 2) {
      if (!this.user) return;
      this.exports.armless_user_kill(sig);
      this._poke();
    }

    /**
     * User mode: the process's state (uncompressed). It goes with the machine's (save_state_pieces,
     * save_state) as `user_state` goes with `initial_state`, and has the bytes of a native host's
     * Process::save. Take both while the machine is stopped. Refused while a process started with
     * spawn() is held (not released): a snapshot keeps no host end of a job's pipes.
     */
    async save_process() {
      await this.ready;
      if (!this.user) throw new Error("armless: no user-mode process");
      const e = this.exports;
      const n = e.armless_user_save();
      if (n === 0) throw new Error("armless: " + this._error());
      return new Uint8Array(this.memory.buffer, e.armless_buf(), n).slice();
    }

    /**
     * User mode: start a process beside the program, as `Process::spawn` does natively: `{ argv, env, cwd }`
     * (`argv[0]` is found by PATH from `env` when it has no slash; `cwd` defaults to "/"). It runs as root, in a
     * session of its own with no parent, while the machine runs (run(); a stopped machine runs nothing), with
     * pipes for stdin, stdout and stderr the returned job holds the other ends of:
     *
     *   job.pid
     *   job.write(bytesOrString)  true if accepted, false once its stdin is gone (closed, or the job released)
     *   job.closeInput()          its reads past what is written return 0
     *   job.poll()                what it printed since the last poll: { stdout, stderr: Uint8Array, exit }, exit
     *                             being null while it runs, else 0..255 (128 + the signal for one a signal ended).
     *                             Throws once the job is released. The machine emits "spawn-output" ({ pid }) once
     *                             when a job has something to poll (output, or its end), until it is polled.
     *   job.kill(sig = 15)        a signal for its process group: it and what it started
     *   job.release()             done with it: its pipes let go and, once it ended, the process taken away
     *
     * Its program's files that the filesystem hasn't got yet are fetched first, as for `user`. Throws when the
     * machine isn't in user mode or the build has no spawn.
     */
    async spawn({ argv, env = [], cwd = "/" } = {}) {
      await this.ready;
      if (!this.user) throw new Error("armless: spawn needs user mode");
      const e = this.exports;
      if (!e.armless_user_spawn) throw new Error("armless: this build has no spawn");
      argv = typeof argv === "string" ? [argv] : argv || [];
      if (!argv.length) throw new Error("armless: spawn needs a program (argv)");
      const list = (items) => {
        const s = items.map(String);
        if (s.some((x) => x.includes("\0"))) throw new Error("armless: spawn: a NUL in an argument or variable");
        return utf8.encode(s.map((x) => x + "\0").join(""));
      };
      const parts = [list(argv), list(env), utf8.encode(cwd || "/")];
      const ptrs = parts.map((b) => this._copy_in(b));
      let pid;
      try {
        for (;;) {
          const r = Number(e.armless_user_spawn(ptrs[0], parts[0].length, ptrs[1], parts[1].length, ptrs[2], parts[2].length));
          if (r >= 0) { pid = r; break; }
          if (r === -1) throw new Error("armless: spawn: " + this._error());
          // -2: its files are on their way
          this._drain();
          await this._blobs_settled();
        }
      } finally {
        ptrs.forEach((p, i) => e.armless_free(p, parts[i].length));
      }
      this._poke();
      return this._job(pid);
    }

    /** Spawned jobs with something to poll, said once each until polled: "spawn-output" ({ pid }). */
    _spawn_events() {
      const e = this.exports;
      if (!this._jobs?.size || !e.armless_user_spawn_pending) return;
      const n = e.armless_user_spawn_pending();
      if (!n) return;
      const v = new DataView(this.memory.buffer, e.armless_buf(), n);
      for (let i = 0; i + 8 <= n; i += 8) {
        const pid = Number(v.getBigInt64(i, true));
        const job = this._jobs.get(pid);
        if (job && !job.notified) {
          job.notified = true;
          this.emit("spawn-output", { pid });
        }
      }
    }

    _job(pid) {
      const vm = this;
      const big = BigInt(pid);
      let released = false;
      const state = { notified: false };
      (this._jobs ??= new Map()).set(pid, state);
      const bytes = (call) => {
        const e = vm.exports;
        const n = e[call]();
        return new Uint8Array(vm.memory.buffer, e.armless_buf(), n).slice();
      };
      return {
        pid,
        write(data) {
          if (released) return false;
          const b = typeof data === "string" ? utf8.encode(data) : data;
          const ok = vm._with_bytes(b, (p, n) => vm.exports.armless_user_spawn_input(big, p, n)) !== 0;
          vm._poke();
          return ok;
        },
        closeInput() {
          if (released) return;
          vm.exports.armless_user_spawn_close_input(big);
          vm._poke();
        },
        poll() {
          const e = vm.exports;
          if (released || e.armless_user_spawn_poll(big) < 0) throw new Error(`armless: spawn: process ${pid} is released or unknown`);
          const stdout = bytes("armless_user_spawn_stdout");
          const stderr = bytes("armless_user_spawn_stderr");
          state.notified = false;
          const code = e.armless_user_spawn_exit();
          // (A writer that was waiting on a full pipe goes on)
          if (stdout.length || stderr.length) vm._poke();
          return { stdout, stderr, exit: code < 0 ? null : code };
        },
        kill(sig = 15) {
          if (released) return;
          vm.exports.armless_user_spawn_kill(big, sig);
          vm._poke();
        },
        release() {
          if (released) return;
          released = true;
          vm._jobs.delete(pid);
          vm.exports.armless_user_spawn_release(big);
          vm._poke();
        },
      };
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
      if (this.user) return this._user_slice();
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

    _stopped(result, info) {
      this.running = false;
      this.stopped = true;
      const reason = STOP_REASONS[result] || "fault";
      // Let the last output out first
      this._drain();
      this.emit("stop", { reason, pc: this.exports.armless_pc(), ...info });
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
      this._stats = new Float64Array(new SharedArrayBuffer(this.cpus * STAT_WORDS * 8));
      for (let i = 0; i < this.cpus; i++) {
        const stack = e.armless_alloc_aligned(STACK_SIZE, 16);
        const tls = e.armless_alloc_aligned(e.__tls_size.value, e.__tls_align.value);
        const w = await spawn_worker((msg) => this._worker_message(i, msg));
        this._workers.push(w);
        w.post({
          module: this.module, memory: this.memory, index: i,
          stack_top: stack + STACK_SIZE, tls, ctrl: this._ctrl.buffer, stats: this._stats.buffer,
          time_origin: performance.timeOrigin, table_size: TABLE_SIZE,
          jit: this.options.jit, jit_threshold: this.options.jit_threshold | 0, budget: BUDGET,
        });
      }
    }

    _worker_message(i, msg) {
      switch (msg.t) {
        case "events": {
          // How long the message took to reach this thread: a device request waits that long
          if (msg.at != null) {
            const late = Math.max(0, performance.now() - msg.at);
            const d = this._event_delay;
            d.count++;
            d.ms += late;
            if (late > d.max_ms) d.max_ms = late;
          }
          this._drain();
          break;
        }
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
      if (this.user) this._user_idle = false;
      this._with_bytes(bytes, (p, n) => this._call(this.user ? "armless_user_input" : "armless_console_input", p, n));
      this._poke();
    }

    /** Tell the guest its console size. */
    console_resize(cols, rows) {
      this._call(this.user ? "armless_user_resize" : "armless_console_resize", cols, rows);
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

    /**
     * A snapshot of the machine (uncompressed), in pieces of about 1 MiB, in order: the machine
     * hands it out a piece at a time, so its memory (which never shrinks again) holds one piece
     * of it, not the whole, and a host that writes or compresses the pieces as it goes never
     * holds the whole either. The CPUs stay stopped while it is taken.
     */
    async save_state_pieces() {
      await this.ready;
      const e = this.exports;
      const was_running = this.running;
      if (this.cpus > 1) await this._pause();
      try {
        // A machine built before the snapshot came in pieces: the one buffer
        if (!e.armless_save_begin) {
          const n = e.armless_save_state();
          return [new Uint8Array(this.memory.buffer, e.armless_buf(), n).slice()];
        }
        this._debug_sample("save begins");
        if (e.armless_save_begin() !== 0) throw new Error("armless: snapshot: " + this._error());
        // No await until the last piece: nothing runs the machine in between
        const pieces = [];
        try {
          for (let n; (n = e.armless_save_next()) !== 0;) {
            pieces.push(new Uint8Array(this.memory.buffer, e.armless_buf(), n).slice());
          }
        } catch (err) {
          e.armless_save_abort();
          throw err;
        }
        this._debug_sample("save ends");
        return pieces;
      } finally {
        if (this.cpus > 1 && was_running) {
          Atomics.store(this._ctrl, CTRL_PAUSE, 0);
          Atomics.notify(this._ctrl, CTRL_PAUSE);
        }
      }
    }

    /** A snapshot of the machine (uncompressed), in one buffer (see save_state_pieces). */
    async save_state() {
      const pieces = await this.save_state_pieces();
      if (pieces.length === 1) return pieces[0];
      const state = new Uint8Array(pieces.reduce((n, p) => n + p.length, 0));
      let at = 0;
      for (let i = 0; i < pieces.length; i++) {
        state.set(pieces[i], at);
        at += pieces[i].length;
        pieces[i] = null; // each piece can go as soon as it is in
      }
      return state;
    }

    /**
     * Replace the machine's state with a snapshot (ArrayBuffer, bytes or URL), and in user mode the
     * process with `user_state`, saved with it (without one, no process runs: a kernel would).
     * `user_asleep_ms` is how long they were saved for (see `user_state` above).
     */
    async restore_state(src, { user_state, user_asleep_ms } = {}) {
      await this.ready;
      const was_running = this.running;
      await this.stop();
      await this._restore(src);
      this._seed();
      this.user = false;
      this._user_idle = false;
      if (user_state) await this._user_restore(user_state.url ?? user_state, user_asleep_ms);
      this._drain();
      if (was_running) this.run();
    }

    destroy() {
      this.running = false;
      this.stopped = true;
      this._cancel();
      this._channel?.port1.close();
      clearInterval(this._interval);
      clearInterval(this._debug_timer);
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
        case 2: if (this.user) this._user_idle = false; this.emit("console", data); this.emit("virtio-console0-output-bytes", data); break;
        case 3: this._fetch_blob(text()); break;
        case 4: {
          const conn = new Connection(this, id, text(), port);
          this._conns.set(id, conn);
          const t = this._web_options && tunnel_for(this._web_options.tunnels, conn.host, port);
          if (t) this._tunnel(conn, t.url);
          else this.emit("tcp-connection", conn);
          // Ports other than 80 and 443: raw TCP, which a page can't make
          if (!conn.decided) conn.refuse();
          break;
        }
        case 5: {
          const c = this._conns.get(id);
          if (c) { c.state = "open"; c.emit("connect"); }
          break;
        }
        case 6: {
          // The guest's data for a connection: once it has gone where it goes, the guest may
          // send more (flow control). A listener takes it at once; a tunnel once its WebSocket
          // sent it (`consume_later`); a connection no longer here drops it.
          const c = this._conns.get(id);
          if (c) c.emit("data", data);
          if (!c?.consume_later) this._call("armless_tcp_consumed", id, data.length);
          break;
        }
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
      const refused = await egress_refusal(o, target, "http");
      if (refused) return fail(refused);
      if (this._web.get(id) !== w) return;
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
          // Never the visitor's cookies, HTTP authentication or referrer: the guest's requests
          // are its own, not the page's
          credentials: "omit",
          referrerPolicy: "no-referrer",
          // Where fetch() hands back redirects (not in a browser, which hides them), they go to
          // the guest, whose next request is checked as this one was
          redirect: SERVER_SIDE ? "manual" : "follow",
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

    /**
     * A guest TCP connection carried over a WebSocket to `url`, as binary messages. The guest's
     * SYN waits until the WebSocket opens, so an unreachable tunnel is refused like a port.
     */
    _tunnel(conn, url) {
      conn.hold();
      // A function gives each connection its own URL (a fresh access token, say)
      Promise.resolve(typeof url === "function" ? url({ host: conn.host, port: conn.port }) : url).then(
        (u) => this._tunnel_open(conn, u),
        () => { conn.refuse(); this._poke(); },
      );
    }

    _tunnel_open(conn, url) {
      const o = this._web_options;
      let target = String(url);
      if (o.upgrade_https && target.startsWith("ws:")) target = "wss:" + target.slice(3);
      let socket;
      try {
        socket = new o.WebSocket(target);
      } catch {
        conn.refuse();
        return this._poke();
      }
      socket.binaryType = "arraybuffer";
      let opened = false;
      socket.onopen = () => {
        opened = true;
        conn.accept();
        this._poke();
      };
      socket.onmessage = ({ data }) => {
        conn.write(typeof data === "string" ? utf8.encode(data) : data);
        this._poke();
      };
      socket.onclose = () => {
        if (opened) conn.close();
        else conn.refuse();
        this._poke();
      };
      // A WebSocket can't half-close: the guest finishing its side closes it
      conn.consume_later = true;
      let unsent = 0; // bytes sent to the WebSocket, not yet said consumed
      let waiting = false; // a settle() is scheduled
      const settle = () => {
        waiting = false;
        // What has left the WebSocket's buffer is consumed; the guest's window opens again
        const left = socket.readyState === 1 ? socket.bufferedAmount : 0;
        const done = unsent - Math.min(unsent, left);
        if (done > 0) {
          unsent -= done;
          if (conn.state !== "closed") this._call("armless_tcp_consumed", conn.id, done);
          this._poke();
        }
        if (unsent > 0 && socket.readyState === 1 && !waiting) {
          waiting = true;
          setTimeout(settle, 5);
        }
      };
      conn.on("data", (bytes) => {
        if (socket.readyState === 1) socket.send(bytes);
        unsent += bytes.length;
        settle();
      });
      conn.on("end", () => socket.readyState <= 1 && socket.close(1000));
      conn.on("close", () => socket.readyState <= 1 && socket.close(1000));
    }

    /** One of the guest's WebSockets, opened with the page's WebSocket. */
    _websocket(id, url, protocols) {
      const o = this._web_options;
      let target = url;
      if (o.upgrade_https && target.startsWith("ws:")) target = "wss:" + target.slice(3);
      const w = { socket: null };
      this._web.set(id, w);
      egress_refusal(o, target, "websocket").then((refused) => {
        if (this._web.get(id) !== w) return; // the guest gave up meanwhile
        if (refused) {
          this._web.delete(id);
          this._str_call("armless_http_fail", id, refused);
          this._drain();
          this._poke();
          return;
        }
        this._websocket_open(id, w, target, protocols);
      });
    }

    _websocket_open(id, w, target, protocols) {
      const o = this._web_options;
      let socket;
      try {
        socket = new o.WebSocket(target, protocols);
      } catch (err) {
        this._web.delete(id);
        this._str_call("armless_http_fail", id, `the browser could not open ${target}: ${err}`);
        this._drain();
        this._poke();
        return;
      }
      socket.binaryType = "arraybuffer";
      w.socket = socket;
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
      // A blob's name is a file name (a hash, an extension): anything else, which a snapshot
      // from elsewhere could hold, would resolve to another path or another site
      if (!/^[A-Za-z0-9_~-][A-Za-z0-9._~-]*$/.test(name) || name.includes("..")) {
        console.warn(`armless: refused a blob named ${JSON.stringify(name)}`);
        const n = utf8.encode(name);
        this._with_bytes(n, (p, len) => this._call("armless_blob_failed", p, len));
        this._drain();
        this._poke();
        return;
      }
      if (this._blobs_in_flight.has(name)) return;
      this._blobs_in_flight.add(name);
      this.emit("blob", name);
      const fsopt = this.options.filesystem || {};
      try {
        const base = fsopt.baseurl || "";
        const url = IS_NODE && !is_url(base) ? base + name :
          String(new URL(name, typeof location !== "undefined" ? new URL(base, location.href) : base));
        // A burst of first reads can be throttled or dropped by the server: try again, waiting longer
        // each time, before the guest's read is failed
        let data;
        for (let attempt = 0; ; attempt++) {
          try {
            data = fsopt.fetch_blob ? await fsopt.fetch_blob(name) : await load(url);
            break;
          } catch (err) {
            if (attempt >= BLOB_RETRIES.length) throw err;
            await new Promise((resolve) => setTimeout(resolve, BLOB_RETRIES[attempt]));
          }
        }
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
  // What the guest may reach, for hosts that build their own `allow` on it
  Armless.private_host = private_host;
  Armless._web_options = web_options; // for tests

  root.Armless = Armless;
  if (typeof module === "object" && module.exports) module.exports = { Armless };
})(typeof globalThis !== "undefined" ? globalThis : this);
