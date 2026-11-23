# GPU compute (experimental)

A guest program can run WebGPU compute shaders on the host's GPU. It opens a connection to
`gpu.internal:7070` and speaks a small protocol (`gpu/src/wire.rs`): buffers, WGSL shaders,
compute pipelines, bind groups, dispatches, copies and reads. The host runs them on a real GPU.
Shaders travel as WGSL text, so the guest compiles nothing and the host passes them on as they
are. Only compute is covered: what inference needs. There is no graphics device and no Linux
driver; the guest needs no kernel changes.

The guest's CPU is still emulated; only the shaders run on the GPU. Everything a program sends
goes through the guest's network stack, so uploading large buffers (model weights) is the slow
part.

## Natively

`wgpu` runs the commands, on Vulkan, Metal, DX12 or GL (`gpu/src/server.rs`). Each guest
connection gets its own device on the host's preferred adapter.

In process, with armless-exec (or armless-run, for a kernel) built with the `gpu` feature:

```sh
cargo build --release -p armless-user --features gpu
build/release/armless-exec --gpu PROGRAM
cargo build --release --bin armless-run --features gpu
build/release/armless-run --gpu --kernel Image --fs FS …
```

`--gpu` gives the guest a network device for the GPU's link alone: its connection to
`gpu.internal:7070` is answered, and every other connection, request and WebSocket is refused
(`Egress::deny_all`). With `--net` too it has the network as well.

Or as a separate server, reached through a tunnel (any host that can tunnel, armless-run included):

```sh
cargo build --release -p armless-gpu
build/release/armless-gpu-server 127.0.0.1:7070 &
build/release/armless-exec --net --allow-private --tunnel gpu.internal:7070=tcp://127.0.0.1:7070 PROGRAM
```

`gpu/examples/vecadd.rs` is a guest program that adds two vectors of a million floats:

```sh
cargo build --release -p armless-gpu --example vecadd --no-default-features \
    --target aarch64-unknown-linux-musl   # CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
build/release/armless-exec --net --gpu build/aarch64-unknown-linux-musl/release/examples/vecadd
```

```text
armless-exec: gpu.internal:7070 is llvmpipe (LLVM 20.1.2, 256 bits) (Vulkan)
gpu: llvmpipe (LLVM 20.1.2, 256 bits) (Vulkan), max buffer 2047 MiB
vector add of 1048576 floats on the host GPU: correct in 403 ms
```

That run was on Mesa's software Vulkan (lavapipe), on a machine with no GPU. The same program run
natively, not in armless, took 61 ms: the difference is the 12 MiB through the guest's network.

## In the browser

`js/armless-gpu.js` answers the guest's connections to `gpu.internal:7070` in the page and runs
the commands on `navigator.gpu`, one device per connection. armless.js is unchanged: the page
takes the connections as it takes any other (`tcp-connection`).

```html
<script src="armless.js"></script>
<script src="armless-gpu.js"></script>
<script>
  const vm = new Armless({ ..., network: true });
  ArmlessGPU.attach(vm); // or attach(vm, { gpu, host, port, adapter, log })
</script>
```

