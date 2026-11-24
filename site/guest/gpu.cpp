// The GPU example's guest program (site/examples/gpu.md): a 3D scene raymarched in real time on the
// visitor's GPU, from inside the machine, drawn in the console. It asks gpu.internal:7070 for a GPU
// (armless-gpu.js answers it with the page's WebGPU) and renders each frame with a WGSL compute
// shader there; `c` switches to the same renderer on the guest's own (emulated) CPU, to compare.
//
//   gpu [COLS ROWS] [--frames N]       (no size: the console's)
//
// --frames N renders N frames on the GPU and N on the CPU, then says how long each took on average.
// Built static for the guest against gpu/guest's webgpu_cpp.h, which speaks armless-gpu's protocol
// instead of a driver (site's pages.yml):
//   zig c++ -target aarch64-linux-musl -O2 -s -static -std=c++17 -I gpu/guest/include site/guest/gpu.cpp -o gpu
#include <webgpu/webgpu_cpp.h>

#include <chrono>
#include <cmath>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>
#include <algorithm>
#include <vector>

// ------------------------------------------------------------------------------------- the scene
//
// The same scene twice, kept line for line alike: a torus turning over a checkered floor, a ball
// bouncing round it, soft shadows, ambient occlusion and fog, the camera circling. Pixels are
// 0x00BBGGRR.

static const char * SHADER = R"(
struct Params { w: u32, h: u32, t: f32, pad: f32 };
@group(0) @binding(0) var<storage, read> p: Params;
@group(0) @binding(1) var<storage, read_write> out: array<u32>;

fn rot(v: vec2<f32>, a: f32) -> vec2<f32> {
  let c = cos(a);
  let s = sin(a);
  return vec2<f32>(c * v.x - s * v.y, s * v.x + c * v.y);
}

// x: the distance, y: the material (0 floor, 1 torus, 2 ball)
fn scene(q: vec3<f32>, t: f32) -> vec2<f32> {
  var r = q - vec3<f32>(0.0, 0.1, 0.0);
  let xz = rot(r.xz, t * 0.7);
  r = vec3<f32>(xz.x, r.y, xz.y);
  let xy = rot(r.xy, t * 0.5);
  r = vec3<f32>(xy.x, xy.y, r.z);
  let torus = length(vec2<f32>(length(r.xz) - 0.9, r.y)) - 0.28;
  let c = vec3<f32>(1.7 * sin(t * 0.9), -0.65 + 0.35 * abs(sin(t * 2.1)), 1.7 * cos(t * 0.9));
  let ball = length(q - c) - 0.35;
  var d = vec2<f32>(q.y + 1.0, 0.0);
  if (torus < d.x) { d = vec2<f32>(torus, 1.0); }
  if (ball < d.x) { d = vec2<f32>(ball, 2.0); }
  return d;
}

fn normal(q: vec3<f32>, t: f32) -> vec3<f32> {
  let e = 0.001;
  let a = scene(q + vec3<f32>(e, -e, -e), t).x;
  let b = scene(q + vec3<f32>(-e, -e, e), t).x;
  let c = scene(q + vec3<f32>(-e, e, -e), t).x;
  let d = scene(q + vec3<f32>(e, e, e), t).x;
  return normalize(vec3<f32>(a - b - c + d, -a - b + c + d, -a + b - c + d));
}

fn shadow(o: vec3<f32>, l: vec3<f32>, t: f32) -> f32 {
  var res = 1.0;
  var s = 0.02;
  for (var i = 0; i < 24; i++) {
    let h = scene(o + l * s, t).x;
    res = min(res, 8.0 * h / s);
    s += clamp(h, 0.02, 0.25);
    if (res < 0.01 || s > 6.0) { break; }
  }
  return clamp(res, 0.0, 1.0);
}

