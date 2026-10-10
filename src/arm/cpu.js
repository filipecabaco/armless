// The AArch64 CPU for v86's JavaScript runtime.
//
// ArmCPU drives armless.wasm (see arm/src/wasm.rs) and presents the same
// surface the existing v86 devices, state.js and starter.js use on the x86
// CPU: mem8/read*/write*, io, devices, device_raise_irq/lower_irq,
// main_loop, get_state/set_state, and the zstd exports. The device models
// themselves (uart.js, pci.js, virtio*.js, 9p) are used unchanged:
//
//   * guest RAM starts at physical address 0, so DMA addresses are the same;
//   * the 16550 UART window maps onto ports 0x3F8.. (serial0);
//   * the PCI I/O window maps onto x86 port numbers (virtio I/O BARs);
//   * PCI ECAM config cycles are replayed through ports 0xCF8/0xCFC.

import { LOG_CPU, MMAP_BLOCK_BITS } from "../const.js";
import { h, Bitmap } from "../lib.js";
import { dbg_assert, dbg_log } from "../log.js";

import { IO } from "../io.js";
import { PCI } from "../pci.js";
import { UART } from "../uart.js";
import { VirtioConsole } from "../virtio_console.js";
import { VirtioNet } from "../virtio_net.js";
import { VirtioBalloon } from "../virtio_balloon.js";
import { Virtio9p, Virtio9pHandler, Virtio9pProxy } from "../../lib/9p.js";

import * as board from "./board.js";

// For types only
import { CPU } from "../cpu.js";

// Instructions per armless_run call, and how long main_loop may run before
// yielding to the event loop (milliseconds), like v86's TIME_PER_FRAME.
const RUN_BUDGET = 25000;
const TIME_PER_FRAME = 1;

const STATE_MARKER = "armless-arm64-v1";

/**
 * @constructor
 * @param {Object} wm  { exports } of armless.wasm
 * @param {function()} stop_idling
 * @param {function(): number} microtick
 */
export function ArmCPU(bus, wm, stop_idling, microtick)
{
    this.arch = "arm64";
    this.bus = bus;
    this.wm = wm;
    this.exports = wm.exports;
    this.stop_idling = stop_idling;
    this.microtick = microtick;

    this.wasm_memory = this.exports["memory"];
    this.memory_size = new Uint32Array(1);
    /** @type {!Uint8Array} guest RAM, physical address 0 */
    this.mem8 = new Uint8Array(0);

    /** @type {!Object} */
    this.devices = {};
    /** @type {IO} */
    this.io = null;

    // used by io.js
    /** @const */ this.memory_map_read8 = [];
    /** @const */ this.memory_map_write8 = [];
    /** @const */ this.memory_map_read32 = [];
    /** @const */ this.memory_map_write32 = [];

    // GIC SPI -> set of PCI ids currently asserting it (INTx lines are shared)
    this.pci_irq_sources = new Map();

    this.halted = false;
    this.boot = null;

    // JIT: generated block modules import the memory and these helpers
    this.wasm_table = wm.wasm_table;
    this.jit_imports = {
        "m": this.exports["memory"],
        "armless_jit_read": this.exports["armless_jit_read"],
        "armless_jit_write": this.exports["armless_jit_write"],
        "armless_jit_interp": this.exports["armless_jit_interp"],
    };
    /** @type {!Uint8Array} non-zero for RAM pages holding translated code */
    this.jit_code_pages = new Uint8Array(0);

    // zstd decoder exported by armless.wasm, same names as v86.wasm
    for(const name of ["zstd_create_ctx", "zstd_get_src_ptr", "zstd_free_ctx", "zstd_read", "zstd_read_free"])
    {
        this[name] = this.exports[name];
    }
}

/**
 * Re-create the RAM view if wasm memory grew (growing detaches old views).
 * Called after every call into armless.wasm that may allocate, and on entry
 * to the MMIO callbacks, so device code always sees a live mem8.
 */
