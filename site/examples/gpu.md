---
title: The guest computes on your GPU
description: A program inside the machine runs a WebGPU compute shader on this page's GPU, through armless-gpu, and races the same work on its own emulated CPU.
script: gpu.js
pane: /bin/sh, with the GPU
caption: Linux 6.18 with a network device for the GPU's connection alone. gpu asks gpu.internal for a GPU, and this page answers with its WebGPU.
---

## What is running

`gpu` is a small C++ program in the machine's initramfs. It asks for a GPU the way a WebGPU program
does, through `webgpu_cpp.h`, but armless's version of that header (`gpu/guest`) has no driver
under it: it speaks a small protocol to `gpu.internal:7070`. The page answers that connection with
`armless-gpu.js`, which makes the WebGPU calls on this browser's GPU. The program prints which GPU
it reached, runs a Mandelbrot set as a WGSL compute shader there, runs the same loop on the
machine's own CPU (emulated in WebAssembly), and draws the GPU's result with both times.

The machine has a network device only for that connection: its `allow()` lets nothing else out.
A browser without WebGPU refuses the connection, and the program says so.

The same header is what llama.cpp's WebGPU backend is built against, unchanged: a model's layers
run on the visitor's GPU this way. [snowglobe's Ollama example](https://snowglobe.build) runs the
official Ollama release on it.

## The code

```js
const vm = new Armless({
  wasm: "armless.wasm",
  boot: { kernel: "Image.gz", initrd: "initrd.cpio" },
  // a network device for the GPU's connection; allow() lets nothing else out
  network: { allow: () => false },
});
// gpu.internal:7070 is answered with this page's WebGPU (armless-gpu.js)
ArmlessGPU.attach(vm, { max_memory: 256 * 1048576 });
vm.run();
```

In the guest, a WebGPU program built against `gpu/guest/include`:

```cpp
#include <webgpu/webgpu_cpp.h>

wgpu::Instance instance = wgpu::CreateInstance();  // connects to gpu.internal:7070
instance.RequestAdapter(nullptr, wgpu::CallbackMode::WaitAnyOnly, on_adapter);
// ... a shader module, a compute pipeline, a dispatch, a buffer mapped back
```

`site/guest/gpu.cpp` is the whole program, and [docs/gpu.md](/docs/gpu) the protocol and what
it covers.