fn occlusion(o: vec3<f32>, n: vec3<f32>, t: f32) -> f32 {
  var occ = 0.0;
  for (var i = 1; i <= 4; i++) {
    let h = 0.06 * f32(i);
    occ += h - scene(o + n * h, t).x;
  }
  return clamp(1.0 - 3.0 * occ, 0.0, 1.0);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= p.w || id.y >= p.h) { return; }
  let t = p.t;
  let a = t * 0.25;
  let ro = vec3<f32>(4.2 * sin(a), 0.9, 4.2 * cos(a));
  let fwd = normalize(vec3<f32>(0.0, -0.2, 0.0) - ro);
  let right = normalize(cross(fwd, vec3<f32>(0.0, 1.0, 0.0)));
  let up = cross(right, fwd);
  let aspect = f32(p.w) / f32(p.h);
  let u = ((f32(id.x) + 0.5) / f32(p.w) * 2.0 - 1.0) * aspect;
  let v = 1.0 - (f32(id.y) + 0.5) / f32(p.h) * 2.0;
  let rd = normalize(fwd + right * (u * 0.9) + up * (v * 0.9));
  let sky = mix(vec3<f32>(0.55, 0.7, 0.95), vec3<f32>(0.1, 0.15, 0.35), clamp(rd.y * 1.5 + 0.2, 0.0, 1.0));
  var col = sky;
  var dist = 0.0;
  var mat = -1.0;
  for (var i = 0; i < 80; i++) {
    let h = scene(ro + rd * dist, t);
    if (h.x < 0.002 * dist) { mat = h.y; break; }
    dist += h.x;
    if (dist > 20.0) { break; }
  }
  if (mat >= 0.0) {
    let q = ro + rd * dist;
    let n = normal(q, t);
    let l = normalize(vec3<f32>(0.6, 0.8, 0.4));
    var base = vec3<f32>(1.0, 0.55, 0.15);
    if (mat < 0.5) {
      let check = (i32(floor(q.x)) + i32(floor(q.z))) & 1;
      base = select(vec3<f32>(0.25, 0.27, 0.32), vec3<f32>(0.62, 0.64, 0.7), check == 1);
    } else if (mat > 1.5) {
      base = vec3<f32>(0.2, 0.45, 1.0);
    }
    let sh = shadow(q + n * 0.02, l, t);
    let ao = occlusion(q, n, t);
    let diff = max(dot(n, l), 0.0) * sh;
    var spec = pow(max(dot(reflect(rd, n), l), 0.0), 24.0) * sh * 0.6;
    if (mat < 0.5) { spec = 0.0; }
    col = base * (0.25 * ao + diff) + vec3<f32>(spec);
    col = mix(col, sky, 1.0 - exp(-0.02 * dist * dist));
  }
  let g = sqrt(clamp(col, vec3<f32>(0.0), vec3<f32>(1.0))) * 255.0;
  out[id.y * p.w + id.x] = u32(g.x) | (u32(g.y) << 8u) | (u32(g.z) << 16u);
}
)";

struct V3 {
    float x, y, z;
};
static V3 operator+(V3 a, V3 b) { return {a.x + b.x, a.y + b.y, a.z + b.z}; }
static V3 operator-(V3 a, V3 b) { return {a.x - b.x, a.y - b.y, a.z - b.z}; }
static V3 operator*(V3 a, float s) { return {a.x * s, a.y * s, a.z * s}; }
static float dot(V3 a, V3 b) { return a.x * b.x + a.y * b.y + a.z * b.z; }
static float len(V3 a) { return std::sqrt(dot(a, a)); }
static V3 norm(V3 a) { return a * (1.0f / len(a)); }
static V3 cross(V3 a, V3 b) { return {a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x}; }
static float clampf(float v, float lo, float hi) { return v < lo ? lo : v > hi ? hi : v; }
static V3 mix(V3 a, V3 b, float k) { return a * (1 - k) + b * k; }
static void rot(float & x, float & y, float a) {
    float c = std::cos(a), s = std::sin(a), nx = c * x - s * y;
    y = s * x + c * y;
    x = nx;
}

