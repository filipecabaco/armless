// webgpu_cpp.h against a running armless-gpu host: writes and map reads larger than a frame (256
// MiB) arrive whole, and ranges outside a buffer are refused rather than copied past it.
//
//   c++ -std=c++17 -I gpu/guest/include gpu/guest/tests/transfers.cpp -o transfers
//   armless-gpu-server 127.0.0.1:7070 &
//   ARMLESS_GPU=127.0.0.1:7070 ./transfers
#include <webgpu/webgpu_cpp.h>

#include <cstdio>
#include <vector>

static int failures = 0;
static void check(bool ok, const char * what) {
    std::printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) failures++;
}

int main() {
    wgpu::Instance instance = wgpu::CreateInstance();
    wgpu::Adapter adapter;
    instance.WaitAny(instance.RequestAdapter(nullptr, wgpu::CallbackMode::WaitAnyOnly,
                                             [&](wgpu::RequestAdapterStatus s, wgpu::Adapter a, wgpu::StringView) {
                                                 if (s == wgpu::RequestAdapterStatus::Success) adapter = a;
                                             }),
                     UINT64_MAX);
    if (!adapter) {
        std::printf("no GPU host (ARMLESS_GPU)\n");
        return 2;
    }
    wgpu::Device device;
    instance.WaitAny(adapter.RequestDevice(nullptr, wgpu::CallbackMode::WaitAnyOnly,
                                           [&](wgpu::RequestDeviceStatus s, wgpu::Device d, wgpu::StringView) {
                                               if (s == wgpu::RequestDeviceStatus::Success) device = d;
                                           }),
                     UINT64_MAX);
    wgpu::Queue queue = device.GetQueue();

    // 300 MB: more than a frame, and not a multiple of the 64 MiB pieces
    const size_t size = 300u << 20;
    std::vector<uint8_t> data(size);
    for (size_t i = 0; i < size; i++) data[i] = uint8_t((i * 2654435761u) >> 24);

    wgpu::BufferDescriptor bd;
    bd.size = size;
    bd.usage = wgpu::BufferUsage::Storage | wgpu::BufferUsage::CopySrc | wgpu::BufferUsage::CopyDst;
    wgpu::Buffer buf = device.CreateBuffer(&bd);
    bd.usage = wgpu::BufferUsage::MapRead | wgpu::BufferUsage::CopyDst;
    wgpu::Buffer back = device.CreateBuffer(&bd);

    queue.WriteBuffer(buf, 0, data.data(), size);
    wgpu::CommandEncoder enc = device.CreateCommandEncoder();
    enc.CopyBufferToBuffer(buf, 0, back, 0, size);
    wgpu::CommandBuffer cmds = enc.Finish();
    queue.Submit(1, &cmds);

    bool mapped = false;
    instance.WaitAny(back.MapAsync(wgpu::MapMode::Read, 0, size, wgpu::CallbackMode::WaitAnyOnly,
                                   [&](wgpu::MapAsyncStatus s, wgpu::StringView) { mapped = s == wgpu::MapAsyncStatus::Success; }),
                     UINT64_MAX);
    check(mapped, "a 300 MB map read succeeds");
    const uint8_t * got = static_cast<const uint8_t *>(back.GetConstMappedRange(0, size));
    bool same = got != nullptr;
    for (size_t i = 0; same && i < size; i++) same = got[i] == data[i];
    check(same, "a 300 MB write and map read carry every byte");

    check(back.GetConstMappedRange(size - 4, 4) != nullptr, "the mapped range's last bytes are there");
    check(back.GetConstMappedRange(size - 4, 8) == nullptr, "a mapped range past the buffer is null");
    check(back.GetConstMappedRange(size + 4, 0) == nullptr, "a mapped range starting past the buffer is null");
    back.Unmap();
    check(back.GetConstMappedRange(0, 4) == nullptr, "nothing is mapped after Unmap");

    // A part of the buffer mapped: only that part is there
    mapped = false;
    instance.WaitAny(back.MapAsync(wgpu::MapMode::Read, 4096, 4096, wgpu::CallbackMode::WaitAnyOnly,
                                   [&](wgpu::MapAsyncStatus s, wgpu::StringView) { mapped = s == wgpu::MapAsyncStatus::Success; }),
                     UINT64_MAX);
    const uint8_t * part = static_cast<const uint8_t *>(back.GetConstMappedRange(4096, 4096));
    check(mapped && part != nullptr && part[0] == data[4096] && part[4095] == data[8191], "a partial map read has its range");
    check(back.GetConstMappedRange(0, 4) == nullptr, "a range before the mapped one is null");
    check(back.GetConstMappedRange(8192, 4) == nullptr, "a range after the mapped one is null");
    check(back.GetConstMappedRange(4096, 4097) == nullptr, "a range running past the mapped one is null");
    back.Unmap();

    bool refused = false;
    instance.WaitAny(back.MapAsync(wgpu::MapMode::Read, size - 4, 8, wgpu::CallbackMode::WaitAnyOnly,
                                   [&](wgpu::MapAsyncStatus s, wgpu::StringView) { refused = s == wgpu::MapAsyncStatus::Error; }),
                     UINT64_MAX);
    check(refused, "a map read past the buffer is refused");
    refused = false;
    instance.WaitAny(back.MapAsync(wgpu::MapMode::Read, size + 4, wgpu::kWholeMapSize, wgpu::CallbackMode::WaitAnyOnly,
                                   [&](wgpu::MapAsyncStatus s, wgpu::StringView) { refused = s == wgpu::MapAsyncStatus::Error; }),
                     UINT64_MAX);
    check(refused, "a whole-buffer map read from past the buffer is refused");

    return failures == 0 ? 0 : 1;
}
