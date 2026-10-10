// Several vCPUs for the arm64 machine (V86 option cpu_count > 1).
//
// armless-smp.wasm is the threaded build of the core: its memory is a shared
// WebAssembly.Memory. Each vCPU runs in its own worker, in its own instance of
// the module on that memory (own stack, TLS and JIT function table), calling
// armless_run in a loop. The devices stay on the main thread with v86's JS
// models; the main thread also has an instance, for booting, interrupts
// (armless_set_irq updates the shared GIC and wakes the targeted CPUs) and
// state snapshots.
//
// Workers reach the devices through a per-worker MMIO mailbox: they post a
// request and sleep (Atomics.wait) until the main thread answered it. An idle
// vCPU sleeps on its CPU's wake counter, which the core notifies.
//
// A shared control block pauses all vCPUs (save_state, reset, stop): while
// waiting for them to acknowledge, the main thread keeps serving mailboxes,
// so a vCPU blocked on a device access cannot deadlock it.

/* global self, setImmediate, MessageChannel */

// Control block (Int32Array over a SharedArrayBuffer)
export const CTRL_PAUSE = 0;     // 1: vCPUs must park
export const CTRL_PARKED = 1;    // number of parked vCPUs
export const CTRL_QUIT = 2;      // 1: vCPU workers exit
export const CTRL_EXITED = 3;    // number of vCPU workers that left their loop
export const CTRL_WORDS = 4;

// MMIO mailbox (Int32Array over a SharedArrayBuffer, one per vCPU): a
// synchronous request slot, and a ring of posted writes. Device memory is
// Device-nGnRE on arm64 Linux (early write acknowledgement), so writes may
// complete before the device saw them; reads, and PCI config writes (which
// Linux maps nGnRnE), wait. The main thread drains a vCPU's ring before it
// serves that vCPU's synchronous request, keeping each CPU's order.
export const MBOX_STATE = 0;     // 0 idle, 1 request, 2 answered
export const MBOX_OP = 1;        // 0 read, 1 write
export const MBOX_ADDR = 2;
export const MBOX_SIZE = 3;
export const MBOX_VALUE = 4;
export const MBOX_HEAD = 5;      // posted writes queued (written by the vCPU)
export const MBOX_TAIL = 6;      // posted writes done (written by the main thread)
export const MBOX_SIGNAL = 7;    // 1: the main thread has been told to look
export const MBOX_RING = 16;     // ring of (addr, size, value)
export const MBOX_RING_ENTRIES = 256;
export const MBOX_WORDS = MBOX_RING + 3 * MBOX_RING_ENTRIES;

/**
 * The vCPU worker. Stringified and evaluated in a worker, so it must not
 * reference anything outside itself; message fields use quoted names.
 * @param {*} node_port worker_threads.parentPort in Node, null in a browser
 */