struct Hit {
    float d, mat;
};
static Hit scene(V3 q, float t) {
    V3 r = q - V3{0, 0.1f, 0};
    rot(r.x, r.z, t * 0.7f);
    rot(r.x, r.y, t * 0.5f);
    float torus = std::sqrt((std::sqrt(r.x * r.x + r.z * r.z) - 0.9f) * (std::sqrt(r.x * r.x + r.z * r.z) - 0.9f) + r.y * r.y) - 0.28f;
    V3 c{1.7f * std::sin(t * 0.9f), -0.65f + 0.35f * std::fabs(std::sin(t * 2.1f)), 1.7f * std::cos(t * 0.9f)};
    float ball = len(q - c) - 0.35f;
    Hit h{q.y + 1.0f, 0};
    if (torus < h.d) h = {torus, 1};
    if (ball < h.d) h = {ball, 2};
    return h;
}
static V3 normal(V3 q, float t) {
    const float e = 0.001f;
    float a = scene(q + V3{e, -e, -e}, t).d, b = scene(q + V3{-e, -e, e}, t).d;
    float c = scene(q + V3{-e, e, -e}, t).d, d = scene(q + V3{e, e, e}, t).d;
    return norm({a - b - c + d, -a - b + c + d, -a + b - c + d});
}
static float shadow(V3 o, V3 l, float t) {
    float res = 1, s = 0.02f;
    for (int i = 0; i < 24; i++) {
        float h = scene(o + l * s, t).d;
        res = std::fmin(res, 8.0f * h / s);
        s += clampf(h, 0.02f, 0.25f);
        if (res < 0.01f || s > 6.0f) break;
    }
    return clampf(res, 0, 1);
}
static float occlusion(V3 o, V3 n, float t) {
    float occ = 0;
    for (int i = 1; i <= 4; i++) {
        float h = 0.06f * float(i);
        occ += h - scene(o + n * h, t).d;
    }
    return clampf(1 - 3 * occ, 0, 1);
}
static uint32_t pixel(uint32_t x, uint32_t y, uint32_t w, uint32_t h, float t) {
    float a = t * 0.25f;
    V3 ro{4.2f * std::sin(a), 0.9f, 4.2f * std::cos(a)};
    V3 fwd = norm(V3{0, -0.2f, 0} - ro), right = norm(cross(fwd, V3{0, 1, 0})), up = cross(right, fwd);
    float aspect = float(w) / float(h);
    float u = ((float(x) + 0.5f) / float(w) * 2 - 1) * aspect, v = 1 - (float(y) + 0.5f) / float(h) * 2;
    V3 rd = norm(fwd + right * (u * 0.9f) + up * (v * 0.9f));
    V3 sky = mix({0.55f, 0.7f, 0.95f}, {0.1f, 0.15f, 0.35f}, clampf(rd.y * 1.5f + 0.2f, 0, 1));
    V3 col = sky;
    float dist = 0, mat = -1;
    for (int i = 0; i < 80; i++) {
        Hit hh = scene(ro + rd * dist, t);
        if (hh.d < 0.002f * dist) {
            mat = hh.mat;
            break;
        }
        dist += hh.d;
        if (dist > 20) break;
    }
    if (mat >= 0) {
        V3 q = ro + rd * dist, n = normal(q, t), l = norm(V3{0.6f, 0.8f, 0.4f});
        V3 base{1.0f, 0.55f, 0.15f};
        if (mat < 0.5f) {
            int check = (int(std::floor(q.x)) + int(std::floor(q.z))) & 1;
            base = check == 1 ? V3{0.62f, 0.64f, 0.7f} : V3{0.25f, 0.27f, 0.32f};
        } else if (mat > 1.5f) {
            base = {0.2f, 0.45f, 1.0f};
        }
        float sh = shadow(q + n * 0.02f, l, t), ao = occlusion(q, n, t);
        float diff = std::fmax(dot(n, l), 0) * sh;
        V3 refl = rd - n * (2 * dot(n, rd));
        float spec = mat < 0.5f ? 0 : std::pow(std::fmax(dot(refl, l), 0), 24.0f) * sh * 0.6f;
        col = base * (0.25f * ao + diff) + V3{spec, spec, spec};
        col = mix(col, sky, 1 - std::exp(-0.02f * dist * dist));
    }
    auto ch = [](float c) { return uint32_t(std::sqrt(clampf(c, 0, 1)) * 255); };
    return ch(col.x) | (ch(col.y) << 8) | (ch(col.z) << 16);
}

