// The GPU example's guest program (site/examples/gpu.md): WebGPU compute on the visitor's GPU from
// inside the machine. It asks gpu.internal:7070 for a GPU (armless-gpu.js answers it with the
// page's WebGPU), runs a Mandelbrot set as a WGSL compute shader there, runs the same on the
// guest's own (emulated) CPU, and draws the GPU's in the console with both times.
//
//   gpu [COLS ROWS]
//
// Built static for the guest against gpu/guest's webgpu_cpp.h, which speaks armless-gpu's protocol
// instead of a driver (site's pages.yml):
//   zig c++ -target aarch64-linux-musl -O2 -static -std=c++17 -I gpu/guest/include site/guest/gpu.cpp -o gpu
#include <webgpu/webgpu_cpp.h>

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

// The picture computed both ways: the same size and iterations, so the times compare
static const uint32_t W = 640, H = 320, ITERS = 256;
static const float CX = -0.65f, CY = 0.0f, SCALE = 1.25f;

static const char * SHADER = R"(
struct Params { w: u32, h: u32, iters: u32, pad: u32, cx: f32, cy: f32, scale: f32, pad2: f32 };
@group(0) @binding(0) var<storage, read> p: Params;
@group(0) @binding(1) var<storage, read_write> out: array<u32>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= p.w || id.y >= p.h) { return; }
  let x0 = p.cx + (f32(id.x) / f32(p.w) - 0.5) * p.scale * 2.0;
  let y0 = p.cy + (f32(id.y) / f32(p.h) - 0.5) * p.scale;
  var x = 0.0;
  var y = 0.0;
  var i = 0u;
  loop {
    if (i >= p.iters || x * x + y * y > 4.0) { break; }
    let xt = x * x - y * y + x0;
    y = 2.0 * x * y + y0;
    x = xt;
    i = i + 1u;
  }
  out[id.y * p.w + id.x] = i;
}
)";

struct Params {
    uint32_t w, h, iters, pad;
    float cx, cy, scale, pad2;
};

static double ms_since(std::chrono::steady_clock::time_point t) {
    return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t).count();
}

// The same loop as the shader, in f32, on the guest's CPU
static std::vector<uint32_t> on_cpu() {
    std::vector<uint32_t> out(W * H);
    for (uint32_t py = 0; py < H; py++) {
        for (uint32_t px = 0; px < W; px++) {
            float x0 = CX + (float(px) / float(W) - 0.5f) * SCALE * 2.0f;
            float y0 = CY + (float(py) / float(H) - 0.5f) * SCALE;
            float x = 0, y = 0;
            uint32_t i = 0;
            while (i < ITERS && x * x + y * y <= 4.0f) {
                float xt = x * x - y * y + x0;
                y = 2.0f * x * y + y0;
                x = xt;
                i++;
            }
            out[py * W + px] = i;
        }
    }
    return out;
}

static std::string view(wgpu::StringView s) { return std::string(std::string_view(s)); }

