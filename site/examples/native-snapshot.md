---
title: Saved natively, resumed in a tab
description: A machine booted and saved by armless-run on a native host resumes in the browser from the same snapshot, mid-session, its shell still waiting.
script: native.js
pane: /bin/sh, restored
caption: The snapshot was made when this site was built. A native armless-run booted Linux 6.18, wrote where and when to /root/origin, and saved the machine at the shell prompt. This tab restores it on the one CPU it was saved with.
---

## What is running

No kernel boots on this page. When the site was built, a native `armless-run`
(Rust, Cranelift JIT) booted the same Linux 6.18 kernel and busybox initramfs as the
[trench run](/examples/trench-run). It typed one line into the guest's console, which wrote where and
when the guest was running to `/root/origin`, and then saved the whole machine: CPU registers,
devices, and 256 MiB of RAM, most of it zero. Compressed with zstd, the snapshot is a few MB.

This page fetches that file and hands it to armless's WebAssembly build as `initial_state`.
The machine continues from the instruction it was saved at. The shell was waiting for input at
its prompt, and it still is: the page types `cat /root/origin`, and the line written natively
comes back. Nothing in the guest knows it moved.

## The code

On the native side, `armless-run` boots the guest and saves it once the guest prints a marker:

```sh
armless-run --kernel Image --initrd initrd.cpio --append "console=hvc0 rdinit=/init" \
  -m 256 --smp 1 \
  --console-after "# " --console-input "echo ... > /root/origin; echo SA''VED\n" \
  --save native.bin --save-after SAVED
zstd -19 native.bin
```

In the page, the snapshot replaces `boot`. The machine has to be made as the saved one was: the
same memory, CPUs and devices.

```js
const vm = new Armless({
  wasm: "armless.wasm",
  memory_mb: 256,
  cpus: 1,
  initial_state: "native.bin.zst",
});
vm.on("console", (bytes) => term.write(bytes));
term.onData((text) => vm.console_input(text));
```

How it is built:
[site/demo.exs](https://github.com/filipecabaco/armless/blob/main/site/demo.exs) and
[site/static/native.js](https://github.com/filipecabaco/armless/blob/main/site/static/native.js).