function vcpu_worker(node_port)
{
    const RUN_BUDGET = 25000;
    const post = (msg) => node_port ? node_port.postMessage(msg) : self.postMessage(msg);
    const listen = (fn) => node_port ? node_port.on("message", fn) : (self.onmessage = e => fn(e.data));
    const yield_now = typeof setImmediate === "function" ?
        () => new Promise(resolve => setImmediate(resolve)) :
        () => new Promise(resolve => {
            const ch = new MessageChannel();
            ch.port1.onmessage = () => resolve();
            ch.port2.postMessage(0);
        });

    listen(async function(msg)
    {
        if(!msg || !msg["module"]) return;
        const memory = msg["memory"];
        const ctrl = new Int32Array(msg["ctrl"]);
        const mbox = new Int32Array(msg["mbox"]);
        const index = msg["index"];
        // The main thread's microtick (performance.now() there)
        const time_offset = performance.timeOrigin - msg["time_origin"];
        const now = () => performance.now() + time_offset;

        const ecam_base = msg["ecam_base"] >>> 0, ecam_end = ecam_base + msg["ecam_size"];
        const RING = 16, ENTRIES = 256;
        const signal = () =>
        {
            if(Atomics.exchange(mbox, 7, 1) === 0) post({ "t": "mmio" });
        };
        const posted_write = (addr, size, value) =>
        {
            const head = mbox[5];
            if(head - Atomics.load(mbox, 6) >= ENTRIES) return false;   // full
            const at = RING + 3 * (head % ENTRIES);
            mbox[at] = addr;
            mbox[at + 1] = size;
            mbox[at + 2] = value;
            Atomics.store(mbox, 5, head + 1);
            signal();
            return true;
        };
        const mmio = (op, addr, size, value) =>
        {
            if(op === 1 && !((addr >>> 0) >= ecam_base && (addr >>> 0) < ecam_end) && posted_write(addr, size, value))
            {
                return 0;
            }
            mbox[1] = op;
            mbox[2] = addr;
            mbox[3] = size;
            mbox[4] = value;
            Atomics.store(mbox, 0, 1);
            signal();
            while(Atomics.load(mbox, 0) !== 2)
            {
                Atomics.wait(mbox, 0, 1, 50);
            }
            const result = mbox[4];
            Atomics.store(mbox, 0, 0);
            return result;
        };

        const table = new WebAssembly.Table({ "element": "anyfunc", "initial": msg["table_size"] });
        let exports = null;
        const jit_imports = { "m": memory };
        const env = {
            "memory": memory,
            "__indirect_function_table": table,
            "mmio_read": (addr, size) => mmio(0, addr, size, 0),
            "mmio_write": (addr, size, value) => { mmio(1, addr, size, value); },
            "microtick": now,
            "wall_clock_ms": () => Date.now(),
            "jit_compile": (table_index, ptr, len) =>
            {
                const code = new Uint8Array(memory.buffer, ptr >>> 0, len >>> 0).slice();
                WebAssembly.instantiate(code, { "e": jit_imports }).then(result => {
                    table.set(table_index, result.instance.exports["f"]);
                    exports["armless_jit_ready"](table_index);
                }, err => {
                    post({ "t": "log", "msg": "armless: JIT module failed to compile on CPU " + index + ": " + err });
                });
                return 1;
            },
        };
        const instance = new WebAssembly.Instance(msg["module"], { "env": env });
        exports = instance.exports;
        // Own stack and thread-local storage, then pick our CPU.
        exports["__stack_pointer"].value = msg["stack_top"];
        exports["__wasm_init_tls"](msg["tls"]);
        exports["armless_smp_attach"](index);
        jit_imports["armless_jit_read"] = exports["armless_jit_read"];
        jit_imports["armless_jit_write"] = exports["armless_jit_write"];
        jit_imports["armless_jit_interp"] = exports["armless_jit_interp"];
        exports["armless_jit_config"](msg["jit_enabled"], msg["jit_threshold"]);

        const mem32 = new Int32Array(memory.buffer);
        const wake_index = exports["armless_smp_wake_addr"](index) >>> 2;
        post({ "t": "ready" });

        let last_yield = now();
        for(;;)
        {
            if(Atomics.load(ctrl, 2)) break;
            if(Atomics.load(ctrl, 0))
            {
                Atomics.add(ctrl, 1, 1);
                Atomics.notify(ctrl, 1);
                while(Atomics.load(ctrl, 0) && !Atomics.load(ctrl, 2))
                {
                    Atomics.wait(ctrl, 0, 1, 50);
                }
                Atomics.sub(ctrl, 1, 1);
                continue;
            }
            const woken = Atomics.load(mem32, wake_index);
            const result = exports["armless_run"](RUN_BUDGET);
            if(result === 0 || result === 1)
            {
                if(result === 1)
                {
                    const wake = exports["armless_wake_ms"]();
                    const timeout = wake < 0 ? 20 : Math.min(20, Math.max(0, wake - now()));
                    if(timeout > 0)
                    {
                        Atomics.wait(mem32, wake_index, woken, timeout);
                    }
                }
                // Let JIT compilations complete now and then.
                if(now() - last_yield > 4)
                {
                    await yield_now();
                    last_yield = now();
                }
                continue;
            }
            post({ "t": "stop", "result": result, "pc": exports["armless_pc"]() });
            if(result !== 3) break;
            // Reset: the main thread pauses everyone, reboots, then resumes.
            while(!Atomics.load(ctrl, 0) && !Atomics.load(ctrl, 2))
            {
                Atomics.wait(ctrl, 0, 0, 50);
            }
        }
        Atomics.add(ctrl, 3, 1);
        Atomics.notify(ctrl, 3);
        post({ "t": "exit" });
    });
}

/** Source text for a vCPU worker (browser Worker from a Blob, or Node eval). */
export function vcpu_worker_source()
{
    return "var node_port = typeof self === 'undefined' ? require('worker_threads').parentPort : null;\n" +
        "(" + vcpu_worker.toString() + ")(node_port);\n";
}

/**
 * Start a vCPU worker.
 * @param {Function|undefined} NodeWorker worker_threads.Worker when running in Node
 * @param {function(Object)} on_message
 * @return {{post: function(Object), terminate: function()}}
 */
export function spawn_vcpu_worker(NodeWorker, on_message)
{
    const source = vcpu_worker_source();
    if(NodeWorker)
    {
        const w = new NodeWorker(source, { "eval": true });
        w.on("message", on_message);
        w.on("error", e => on_message({ "t": "error", "msg": String(e && e.stack || e) }));
        return {
            post: msg => w.postMessage(msg),
            terminate: () => w.terminate(),
        };
    }
    const url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
    const w = new Worker(url);
    w.onmessage = e => on_message(e.data);
    w.onerror = e => on_message({ "t": "error", "msg": String(e && e.message || e) });
    return {
        post: msg => w.postMessage(msg),
        terminate: () => w.terminate(),
    };
}