int main(int argc, char ** argv) {
    int cols = argc > 2 ? std::atoi(argv[1]) : 100, rows = argc > 2 ? std::atoi(argv[2]) : 30;
    if (cols < 20) cols = 20;
    if (rows < 8) rows = 8;

    wgpu::Instance instance = wgpu::CreateInstance();
    wgpu::Adapter adapter;
    instance.WaitAny(instance.RequestAdapter(nullptr, wgpu::CallbackMode::WaitAnyOnly,
                                             [&](wgpu::RequestAdapterStatus s, wgpu::Adapter a, wgpu::StringView) {
                                                 if (s == wgpu::RequestAdapterStatus::Success) adapter = a;
                                             }),
                     UINT64_MAX);
    if (!adapter) {
        std::printf("No GPU answered at gpu.internal:7070: this browser has no WebGPU here.\n");
        return 2;
    }
    wgpu::AdapterInfo info;
    adapter.GetInfo(&info);
    std::string name = view(info.description);
    if (name.empty()) name = view(info.vendor) + " " + view(info.architecture) + " " + view(info.device);
    std::printf("GPU: %s\n", name.c_str());

    wgpu::Device device;
    instance.WaitAny(adapter.RequestDevice(nullptr, wgpu::CallbackMode::WaitAnyOnly,
                                           [&](wgpu::RequestDeviceStatus s, wgpu::Device d, wgpu::StringView) {
                                               if (s == wgpu::RequestDeviceStatus::Success) device = d;
                                           }),
                     UINT64_MAX);
    if (!device) {
        std::printf("The GPU gave no device.\n");
        return 2;
    }
    wgpu::Queue queue = device.GetQueue();

    wgpu::ShaderSourceWGSL wgsl;
    wgsl.code = SHADER;
    wgpu::ShaderModuleDescriptor smd;
    smd.nextInChain = &wgsl;
    wgpu::ComputePipelineDescriptor pd;
    pd.compute.module = device.CreateShaderModule(&smd);
    pd.compute.entryPoint = "main";
    wgpu::ComputePipeline pipeline = device.CreateComputePipeline(&pd);

    const size_t bytes = size_t(W) * H * 4;
    wgpu::BufferDescriptor bd;
    bd.size = sizeof(Params);
    bd.usage = wgpu::BufferUsage::Storage | wgpu::BufferUsage::CopyDst;
    wgpu::Buffer params = device.CreateBuffer(&bd);
    bd.size = bytes;
    bd.usage = wgpu::BufferUsage::Storage | wgpu::BufferUsage::CopySrc;
    wgpu::Buffer out = device.CreateBuffer(&bd);
    bd.usage = wgpu::BufferUsage::MapRead | wgpu::BufferUsage::CopyDst;
    wgpu::Buffer back = device.CreateBuffer(&bd);

    wgpu::BindGroupEntry entries[2];
    entries[0].binding = 0;
    entries[0].buffer = params;
    entries[1].binding = 1;
    entries[1].buffer = out;
    wgpu::BindGroupDescriptor bgd;
    bgd.layout = pipeline.GetBindGroupLayout(0);
    bgd.entryCount = 2;
    bgd.entries = entries;
    wgpu::BindGroup group = device.CreateBindGroup(&bgd);

    // On the GPU: the parameters in, the pass, the picture back (all of it timed, transfers included)
    auto t0 = std::chrono::steady_clock::now();
    Params pv{W, H, ITERS, 0, CX, CY, SCALE, 0};
    queue.WriteBuffer(params, 0, &pv, sizeof pv);
    wgpu::CommandEncoder enc = device.CreateCommandEncoder();
    wgpu::ComputePassEncoder pass = enc.BeginComputePass();
    pass.SetPipeline(pipeline);
    pass.SetBindGroup(0, group);
    pass.DispatchWorkgroups((W + 7) / 8, (H + 7) / 8);
    pass.End();
    enc.CopyBufferToBuffer(out, 0, back, 0, bytes);
    wgpu::CommandBuffer cmds = enc.Finish();
    queue.Submit(1, &cmds);
    bool mapped = false;
    instance.WaitAny(back.MapAsync(wgpu::MapMode::Read, 0, bytes, wgpu::CallbackMode::WaitAnyOnly,
                                   [&](wgpu::MapAsyncStatus s, wgpu::StringView) { mapped = s == wgpu::MapAsyncStatus::Success; }),
                     UINT64_MAX);
    const uint32_t * gpu = mapped ? static_cast<const uint32_t *>(back.GetConstMappedRange(0, bytes)) : nullptr;
    double gpu_ms = ms_since(t0);
    if (!gpu) {
        std::printf("The GPU's result didn't come back.\n");
        return 1;
    }

    // On the guest's CPU, emulated
    t0 = std::chrono::steady_clock::now();
    std::vector<uint32_t> cpu = on_cpu();
    double cpu_ms = ms_since(t0);
    size_t same = 0;
    for (size_t i = 0; i < size_t(W) * H; i++) same += gpu[i] == cpu[i];

    // The GPU's picture, sampled to the console's size, darkest outside to brightest near the set
    static const char shades[] = " .:-=+*#%@";
    static const int colors[] = {17, 18, 19, 20, 21, 27, 33, 39, 45, 51, 87, 123, 159, 195, 230, 229, 228, 227, 226, 220};
    const int lines = rows - 4;
    std::string frame;
    for (int r = 0; r < lines; r++) {
        for (int c = 0; c < cols; c++) {
            uint32_t n = gpu[(uint32_t(r) * H / lines) * W + uint32_t(c) * W / cols];
            if (n >= ITERS) {
                frame += ' ';
                continue;
            }
            int k = int(n * 19 / 40 > 19 ? 19 : n * 19 / 40);
            char buf[32];
            std::snprintf(buf, sizeof buf, "\x1b[38;5;%dm%c", colors[k], shades[k / 2]);
            frame += buf;
        }
        frame += "\x1b[0m\n";
    }
    std::fputs(frame.c_str(), stdout);
    std::printf("%ux%u, %u iterations: GPU %.1f ms (transfers included), this machine's CPU %.1f ms: %.0fx\n",
                W, H, ITERS, gpu_ms, cpu_ms, gpu_ms > 0 ? cpu_ms / gpu_ms : 0.0);
    std::printf("%.2f%% of the pixels agree (the GPU's f32 may round differently at the edge)\n", 100.0 * same / (double(W) * H));
    back.Unmap();
    return 0;
}