ArmCPU.prototype.refresh_memory = function()
{
    if(this.mem8.byteLength === 0 && this.memory_size[0])
    {
        this.mem8 = new Uint8Array(this.wasm_memory.buffer, this.exports["armless_mem_ptr"](), this.memory_size[0]);
        this.jit_code_pages = new Uint8Array(this.wasm_memory.buffer, this.exports["armless_jit_code_pages"](),
            Math.ceil(this.memory_size[0] / 4096));
    }
};

/**
 * Host writes to guest RAM (DMA) must drop JIT translations of those pages.
 * @param {number} addr
 * @param {number} length
 */
ArmCPU.prototype.jit_dirty = function(addr, length)
{
    const pages = this.jit_code_pages;
    const last = addr + length - 1 >>> 12;
    for(let page = addr >>> 12; page <= last; page++)
    {
        if(pages[page])
        {
            this.exports["armless_jit_dirty"](addr, length);
            return;
        }
    }
};

/**
 * Called by armless.wasm: compile a JIT block module into a table slot.
 * Compilation is asynchronous; the interpreter keeps running meanwhile.
 * @return {number}
 */
ArmCPU.prototype.jit_compile = function(table_index, ptr, len)
{
    const code = new Uint8Array(this.wasm_memory.buffer, ptr >>> 0, len >>> 0).slice();
    WebAssembly.instantiate(code, { "e": this.jit_imports }).then(result => {
        this.wasm_table.set(table_index, result.instance.exports["f"]);
        this.exports["armless_jit_ready"](table_index);
    }, err => {
        dbg_log("armless: JIT module failed to compile: " + err, LOG_CPU);
    });
    return 1;
};

// ------------------------------------------------------------------ memory
// Each helper first makes sure mem8 is live: device code can run from async
// callbacks after something else (zstd, state restore) grew wasm memory.

ArmCPU.prototype.read8 = function(addr)
{
    this.refresh_memory();
    return this.mem8[addr];
};
ArmCPU.prototype.read16 = function(addr)
{
    this.refresh_memory();
    const m = this.mem8;
    return m[addr] | m[addr + 1] << 8;
};
ArmCPU.prototype.read32s = function(addr)
{
    this.refresh_memory();
    const m = this.mem8;
    return m[addr] | m[addr + 1] << 8 | m[addr + 2] << 16 | m[addr + 3] << 24;
};
ArmCPU.prototype.write8 = function(addr, value)
{
    this.refresh_memory();
    this.mem8[addr] = value;
    this.jit_dirty(addr, 1);
};
ArmCPU.prototype.write16 = function(addr, value)
{
    this.refresh_memory();
    const m = this.mem8;
    m[addr] = value;
    m[addr + 1] = value >> 8;
    this.jit_dirty(addr, 2);
};
ArmCPU.prototype.write32 = function(addr, value)
{
    this.refresh_memory();
    const m = this.mem8;
    m[addr] = value;
    m[addr + 1] = value >> 8;
    m[addr + 2] = value >> 16;
    m[addr + 3] = value >> 24;
    this.jit_dirty(addr, 4);
};
ArmCPU.prototype.write_blob = function(blob, offset)
{
    this.refresh_memory();
    dbg_assert(offset + blob.length <= this.memory_size[0]);
    this.mem8.set(blob, offset);
    this.jit_dirty(offset, blob.length);
};
ArmCPU.prototype.read_blob = function(offset, length)
{
    this.refresh_memory();
    return this.mem8.subarray(offset, offset + length);
};
ArmCPU.prototype.zero_memory = function(offset, length)
{
    this.refresh_memory();
    this.mem8.fill(0, offset, offset + length);
    this.jit_dirty(offset, length);
};
ArmCPU.prototype.reset_memory = function()
{
    this.refresh_memory();
    this.mem8.fill(0);
};

// ------------------------------------------------------- MMIO from the core

