---
title: The guest renders 3D on your GPU
description: A program inside the machine raymarches a 3D scene in real time with a WebGPU compute shader on this page's GPU. Press c to render the same scene on its emulated CPU instead.
script: gpu.js
pane: /bin/sh, with the GPU
caption: Linux 6.18 with a network device for the GPU's connection alone. gpu asks gpu.internal for a GPU, and this page answers with its WebGPU. c switches renderers, q stops.
---

## What is running

`gpu` is a small C++ program in the machine's initramfs. It asks for a GPU the way a WebGPU program
does, through `webgpu_cpp.h`, but armless's version of that header (`gpu/guest`) has no driver
under it: it speaks a small protocol to `gpu.internal:7070`. The page answers that connection with
`armless-gpu.js`, which makes the WebGPU calls on this browser's GPU.

Each frame is a raymarched scene, a torus turning over a checkered floor and a ball bouncing round
it, with soft shadows, ambient occlusion and fog: one WGSL compute shader, dispatched on the GPU,
its pixels mapped back and drawn in the console two to a character. The header says which GPU
answered, the frames per second and how long each frame took. Press `c` and the same scene, the
same code line for line in C++, renders on the machine's own CPU, emulated in WebAssembly: the
difference is the GPU. The console itself caps the frame rate, at what it can draw.

The machine has a network device only for that connection: its `allow()` lets nothing else out.
A browser without WebGPU refuses the connection, and the program says so.

The same header is what llama.cpp's WebGPU backend is built against, unchanged: a model's layers
run on the visitor's GPU this way. [snowglobe's Ollama demo](https://snowglobe.build/ollama/) runs the
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