// ----------------------------------------------------------------------------------- the console

static termios saved;
static bool raw = false;
static void restore() {
    std::fputs("\x1b[0m\x1b[?25h\n", stdout);
    std::fflush(stdout);
    if (raw) tcsetattr(0, TCSANOW, &saved);
}
static void on_signal(int) {
    restore();
    std::_Exit(130);
}

// Four pixels a cell, as quadrant blocks: of the 2x2 pixels, the ones in the foreground colour
// (bit 0 top left, 1 top right, 2 bottom left, 3 bottom right) and the rest in the background's,
// the split that loses the least of the four. Masks 8..14 are 7..1 with the colours swapped.
static const char * const QUADRANT[16] = {
    " ", "\xe2\x96\x98", "\xe2\x96\x9d", "\xe2\x96\x80", "\xe2\x96\x96", "\xe2\x96\x8c", "\xe2\x96\x9e", "\xe2\x96\x9b",
    "\xe2\x96\x97", "\xe2\x96\x9a", "\xe2\x96\x90", "\xe2\x96\x9c", "\xe2\x96\x84", "\xe2\x96\x99", "\xe2\x96\x9f", "\xe2\x96\x88",
};

struct Rgb {
    int r, g, b;
    bool operator!=(const Rgb & o) const { return r != o.r || g != o.g || b != o.b; }
};
static Rgb rgb(uint32_t px) { return {int(px & 255), int((px >> 8) & 255), int((px >> 16) & 255)}; }

// The cell's glyph and its two colours: every split of the four pixels into two (7 of them, and
// all four alike), each side its mean colour, the one with the least squared error
static void cell(const Rgb p[4], int & mask, Rgb & fg, Rgb & bg) {
    long best = -1;
    for (int m : {15, 1, 2, 3, 4, 5, 6, 7}) {
        long sum[2][3] = {{0, 0, 0}, {0, 0, 0}};
        int n[2] = {0, 0};
        for (int i = 0; i < 4; i++) {
            int side = (m >> i) & 1;
            sum[side][0] += p[i].r, sum[side][1] += p[i].g, sum[side][2] += p[i].b;
            n[side]++;
        }
        Rgb mean[2];
        for (int s = 0; s < 2; s++) {
            mean[s] = n[s] ? Rgb{int(sum[s][0] / n[s]), int(sum[s][1] / n[s]), int(sum[s][2] / n[s])} : Rgb{0, 0, 0};
        }
        long err = 0;
        for (int i = 0; i < 4; i++) {
            const Rgb & c = mean[(m >> i) & 1];
            err += long(p[i].r - c.r) * (p[i].r - c.r) + long(p[i].g - c.g) * (p[i].g - c.g) + long(p[i].b - c.b) * (p[i].b - c.b);
        }
        if (best < 0 || err < best) {
            best = err;
            mask = m;
            fg = mean[1];
            bg = mean[0];
        }
    }
}