ArmCPU.prototype.mmio_read = function(addr, size)
{
    this.refresh_memory();
    addr >>>= 0;
    const io = this.io;
    if(addr >= board.UART_BASE && addr < board.UART_BASE + board.UART_SIZE)
    {
        return io.port_read8(board.UART_PORT + (addr - board.UART_BASE & 7));
    }
    if(addr >= board.PCI_PIO_BASE && addr < board.PCI_PIO_BASE + board.PCI_PIO_SIZE)
    {
        const port = addr - board.PCI_PIO_BASE;
        if(size === 1) return io.port_read8(port);
        if(size === 2) return io.port_read16(port);
        return io.port_read32(port);
    }
    if(addr >= board.PCI_ECAM_BASE && addr < board.PCI_ECAM_BASE + board.PCI_ECAM_SIZE)
    {
        return this.ecam_read(addr - board.PCI_ECAM_BASE, size);
    }
    if(addr >= board.PCI_MMIO_BASE && addr < board.PCI_MMIO_BASE + board.PCI_MMIO_SIZE)
    {
        if(size === 1) return this.mmap_read8(addr);
        if(size === 2) return this.mmap_read8(addr) | this.mmap_read8(addr + 1) << 8;
        return this.mmap_read32(addr);
    }
    return -1;
};

ArmCPU.prototype.mmio_write = function(addr, size, value)
{
    this.refresh_memory();
    addr >>>= 0;
    const io = this.io;
    if(addr >= board.UART_BASE && addr < board.UART_BASE + board.UART_SIZE)
    {
        io.port_write8(board.UART_PORT + (addr - board.UART_BASE & 7), value & 0xFF);
        return;
    }
    if(addr >= board.PCI_PIO_BASE && addr < board.PCI_PIO_BASE + board.PCI_PIO_SIZE)
    {
        const port = addr - board.PCI_PIO_BASE;
        if(size === 1) io.port_write8(port, value & 0xFF);
        else if(size === 2) io.port_write16(port, value & 0xFFFF);
        else io.port_write32(port, value | 0);
        return;
    }
    if(addr >= board.PCI_ECAM_BASE && addr < board.PCI_ECAM_BASE + board.PCI_ECAM_SIZE)
    {
        this.ecam_write(addr - board.PCI_ECAM_BASE, size, value);
        return;
    }
    if(addr >= board.PCI_MMIO_BASE && addr < board.PCI_MMIO_BASE + board.PCI_MMIO_SIZE)
    {
        if(size === 1) this.mmap_write8(addr, value & 0xFF);
        else if(size === 2) { this.mmap_write8(addr, value & 0xFF); this.mmap_write8(addr + 1, value >> 8 & 0xFF); }
        else this.mmap_write32(addr, value | 0);
    }
};

// ECAM: offset = bus << 20 | device << 15 | function << 12 | register.
// v86's pci.js implements configuration mechanism #1 (ports 0xCF8/0xCFC);
// replay each access through it so its BAR/command handling stays authoritative.
ArmCPU.prototype.ecam_address = function(offset)
{
    const bdf = offset >>> 12 & 0xFF;
    return (0x80000000 | bdf << 8 | offset & 0xFC) | 0;
};

ArmCPU.prototype.ecam_read = function(offset, size)
{
    if((offset & 0xFFF) >= 0x100)
    {
        return 0; // no extended configuration space
    }
    const io = this.io;
    io.port_write32(0xCF8, this.ecam_address(offset));
    const port = 0xCFC + (offset & 3);
    if(size === 1) return io.port_read8(port);
    if(size === 2) return io.port_read16(port);
    return io.port_read32(port);
};

ArmCPU.prototype.ecam_write = function(offset, size, value)
{
    if((offset & 0xFFF) >= 0x100)
    {
        return;
    }
    const io = this.io;
    io.port_write32(0xCF8, this.ecam_address(offset));
    const port = 0xCFC + (offset & 3);
    if(size === 1) io.port_write8(port, value & 0xFF);
    else if(size === 2) io.port_write16(port, value & 0xFFFF);
    else io.port_write32(port, value | 0);
};