The guest needs a network device, which `network: { allow: () => false }` gives it without
letting anything out: only the connections the page answers itself (the GPU's) get anywhere.

The page needs WebGPU: a secure context (https:// or localhost) in a browser that has it.
Without it the guest's connection is refused. `attach` returns a function that stops answering
new connections.

WebGPU reports validation errors asynchronously, so each command that has no reply runs in an
error scope, and a reply waits for the scopes before it. A guest sees the same errors at the same
points as natively; only the messages' wording differs.

The protocol's checks are `tests/js/gpu-checks.mjs`. `node tests/js/gpu-browser.mjs` runs them in
headless Chromium (Playwright) against the page's WebGPU, then the vecadd guest in user mode on
armless.wasm; `tests/js/gpu.test.mjs` runs them in Node on Dawn (the `webgpu` package), which CI
does on lavapipe (`ARMLESS_GPU_TEST_REQUIRE=1` there makes a missing adapter a failure, here and
in the Rust tests). With no GPU, Chromium's SwiftShader stands in:

```text
ok   hello says what the adapter is
ok   a dispatch then a copy then a write keep their order
ok   a bad shader is reported at the next reply and the connection goes on
ok   an unknown buffer is an error, not a crash
ok   an arm64 guest program on armless.wasm adds vectors on the page's GPU
       gpu: google swiftshader (WebGPU), max buffer 1024 MiB
       vector add of 1048576 floats on the host GPU: correct in 2319 ms
```

## llama.cpp, and Ollama's models

`gpu/guest/include/webgpu/webgpu_cpp.h` is as much of Dawn's C++ WebGPU API as llama.cpp's
WebGPU backend (ggml-webgpu) uses, over this protocol, in one header. llama.cpp built against it
offloads a model's layers to the host's GPU and runs unchanged otherwise:

```sh
gpu/guest/build-llama.sh LLAMA_CPP_SRC   # static aarch64 musl, Zig as the cross compiler
build/release/armless-exec --net --gpu --fs FS /bin/llama-completion -m /models/model.gguf -ngl 99 -t 1 -p "…"
```

`gpu/guest/cmake/DawnConfig.cmake` stands in for Dawn, so the backend's `find_package(Dawn)` finds
the header. llama.cpp's source can be a checkout or the copy in llama-cpp-python's source package
on PyPI (`vendor/llama.cpp`).

How the header behaves like WebGPU without a GPU in the guest:

- Command encoders record in the guest and `Queue::Submit` sends them; queue writes go at once.
  The host runs everything in the order it arrives, which is WebGPU's queue order.
- Futures (buffer maps, work done) send their command at once and read the reply only when the
  program waits, so the guest's CPU and the host's GPU work at the same time.
- A buffer mapped for reading is read through the host (its map usage is a copy there).
- No GPU to reach (the connection refused, or taken and closed) is no adapter: llama.cpp says it
  found no usable GPU and uses the CPU. The backend needs `shader-f16`; a GPU without it is no
  device to llama.cpp either.

smollm2:135m from Ollama's registry (271 MB), every layer on the GPU, greedy sampling, on a 4-core
cloud machine with no GPU (Mesa's lavapipe, software Vulkan on those same cores, as the
GPU):

| | per token | |
|---|---|---|
| CPU only, natively (armless-exec, 4 threads) | 2.68 s | 0.37/s |
| GPU, natively (armless-exec --gpu, wgpu on lavapipe) | 0.81 s | 1.2/s |
| GPU, armless.wasm + armless-gpu.js (Dawn in Node on lavapipe) | 0.42 s | 2.4/s |

The text is the same on every row. Natively nearly all of the time is lavapipe running the
shaders; a real GPU takes most of it away. `tests/js/gpu-llama.mjs` runs the browser's code path
in Node (Dawn's `webgpu` package as `navigator.gpu`).

What made it fast:

- armless.js: a page answering a guest connection (`Connection.write`, `close`) wakes the
  machine, as the tunnel code did. Before, a user-mode guest slept until its next deadline after
  every reply: 10 s a token.
- armless-exec: an idle program is looked at again as soon as the host delivers to its sockets.
- The wgpu host records small writes as copies from staging buffers inside the encoder (wgpu makes
  a staging buffer per `write_buffer`, and a write after recorded commands needed them submitted
  first: ggml writes parameters before every dispatch).
- armless-gpu.js: one error scope per stretch of commands between replies.
- One CPU thread when the layers are on the GPU (`-t 1`): llama.cpp's other threads only spin,
  which on emulated CPUs took the time from everything else.

snowglobe's `examples/ollama` serves Ollama's library models with Ollama's API on top of this.

## Limits

Each guest has a budget, in both hosts, for all of its connections together, so opening more of
them gets it no more of the GPU: bytes of buffers alive (natively 4 GiB, or
`ARMLESS_GPU_MEMORY_MB`, or `Gpu::with_limits`; in the page 2 GiB, or `attach`'s `max_memory`),
objects alive (2^20, `max_objects`) and connections open (16, `max_connections`; each is a device
of its own). A buffer past it is an error the guest is told at its next reply; destroying a
buffer, or closing the connection that made it, gives its bytes back. A connection past it is
refused. A read's staging counts against the memory while the read lasts, and a read must be
smaller than a frame (256 MiB). Hello reports no `maxBufferSize` above the budget.

A `Gpu` is one guest's: `armless-run --gpu` and `armless-exec --gpu` make one for their guest.
`armless-gpu-server` makes one for everything that connects to it, so its guests share a budget.

The page takes the guest's bytes only while less than 32 MiB of whole frames wait to run (it says
so through armless.js's TCP flow control, `armless_tcp_consumed`): a guest sending faster than
the GPU works waits in its own socket buffer. The bytes of a frame still arriving are taken once
the queue drains, so any frame gets through. Natively the host reads the connection as it runs
the commands, which holds the guest back the same way.

## The protocol

Little-endian. Every message is a `u32` length and that many bytes; a command's first byte is
its op. The guest numbers its own objects, so only `Hello`, `ReadBuffer` and `Finish` wait for
an answer. Their reply starts with a status byte: 0, or 1 and the first error since the last
reply. A command that fails without a reply of its own (a shader that doesn't compile, an
unknown buffer) is reported at the next one, and the connection goes on. Buffer usages are
WebGPU's `GPUBufferUsage` bits. `gpu/src/wire.rs` has the full list and a client.

Dispatches and copies are recorded into one command encoder, submitted when the GPU has to
catch up: before a write (so a write never overtakes a dispatch sent before it), a read, or
`Finish`.

## Not yet

- Subgroups natively: naga (wgpu's shader compiler) doesn't take WGSL's `enable subgroups;` yet,
  so the wgpu host doesn't offer them and llama.cpp uses its kernels without. Dawn (the browser)
  offers them.
- A virtio device that reads buffers straight from guest RAM, so uploads skip the network stack
  (271 MB of weights take a few seconds natively; the bigger the model, the more it matters).
- The real Ollama: its build has no WebGPU backend. snowglobe's `examples/ollama` serves Ollama's
  models with Ollama's API on llama.cpp instead.