// The frame over the last one: the header, then four pixels a cell in 24-bit colour, a colour said
// only when it changes
static void draw(const std::vector<uint32_t> & px, uint32_t w, uint32_t h, const std::string & header) {
    std::string s = "\x1b[H\x1b[0m" + header + "\x1b[K\r\n";
    for (uint32_t y = 0; y + 1 < h; y += 2) {
        Rgb fg{-1, -1, -1}, bg{-1, -1, -1};
        for (uint32_t x = 0; x + 1 < w; x += 2) {
            Rgb p[4] = {rgb(px[y * w + x]), rgb(px[y * w + x + 1]), rgb(px[(y + 1) * w + x]), rgb(px[(y + 1) * w + x + 1])};
            int mask;
            Rgb f, b;
            cell(p, mask, f, b);
            char buf[48];
            int k = 0;
            if (mask != 15 && b != bg) k += std::snprintf(buf + k, sizeof buf - k, "\x1b[48;2;%d;%d;%dm", b.r, b.g, b.b), bg = b;
            if (f != fg) k += std::snprintf(buf + k, sizeof buf - k, "\x1b[38;2;%d;%d;%dm", f.r, f.g, f.b), fg = f;
            s.append(buf, size_t(k));
            s += QUADRANT[mask];
        }
        s += "\x1b[0m";
        if (y + 3 < h) s += "\r\n";
    }
    std::fwrite(s.data(), 1, s.size(), stdout);
    std::fflush(stdout);
}

// ---------------------------------------------------------------------------------------- the GPU

struct Gpu {
    wgpu::Instance instance;
    wgpu::Device device;
    wgpu::Queue queue;
    wgpu::ComputePipeline pipeline;
    wgpu::Buffer params, out, back;
    wgpu::BindGroup group;
    std::string name;
    size_t bytes = 0;
};

struct Params {
    uint32_t w, h;
    float t, pad;
};

static bool open_gpu(Gpu & g, uint32_t w, uint32_t h) {
    g.instance = wgpu::CreateInstance();
    wgpu::Adapter adapter;
    g.instance.WaitAny(g.instance.RequestAdapter(nullptr, wgpu::CallbackMode::WaitAnyOnly,
                                                 [&](wgpu::RequestAdapterStatus s, wgpu::Adapter a, wgpu::StringView) {
                                                     if (s == wgpu::RequestAdapterStatus::Success) adapter = a;
                                                 }),
                       UINT64_MAX);
    if (!adapter) return false;
    wgpu::AdapterInfo info;
    adapter.GetInfo(&info);
    auto str = [](wgpu::StringView v) { return std::string(std::string_view(v)); };
    // The adapter's description, else its device (which often has its vendor's name already)
    g.name = str(info.description);
    if (g.name.empty()) g.name = str(info.device);
    if (g.name.empty()) g.name = str(info.vendor) + " " + str(info.architecture);
    g.instance.WaitAny(adapter.RequestDevice(nullptr, wgpu::CallbackMode::WaitAnyOnly,
                                             [&](wgpu::RequestDeviceStatus s, wgpu::Device d, wgpu::StringView) {
                                                 if (s == wgpu::RequestDeviceStatus::Success) g.device = d;
                                             }),
                       UINT64_MAX);
    if (!g.device) return false;
    g.queue = g.device.GetQueue();

    wgpu::ShaderSourceWGSL wgsl;
    wgsl.code = SHADER;
    wgpu::ShaderModuleDescriptor smd;
    smd.nextInChain = &wgsl;
    wgpu::ComputePipelineDescriptor pd;
    pd.compute.module = g.device.CreateShaderModule(&smd);
    pd.compute.entryPoint = "main";
    g.pipeline = g.device.CreateComputePipeline(&pd);

    g.bytes = size_t(w) * h * 4;
    wgpu::BufferDescriptor bd;
    bd.size = sizeof(Params);
    bd.usage = wgpu::BufferUsage::Storage | wgpu::BufferUsage::CopyDst;
    g.params = g.device.CreateBuffer(&bd);
    bd.size = g.bytes;
    bd.usage = wgpu::BufferUsage::Storage | wgpu::BufferUsage::CopySrc;
    g.out = g.device.CreateBuffer(&bd);
    bd.usage = wgpu::BufferUsage::MapRead | wgpu::BufferUsage::CopyDst;
    g.back = g.device.CreateBuffer(&bd);

    wgpu::BindGroupEntry entries[2];
    entries[0].binding = 0;
    entries[0].buffer = g.params;
    entries[1].binding = 1;
    entries[1].buffer = g.out;
    wgpu::BindGroupDescriptor bgd;
    bgd.layout = g.pipeline.GetBindGroupLayout(0);
    bgd.entryCount = 2;
    bgd.entries = entries;
    g.group = g.device.CreateBindGroup(&bgd);
    return true;
}