ArmCPU.prototype.mmap_read8 = function(addr)
{
    const fn = this.memory_map_read8[addr >>> MMAP_BLOCK_BITS];
    return fn ? fn(addr) : 0xFF;
};
ArmCPU.prototype.mmap_read32 = function(addr)
{
    const fn = this.memory_map_read32[addr >>> MMAP_BLOCK_BITS];
    return fn ? fn(addr) : -1;
};
ArmCPU.prototype.mmap_write8 = function(addr, value)
{
    const fn = this.memory_map_write8[addr >>> MMAP_BLOCK_BITS];
    fn && fn(addr, value);
};
ArmCPU.prototype.mmap_write32 = function(addr, value)
{
    const fn = this.memory_map_write32[addr >>> MMAP_BLOCK_BITS];
    fn && fn(addr, value);
};

// ------------------------------------------------------------- interrupts

// Non-PCI devices raise x86 IRQ numbers; map the ones that exist on armless.
const ISA_IRQ_TO_INTID = { 4: board.SPI_UART };

ArmCPU.prototype.device_raise_irq = function(irq)
{
    const intid = ISA_IRQ_TO_INTID[irq];
    if(intid !== undefined)
    {
        this.exports["armless_set_irq"](intid, 1);
        this.stop_idling();
    }
};

ArmCPU.prototype.device_lower_irq = function(irq)
{
    const intid = ISA_IRQ_TO_INTID[irq];
    if(intid !== undefined)
    {
        this.exports["armless_set_irq"](intid, 0);
    }
};

ArmCPU.prototype.pci_intid = function(pci_id)
{
    const space = this.devices.pci.device_spaces[pci_id];
    const pin = (space[0x3C >>> 2] >> 8 & 0xFF) || 1;
    const slot = pci_id >> 3 & 0x1F;
    return this.exports["armless_pci_intx_intid"](slot, pin);
};

ArmCPU.prototype.pci_set_irq = function(pci_id, level)
{
    const intid = this.pci_intid(pci_id);
    let sources = this.pci_irq_sources.get(intid);
    if(!sources)
    {
        sources = new Set();
        this.pci_irq_sources.set(intid, sources);
    }
    if(level) sources.add(pci_id);
    else sources.delete(pci_id);
    this.exports["armless_set_irq"](intid, sources.size ? 1 : 0);
    if(level) this.stop_idling();
};

// --------------------------------------------------------------- set-up

/**
 * The device models are typed against the x86 CPU; ArmCPU provides the same
 * surface they use.
 * @return {CPU}
 */
ArmCPU.prototype.as_cpu = function()
{
    return /** @type {CPU} */ (/** @type {?} */ (this));
};

ArmCPU.prototype.init = function(settings, device_bus)
{
    const cpu = this.as_cpu();
    let size = settings.memory_size || 256 * 1024 * 1024;
    size = Math.ceil(size / 0x200000) * 0x200000; // whole 2 MiB blocks
    dbg_assert(size <= board.RAM_MAX, "armless: memory_size must be below " + h(board.RAM_MAX));
    if(this.exports["armless_init"](size) !== 0)
    {
        throw new Error("armless: cannot create a machine with " + size + " bytes of RAM");
    }
    this.memory_size[0] = size;
    this.mem8 = new Uint8Array(0);
    this.refresh_memory();
    // JIT on unless disabled (disable_jit, as for the x86 machine)
    this.exports["armless_jit_config"](settings.disable_jit ? 0 : 1, settings.jit_threshold || 0);

    this.io = new IO(cpu);

    const pci = this.devices.pci = new PCI(cpu);
    // INTx routing for the ARM board (replaces the x86 PIRQ routing)
    pci.raise_irq = pci_id => this.pci_set_irq(pci_id, true);
    pci.lower_irq = pci_id => this.pci_set_irq(pci_id, false);

    this.devices.uart0 = new UART(cpu, board.UART_PORT, device_bus);

    if(settings.net_device && settings.net_device.type === "virtio")
    {
        this.devices.virtio_net = new VirtioNet(cpu, device_bus, settings.preserve_mac_from_state_image, settings.net_device.mtu);
    }
    if(settings.fs9p)
    {
        this.devices.virtio_9p = new Virtio9p(settings.fs9p, cpu, device_bus);
    }
    else if(settings.handle9p)
    {
        this.devices.virtio_9p = new Virtio9pHandler(settings.handle9p, cpu);
    }
    else if(settings.proxy9p)
    {
        this.devices.virtio_9p = new Virtio9pProxy(settings.proxy9p, cpu);
    }
    if(settings.virtio_console)
    {
        this.devices.virtio_console = new VirtioConsole(cpu, device_bus);
    }
    if(settings.virtio_balloon)
    {
        this.devices.virtio_balloon = new VirtioBalloon(cpu, device_bus);
    }

    if(settings.bzimage)
    {
        this.boot = {
            kernel: new Uint8Array(settings.bzimage),
            initrd: settings.initrd ? new Uint8Array(settings.initrd) : null,
            cmdline: settings.cmdline || "",
        };
        this.load_kernel();
    }
    else
    {
        dbg_log("armless: no kernel given (expected an arm64 Image in bzimage)", LOG_CPU);
    }
};

