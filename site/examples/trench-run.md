---
title: A trench run on every CPU
description: arm64 Linux boots in this tab on as many CPUs as the device can spare, and one awk per CPU draws a trench run in its console.
script: trench.js
pane: /bin/sh
caption: Linux 6.18, booted in this tab from an initramfs with busybox. One awk per CPU draws a trench run, a new one every time. Ctrl+c stops it. Escape, then Tab, leaves the console.
---

## What is running

Nothing on this page is recorded. The page fetches armless's wasm, a Linux 6.18 kernel and an
initramfs holding a static busybox, and boots them. Once the shell prompts, the page types
`trench`, a shell script in the initramfs. Each frame, the script starts one `awk` per CPU, and
each awk draws every Nth row of the picture. The rows fill in from all the CPUs at once.

Each CPU is a Web Worker, and every worker runs the same machine in one shared `WebAssembly.Memory`.
The figures beside the console are armless's own counters, read once a second from
`vm.memory_report()`: instructions per second for each CPU, the pages its JIT has turned into
WebAssembly, and the size of the linear memory.

Ctrl+c gives you the shell. Try `nproc` or `cat /proc/cpuinfo`.

## The code

The machine on this page is the README's browser example with a kernel and an initramfs instead
of a filesystem:

```js
const vm = new Armless({
  wasm: "armless.wasm",
  wasm_smp: "armless-smp.wasm",  // one Web Worker per CPU, over shared memory
  memory_mb: 256,
  cpus: navigator.hardwareConcurrency - 1,
  boot: {
    kernel: "Image.gz",
    initrd: "initrd.cpio",
    cmdline: "console=hvc0 quiet rdinit=/init",
  },
});
vm.on("console", (bytes) => term.write(bytes));
term.onData((text) => vm.console_input(text));
```

More than one CPU needs a cross-origin isolated page. GitHub Pages can't send those headers, so
this site's service worker adds them. The page's source is
[site/static/machine.js](https://github.com/filipecabaco/armless/blob/main/site/static/machine.js),
and the guest is [site/guest](https://github.com/filipecabaco/armless/tree/main/site/guest).