// A frame of w x h, at most the size open_gpu made the buffers for (the console can shrink)
static bool render_gpu(Gpu & g, std::vector<uint32_t> & px, uint32_t w, uint32_t h, float t) {
    const size_t n = size_t(w) * h * 4;
    if (n > g.bytes) return false;
    Params pv{w, h, t, 0};
    g.queue.WriteBuffer(g.params, 0, &pv, sizeof pv);
    wgpu::CommandEncoder enc = g.device.CreateCommandEncoder();
    wgpu::ComputePassEncoder pass = enc.BeginComputePass();
    pass.SetPipeline(g.pipeline);
    pass.SetBindGroup(0, g.group);
    pass.DispatchWorkgroups((w + 7) / 8, (h + 7) / 8);
    pass.End();
    enc.CopyBufferToBuffer(g.out, 0, g.back, 0, n);
    wgpu::CommandBuffer cmds = enc.Finish();
    g.queue.Submit(1, &cmds);
    bool mapped = false;
    g.instance.WaitAny(g.back.MapAsync(wgpu::MapMode::Read, 0, n, wgpu::CallbackMode::WaitAnyOnly,
                                       [&](wgpu::MapAsyncStatus s, wgpu::StringView) { mapped = s == wgpu::MapAsyncStatus::Success; }),
                       UINT64_MAX);
    const uint32_t * got = mapped ? static_cast<const uint32_t *>(g.back.GetConstMappedRange(0, n)) : nullptr;
    if (got) std::memcpy(px.data(), got, n);
    g.back.Unmap();
    return got != nullptr;
}

static void render_cpu(std::vector<uint32_t> & px, uint32_t w, uint32_t h, float t) {
    for (uint32_t y = 0; y < h; y++) {
        for (uint32_t x = 0; x < w; x++) px[y * w + x] = pixel(x, y, w, h, t);
    }
}

// ----------------------------------------------------------------------------------------- main