/** Copy a byte array into a scratch buffer in wasm memory; returns its address. */
ArmCPU.prototype.to_wasm = function(bytes)
{
    const ptr = this.exports["armless_alloc"](bytes.length);
    new Uint8Array(this.wasm_memory.buffer, ptr, bytes.length).set(bytes);
    this.refresh_memory();
    return ptr;
};

ArmCPU.prototype.load_kernel = function()
{
    const { kernel, initrd, cmdline } = this.boot;
    const cmd = new TextEncoder().encode(cmdline);
    const k = this.to_wasm(kernel);
    const r = initrd ? this.to_wasm(initrd) : 0;
    const c = this.to_wasm(cmd);
    const result = this.exports["armless_boot_linux"](k, kernel.length, r, initrd ? initrd.length : 0, c, cmd.length);
    this.exports["armless_free"](k, kernel.length);
    if(initrd) this.exports["armless_free"](r, initrd.length);
    this.exports["armless_free"](c, cmd.length);
    if(result !== 0)
    {
        throw new Error("armless: the kernel is not a valid arm64 Image or does not fit in RAM");
    }
    this.halted = false;
};

ArmCPU.prototype.reboot_internal = function()
{
    for(const name of ["virtio_9p", "virtio_console", "virtio_net"])
    {
        if(this.devices[name]) this.devices[name].reset();
    }
    this.pci_irq_sources.clear();
    this.reset_memory();
    if(this.boot)
    {
        this.load_kernel();
    }
    else
    {
        this.exports["armless_reset"]();
    }
};

// -------------------------------------------------------------- main loop

const RUN_BUDGET_USED = 0, RUN_IDLE = 1, RUN_POWER_OFF = 2, RUN_RESET = 3;

/** Run for up to TIME_PER_FRAME; returns milliseconds until the next call. */
ArmCPU.prototype.main_loop = function()
{
    if(this.halted)
    {
        return 100;
    }
    const start = this.microtick();
    for(;;)
    {
        const result = this.exports["armless_run"](RUN_BUDGET);
        this.refresh_memory();
        if(result === RUN_BUDGET_USED)
        {
            if(this.microtick() - start > TIME_PER_FRAME)
            {
                return 0;
            }
        }
        else if(result === RUN_IDLE)
        {
            const wake = this.exports["armless_wake_ms"]();
            return wake < 0 ? 100 : Math.max(0, wake - this.microtick());
        }
        else if(result === RUN_POWER_OFF)
        {
            this.halted = true;
            this.bus.send("cpu-event-halt");
            return 100;
        }
        else if(result === RUN_RESET)
        {
            this.reboot_internal();
        }
        else
        {
            this.halted = true;
            dbg_log("armless: CPU stopped at pc=" + h(this.exports["armless_pc"]() >>> 0), LOG_CPU);
            this.bus.send("cpu-event-halt");
            return 100;
        }
    }
};

// ------------------------------------------------------------------ state

