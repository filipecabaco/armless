---
title: The guest renders 3D on your GPU
description: A program in the machine renders a 3D scene on this page's GPU through WebGPU. Press c to render it on the emulated CPU instead.
script: gpu.js
pane: /bin/sh, with the GPU
caption: Linux 6.18. gpu connects to gpu.internal, and this page answers with WebGPU. c switches to the CPU, q stops.
---

## What is running

`gpu` is a small C++ program in the initramfs, written against `webgpu_cpp.h`. armless's version of
that header sends each call to `gpu.internal:7070`, and `armless-gpu.js` runs it on this browser's
GPU.

The scene is a raymarched torus and ball over a checkered floor, rendered by one compute shader.
Press `c` to run the same code on the emulated CPU and compare the frame times.

The machine's network reaches nothing but the GPU. Without WebGPU, the program says so and exits.

llama.cpp's WebGPU backend builds against the same header:
[snowglobe's Ollama demo](https://snowglobe.build/ollama/) runs the official Ollama release this way.

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