int main(int argc, char ** argv) {
    int cols = 100, rows = 30, frames = 0;
    std::vector<int> nums;
    for (int i = 1; i < argc; i++) {
        if (!std::strcmp(argv[i], "--frames") && i + 1 < argc) frames = std::atoi(argv[++i]);
        else nums.push_back(std::atoi(argv[i]));
    }
    // The size given, or else the console's
    winsize ws{};
    if (nums.size() >= 2) cols = nums[0], rows = nums[1];
    else if (ioctl(1, TIOCGWINSZ, &ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0) cols = ws.ws_col - 1, rows = ws.ws_row - 1;
    if (cols < 20) cols = 20;
    if (rows < 8) rows = 8;
    // Four pixels a cell (2x2), under a header line
    const uint32_t w = uint32_t(cols) * 2, h = uint32_t(rows - 1) * 2;

    Gpu g;
    if (!open_gpu(g, w, h)) {
        std::printf("No GPU answered at gpu.internal:7070: this browser has no WebGPU here.\n");
        return 2;
    }
    std::printf("GPU: %s\n", g.name.c_str());
    std::vector<uint32_t> px(size_t(w) * h);
    auto now = [] { return std::chrono::steady_clock::now(); };
    auto ms = [](std::chrono::steady_clock::time_point a, std::chrono::steady_clock::time_point b) {
        return std::chrono::duration<double, std::milli>(b - a).count();
    };

    // Timing only: N frames each way, then the averages
    if (frames > 0) {
        double sums[2] = {0, 0};
        for (int on_cpu = 0; on_cpu < 2; on_cpu++) {
            for (int f = 0; f < frames; f++) {
                auto t0 = now();
                if (on_cpu) render_cpu(px, w, h, float(f) * 0.1f);
                else if (!render_gpu(g, px, w, h, float(f) * 0.1f)) return 1;
                sums[on_cpu] += ms(t0, now());
            }
        }
        std::printf("%ux%u pixels, %d frames each: GPU %.1f ms a frame, this machine's CPU %.1f ms a frame (%.1fx)\n",
                    w, h, frames, sums[0] / frames, sums[1] / frames, sums[0] > 0 ? sums[1] / sums[0] : 0.0);
        return 0;
    }

    if (isatty(0) && tcgetattr(0, &saved) == 0) {
        termios t = saved;
        t.c_lflag &= ~(ICANON | ECHO);
        t.c_cc[VMIN] = 0;
        t.c_cc[VTIME] = 0;
        raw = tcsetattr(0, TCSANOW, &t) == 0;
    }
    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);
    std::fputs("\x1b[2J\x1b[?25l", stdout);

    bool on_cpu = false;
    double fps = 0, render_ms = 0;
    auto start = now(), last = start;
    // The frame fits the console as it is now, not only as it was when the program started: a
    // console that shrank under a frame drawn for its old size would scroll a line every frame
    int was_cols = cols, was_rows = rows;
    for (;;) {
        int fc = cols, fr = rows;
        if (ioctl(1, TIOCGWINSZ, &ws) == 0 && ws.ws_col > 1 && ws.ws_row > 2) {
            fc = std::min(fc, int(ws.ws_col) - 1);
            fr = std::min(fr, int(ws.ws_row) - 1);
        }
        if (fc != was_cols || fr != was_rows) {
            std::fputs("\x1b[2J", stdout);
            was_cols = fc, was_rows = fr;
        }
        const uint32_t fw = uint32_t(fc) * 2, fh = uint32_t(fr - 1) * 2;
        float t = float(ms(start, now()) / 1000.0);
        auto t0 = now();
        if (on_cpu) render_cpu(px, fw, fh, t);
        else if (!render_gpu(g, px, fw, fh, t)) break;
        double took = ms(t0, now());
        render_ms = render_ms == 0 ? took : render_ms * 0.8 + took * 0.2;
        // One line, as wide as the console at most: who rendered it (GPU <name>, which the page
        // reads, or the CPU), how fast, and the keys
        char header[512];
        int len = on_cpu ? std::snprintf(header, sizeof header, "CPU, emulated \xc2\xb7 %.1f fps \xc2\xb7 %.1f ms a frame \xc2\xb7 c: back to the GPU \xc2\xb7 q: quit", fps, render_ms)
                         : std::snprintf(header, sizeof header, "GPU %s \xc2\xb7 %.1f fps \xc2\xb7 %.1f ms a frame \xc2\xb7 c: the same on the CPU \xc2\xb7 q: quit",
                                         g.name.c_str(), fps, render_ms);
        std::string line(header, size_t(len > 0 ? len : 0));
        for (size_t chars = 0, i = 0; i < line.size(); i++) {
            if ((line[i] & 0xc0) != 0x80 && ++chars > size_t(fc)) {
                line.resize(i);
                break;
            }
        }
        draw(px, fw, fh, "\x1b[1m" + line + "\x1b[0m");
        auto t1 = now();
        double frame = ms(last, t1);
        last = t1;
        fps = fps == 0 ? 1000.0 / frame : fps * 0.8 + (1000.0 / frame) * 0.2;
        char key[16];
        ssize_t n = read(0, key, sizeof key);
        for (ssize_t i = 0; i < n; i++) {
            if (key[i] == 'c' || key[i] == 'C') {
                on_cpu = !on_cpu;
                fps = 0;
                render_ms = 0;
            }
            if (key[i] == 'q' || key[i] == 'Q') {
                restore();
                return 0;
            }
        }
    }
    restore();
    return 1;
}