ArmCPU.prototype.pack_memory = function()
{
    this.refresh_memory();
    const mem = this.mem8;
    const page_count = mem.length >> 12;
    const bitmap = new Bitmap(page_count);
    const words = new Int32Array(mem.buffer, mem.byteOffset, mem.length >> 2);
    const pages = [];
    for(let page = 0; page < page_count; page++)
    {
        const base = page << 10;
        for(let i = 0; i < 1024; i++)
        {
            if(words[base + i] !== 0)
            {
                pages.push(page);
                bitmap.set(page, 1);
                break;
            }
        }
    }
    const packed_memory = new Uint8Array(pages.length << 12);
    pages.forEach((page, i) => packed_memory.set(mem.subarray(page << 12, page + 1 << 12), i << 12));
    return { bitmap, packed_memory };
};

ArmCPU.prototype.unpack_memory = function(bitmap, packed_memory)
{
    this.refresh_memory();
    const mem = this.mem8;
    mem.fill(0);
    const page_count = this.memory_size[0] >> 12;
    let packed_page = 0;
    for(let page = 0; page < page_count; page++)
    {
        if(bitmap.get(page))
        {
            mem.set(packed_memory.subarray(packed_page << 12, packed_page + 1 << 12), page << 12);
            packed_page++;
        }
    }
};

ArmCPU.prototype.get_state = function()
{
    const state = [];
    state[0] = STATE_MARKER;
    state[1] = this.memory_size[0];

    const len = this.exports["armless_save_state"]();
    const ptr = this.exports["armless_state_ptr"]();
    state[2] = new Uint8Array(this.wasm_memory.buffer, ptr, len).slice();
    this.refresh_memory();

    const { packed_memory, bitmap } = this.pack_memory();
    state[3] = packed_memory;
    state[4] = new Uint8Array(bitmap.get_buffer());

    state[5] = this.devices.pci;
    state[6] = this.devices.uart0;
    state[7] = this.devices.virtio_console;
    state[8] = this.devices.virtio_net;
    state[9] = this.devices.virtio_9p;
    state[10] = this.devices.virtio_balloon;
    state[11] = Array.from(this.pci_irq_sources, ([intid, ids]) => [intid, Array.from(ids)]);
    return state;
};

ArmCPU.prototype.set_state = function(state)
{
    if(state[0] !== STATE_MARKER)
    {
        throw new Error("armless: this state image was not saved by an arm64 machine");
    }
    if(state[1] !== this.memory_size[0])
    {
        throw new Error("armless: state image needs memory_size " + state[1] + ", machine has " + this.memory_size[0]);
    }

    this.unpack_memory(new Bitmap(state[4].buffer), state[3]);

    const blob = state[2];
    const ptr = this.to_wasm(blob);
    const ok = this.exports["armless_load_state"](ptr, blob.length);
    this.exports["armless_free"](ptr, blob.length);
    if(ok !== 0)
    {
        throw new Error("armless: corrupt CPU state");
    }

    this.devices.pci && this.devices.pci.set_state(state[5]);
    this.devices.uart0 && state[6] && this.devices.uart0.set_state(state[6]);
    this.devices.virtio_console && state[7] && this.devices.virtio_console.set_state(state[7]);
    this.devices.virtio_net && state[8] && this.devices.virtio_net.set_state(state[8]);
    this.devices.virtio_9p && state[9] && this.devices.virtio_9p.set_state(state[9]);
    this.devices.virtio_balloon && state[10] && this.devices.virtio_balloon.set_state(state[10]);

    this.pci_irq_sources = new Map((state[11] || []).map(([intid, ids]) => [intid, new Set(ids)]));
    this.halted = false;
};

// Present for API compatibility with the x86 CPU (used by starter.js).
ArmCPU.prototype.run_hardware_timers = function() { return 100; };
ArmCPU.prototype.dump_state = function() {};
ArmCPU.prototype.dump_regs_short = function() {};
ArmCPU.prototype.get_regs_short = function() { return [[], []]; };
ArmCPU.prototype.debug_get_state = function() { return "pc=" + h(this.exports["armless_pc"]() >>> 0); };
