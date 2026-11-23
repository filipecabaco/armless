// webgpu_cpp.h for armless guests: as much of Dawn's C++ WebGPU API as llama.cpp's WebGPU
// backend (ggml-webgpu) uses, implemented over armless-gpu's protocol (gpu/src/wire.rs) instead
// of a GPU driver. The guest program connects to gpu.internal:7070 (or $ARMLESS_GPU, HOST:PORT)
// and the host runs the commands on its GPU: natively on wgpu, in the browser on WebGPU.
//
// What it does differently from a real implementation, and why it still behaves the same:
// - Command encoders record in the guest; Queue::Submit sends what was recorded. Queue writes
//   are sent at once. The host runs everything in the order it arrives, which is WebGPU's queue
//   order.
// - Buffers the guest maps for reading (MapRead) are read through the host: MapAsync asks for
//   the range, the reply fills a copy here, GetConstMappedRange points into it.
// - Futures are answered in order. A call that needs the host (MapAsync, OnSubmittedWorkDone,
//   RequestAdapter) sends its command at once and returns; waiting on its future reads replies
//   until its own has come, running the callbacks of those before it. Nothing blocks before the
//   program waits, so the guest's CPU and the host's GPU work at the same time.
// - Objects are reference counted; the last reference going sends Destroy.
//
// Thread safety: one lock around the connection (Dawn's ImplicitDeviceSynchronization).
//
// Not covered: rendering, textures, samplers, explicit layouts, timestamp queries.

#pragma once

#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace wgpu {

// ------------------------------------------------------------------------------------ basics

constexpr uint64_t kWholeSize = UINT64_MAX;
constexpr size_t kWholeMapSize = SIZE_MAX;

struct StringView {
    const char * data = nullptr;
    size_t length = SIZE_MAX;  // SIZE_MAX: NUL-terminated
    StringView() = default;
    StringView(const char * s) : data(s), length(SIZE_MAX) {}
    StringView(const char * s, size_t n) : data(s), length(n) {}
    StringView(std::string_view s) : data(s.data()), length(s.size()) {}
    StringView(const std::string & s) : data(s.data()), length(s.size()) {}
    operator std::string_view() const {
        if (!data) return {};
        return length == SIZE_MAX ? std::string_view(data) : std::string_view(data, length);
    }
};

enum class SType : uint32_t {
    ShaderSourceWGSL = 2,
    DawnTogglesDescriptor = 0x50000,
    AdapterPropertiesSubgroupMatrixConfigs = 0x50001,
};

struct ChainedStruct {
    const ChainedStruct * nextInChain = nullptr;
    SType sType = SType(0);
};

enum class BufferUsage : uint64_t {
    None = 0,
    MapRead = 0x1,
    MapWrite = 0x2,
    CopySrc = 0x4,
    CopyDst = 0x8,
    Index = 0x10,
    Vertex = 0x20,
    Uniform = 0x40,
    Storage = 0x80,
    Indirect = 0x100,
    QueryResolve = 0x200,
};
inline BufferUsage operator|(BufferUsage a, BufferUsage b) { return BufferUsage(uint64_t(a) | uint64_t(b)); }
inline BufferUsage operator&(BufferUsage a, BufferUsage b) { return BufferUsage(uint64_t(a) & uint64_t(b)); }
inline BufferUsage & operator|=(BufferUsage & a, BufferUsage b) { return a = a | b; }

enum class MapMode : uint64_t { None = 0, Read = 1, Write = 2 };
inline MapMode operator|(MapMode a, MapMode b) { return MapMode(uint64_t(a) | uint64_t(b)); }

enum class CallbackMode : uint32_t { WaitAnyOnly = 1, AllowProcessEvents = 2, AllowSpontaneous = 3 };
enum class WaitStatus : uint32_t { Success = 1, TimedOut = 2, Error = 3 };
enum class Status : uint32_t { Success = 1, Error = 2 };
enum class RequestAdapterStatus : uint32_t { Success = 1, CallbackCancelled = 2, Unavailable = 3, Error = 4 };
enum class RequestDeviceStatus : uint32_t { Success = 1, CallbackCancelled = 2, Error = 3 };
enum class MapAsyncStatus : uint32_t { Success = 1, CallbackCancelled = 2, Error = 3, Aborted = 4 };
enum class QueueWorkDoneStatus : uint32_t { Success = 1, CallbackCancelled = 2, Error = 3 };
enum class ErrorType : uint32_t { NoError = 1, Validation = 2, OutOfMemory = 3, Internal = 4, Unknown = 5 };
enum class DeviceLostReason : uint32_t { Unknown = 1, Destroyed = 2, CallbackCancelled = 3, FailedCreation = 4 };
enum class QueryType : uint32_t { Occlusion = 1, Timestamp = 2 };
enum class FeatureName : uint32_t {
    TimestampQuery = 9,
    ShaderF16 = 11,
    Subgroups = 18,
    ImplicitDeviceSynchronization = 0x50000,
    ChromiumExperimentalSubgroupMatrix = 0x50001,
};
enum class InstanceFeatureName : uint32_t { TimedWaitAny = 1, ShaderSourceSPIRV = 2, MultipleDevicesPerAdapter = 3 };
enum class WGSLLanguageFeatureName : uint32_t {
    ReadonlyAndReadwriteStorageTextures = 1,
    Packed4x8IntegerDotProduct = 2,
    UnrestrictedPointerParameters = 3,
    PointerCompositeAccess = 4,
};
enum class SubgroupMatrixComponentType : uint32_t { F32 = 1, F16 = 2, U32 = 3, I32 = 4 };
enum class PowerPreference : uint32_t { Undefined = 0, LowPower = 1, HighPerformance = 2 };
enum class BackendType : uint32_t { Undefined = 0, Null = 1, WebGPU = 2 };
enum class AdapterType : uint32_t { DiscreteGPU = 1, IntegratedGPU = 2, CPU = 3, Unknown = 4 };
enum class FeatureLevel : uint32_t { Undefined = 0, Compatibility = 1, Core = 2 };

struct Future {
    uint64_t id = 0;
};

struct FutureWaitInfo {
    Future future;
    bool completed = false;
};

struct Limits {
    ChainedStruct * nextInChain = nullptr;
    uint32_t maxTextureDimension1D = 0;
    uint32_t maxTextureDimension2D = 0;
    uint32_t maxTextureDimension3D = 0;
    uint32_t maxTextureArrayLayers = 0;
    uint32_t maxBindGroups = 0;
    uint32_t maxBindGroupsPlusVertexBuffers = 0;
    uint32_t maxBindingsPerBindGroup = 0;
    uint32_t maxDynamicUniformBuffersPerPipelineLayout = 0;
    uint32_t maxDynamicStorageBuffersPerPipelineLayout = 0;
    uint32_t maxSampledTexturesPerShaderStage = 0;
    uint32_t maxSamplersPerShaderStage = 0;
    uint32_t maxStorageBuffersPerShaderStage = 0;
    uint32_t maxStorageTexturesPerShaderStage = 0;
    uint32_t maxUniformBuffersPerShaderStage = 0;
    uint64_t maxUniformBufferBindingSize = 0;
    uint64_t maxStorageBufferBindingSize = 0;
    uint32_t minUniformBufferOffsetAlignment = 0;
    uint32_t minStorageBufferOffsetAlignment = 0;
    uint32_t maxVertexBuffers = 0;
    uint64_t maxBufferSize = 0;
    uint32_t maxVertexAttributes = 0;
    uint32_t maxVertexBufferArrayStride = 0;
    uint32_t maxInterStageShaderVariables = 0;
    uint32_t maxColorAttachments = 0;
    uint32_t maxColorAttachmentBytesPerSample = 0;
    uint32_t maxComputeWorkgroupStorageSize = 0;
    uint32_t maxComputeInvocationsPerWorkgroup = 0;
    uint32_t maxComputeWorkgroupSizeX = 0;
    uint32_t maxComputeWorkgroupSizeY = 0;
    uint32_t maxComputeWorkgroupSizeZ = 0;
    uint32_t maxComputeWorkgroupsPerDimension = 0;
    uint32_t maxImmediateSize = 0;
};

struct SubgroupMatrixConfig {
    SubgroupMatrixComponentType componentType = SubgroupMatrixComponentType::F16;
    SubgroupMatrixComponentType resultComponentType = SubgroupMatrixComponentType::F16;
    uint32_t M = 0, N = 0, K = 0;
};

struct AdapterPropertiesSubgroupMatrixConfigs : ChainedStruct {
    AdapterPropertiesSubgroupMatrixConfigs() { sType = SType::AdapterPropertiesSubgroupMatrixConfigs; }
    size_t configCount = 0;
    const SubgroupMatrixConfig * configs = nullptr;
};

struct AdapterInfo {
    const ChainedStruct * nextInChain = nullptr;
    StringView vendor, architecture, device, description;
    BackendType backendType = BackendType::WebGPU;
    AdapterType adapterType = AdapterType::Unknown;
    uint32_t vendorID = 0, deviceID = 0;
    uint32_t subgroupMinSize = 0, subgroupMaxSize = 0;
};

struct DawnTogglesDescriptor : ChainedStruct {
    DawnTogglesDescriptor() { sType = SType::DawnTogglesDescriptor; }
    size_t enabledToggleCount = 0;
    const char * const * enabledToggles = nullptr;
    size_t disabledToggleCount = 0;
    const char * const * disabledToggles = nullptr;
};

struct ShaderSourceWGSL : ChainedStruct {
    ShaderSourceWGSL() { sType = SType::ShaderSourceWGSL; }
    StringView code;
};

// ------------------------------------------------------------------------- the connection

namespace armless {

enum Op : uint8_t {
    Hello = 1, CreateBuffer = 2, WriteBuffer = 3, CreateShader = 4, CreatePipeline = 5,
    CreateBindGroup = 6, Dispatch = 7, CopyBuffer = 8, ReadBuffer = 9, Destroy = 10, Finish = 11,
};

constexpr uint32_t kVersion = 2;
constexpr uint32_t kFeatureF16 = 1, kFeatureSubgroups = 2, kFeaturePackedDot = 4;

[[noreturn]] inline void die(const char * what, const std::string & why = {}) {
    std::fprintf(stderr, "armless webgpu: %s%s%s\n", what, why.empty() ? "" : ": ", why.c_str());
    std::abort();
}

/** A frame's body. */
struct Enc {
    std::vector<uint8_t> b;
    explicit Enc(Op op) { b.push_back(op); }
    Enc & u32(uint32_t v) { for (int i = 0; i < 4; i++) b.push_back(uint8_t(v >> (8 * i))); return *this; }
    Enc & u64(uint64_t v) { for (int i = 0; i < 8; i++) b.push_back(uint8_t(v >> (8 * i))); return *this; }
    Enc & f64(double v) { uint64_t u; std::memcpy(&u, &v, 8); return u64(u); }
    Enc & str(std::string_view s) { u32(uint32_t(s.size())); b.insert(b.end(), s.begin(), s.end()); return *this; }
};

struct Dec {
    const uint8_t * p;
    size_t n;
    bool ok = true;
    bool need(size_t k) { if (n < k) ok = false; return ok; }
    uint32_t u32() { if (!need(4)) return 0; uint32_t v = 0; for (int i = 0; i < 4; i++) v |= uint32_t(p[i]) << (8 * i); p += 4; n -= 4; return v; }
    uint64_t u64() { if (!need(8)) return 0; uint64_t v = 0; for (int i = 0; i < 8; i++) v |= uint64_t(p[i]) << (8 * i); p += 8; n -= 8; return v; }
    std::string str() { uint32_t k = u32(); if (!need(k)) return {}; std::string s((const char *) p, k); p += k; n -= k; return s; }
};

/** What waits for the host's next reply: called with ok and the payload (or the error). */
using ReplyFn = std::function<void(bool ok, const uint8_t * data, size_t len)>;

struct Link {
    std::recursive_mutex lock;
    int fd = -1;
    std::vector<uint8_t> out;
    std::deque<std::pair<uint64_t, ReplyFn>> pending;  // future id, handler
    uint64_t next_future = 1;
    uint64_t done_upto = 0;  // every future up to this one has run
    uint32_t next_id = 1;
    std::vector<uint32_t> doomed;  // objects whose last reference went: Destroy on the next send
    std::shared_ptr<struct HostInfo> adapter_info;  // Hello's answer, asked once

    /** Connects once; false (and why, in `failed`) when there is no GPU to reach. */
    bool try_connect() {
        if (fd >= 0) return true;
        if (!failed.empty()) return false;
        std::string addr = "gpu.internal:7070";
        if (const char * e = std::getenv("ARMLESS_GPU")) addr = e;
        auto colon = addr.rfind(':');
        std::string host = addr.substr(0, colon), port = colon == std::string::npos ? "7070" : addr.substr(colon + 1);
        addrinfo hints{}, * res = nullptr;
        hints.ai_socktype = SOCK_STREAM;
        if (int rc = getaddrinfo(host.c_str(), port.c_str(), &hints, &res); rc != 0) {
            failed = "can't resolve " + addr + ": " + gai_strerror(rc);
            return false;
        }
        for (addrinfo * a = res; a; a = a->ai_next) {
            fd = ::socket(a->ai_family, a->ai_socktype, a->ai_protocol);
            if (fd < 0) continue;
            if (::connect(fd, a->ai_addr, a->ai_addrlen) == 0) break;
            ::close(fd);
            fd = -1;
        }
        freeaddrinfo(res);
        if (fd < 0) {
            failed = "no GPU at " + addr + " (the machine has none, or the page no WebGPU)";
            return false;
        }
        int one = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
        return true;
    }
    std::string failed;
    void connect() {
        if (!try_connect()) die("the GPU went away", failed);
    }

    void write_all(const uint8_t * p, size_t n) {
        while (n) {
            ssize_t k = ::send(fd, p, n, MSG_NOSIGNAL);
            if (k < 0 && errno == EINTR) continue;
            if (k <= 0) die("the GPU connection broke", std::strerror(errno));
            p += k;
            n -= size_t(k);
        }
    }
    void read_all(uint8_t * p, size_t n) {
        while (n) {
            ssize_t k = ::recv(fd, p, n, 0);
            if (k < 0 && errno == EINTR) continue;
            if (k <= 0) die("the GPU connection broke", k == 0 ? "closed" : std::strerror(errno));
            p += k;
            n -= size_t(k);
        }
    }
    /** Reads a whole reply frame; false if the connection ends or fails (no abort). */
    bool try_read_frame(std::vector<uint8_t> & body) {
        auto get = [&](uint8_t * p, size_t n) {
            while (n) {
                ssize_t k = ::recv(fd, p, n, 0);
                if (k < 0 && errno == EINTR) continue;
                if (k <= 0) return false;
                p += k;
                n -= size_t(k);
            }
            return true;
        };
        uint8_t h[4];
        if (!get(h, 4)) return false;
        size_t n = size_t(h[0]) | size_t(h[1]) << 8 | size_t(h[2]) << 16 | size_t(h[3]) << 24;
        if (n > (256u << 20)) return false;
        body.resize(n);
        return get(body.data(), n);
    }
    void flush() {
        if (!out.empty()) {
            write_all(out.data(), out.size());
            out.clear();
        }
    }
    void frame_header(size_t n) {
        uint8_t h[4] = { uint8_t(n), uint8_t(n >> 8), uint8_t(n >> 16), uint8_t(n >> 24) };
        out.insert(out.end(), h, h + 4);
    }
    void reap() {
        while (!doomed.empty()) {
            uint32_t id = doomed.back();
            doomed.pop_back();
            send_now(Enc(Destroy).u32(id));
        }
    }
    void send_now(const Enc & e) {
        frame_header(e.b.size());
        out.insert(out.end(), e.b.begin(), e.b.end());
        if (out.size() > (1 << 20)) flush();
    }
    /** Sends a command (buffered). */
    void send(const Enc & e) {
        connect();
        reap();
        send_now(e);
    }
    /** A command and a payload too big to copy twice (a buffer write). */
    void send(const Enc & e, const void * data, size_t n) {
        connect();
        reap();
        frame_header(e.b.size() + n);
        out.insert(out.end(), e.b.begin(), e.b.end());
        if (n > (64 << 10)) {
            flush();
            write_all((const uint8_t *) data, n);
        } else {
            out.insert(out.end(), (const uint8_t *) data, (const uint8_t *) data + n);
            if (out.size() > (1 << 20)) flush();
        }
    }
    /** Sends a command whose reply `fn` takes; the returned future completes with it. */
    Future ask(const Enc & e, ReplyFn fn) {
        send(e);
        uint64_t id = next_future++;
        pending.emplace_back(id, std::move(fn));
        return { id };
    }
    /** A future that is already complete. */
    Future done() {
        uint64_t id = next_future++;
        if (pending.empty()) done_upto = id;
        else pending.emplace_back(id, nullptr);
        return { id };
    }
    bool completed(uint64_t id) {
        return id <= done_upto;
    }
    /** Reads the next reply and runs what waited for it. */
    void pump() {
        flush();
        while (!pending.empty() && !pending.front().second) {
            done_upto = pending.front().first;
            pending.pop_front();
        }
        if (pending.empty()) return;
        uint8_t h[4];
        read_all(h, 4);
        size_t n = size_t(h[0]) | size_t(h[1]) << 8 | size_t(h[2]) << 16 | size_t(h[3]) << 24;
        std::vector<uint8_t> body(n);
        read_all(body.data(), n);
        auto [id, fn] = std::move(pending.front());
        pending.pop_front();
        bool ok = n > 0 && body[0] == 0;
        if (fn) fn(ok, body.data() + (n ? 1 : 0), n ? n - 1 : 0);
        done_upto = id;
        while (!pending.empty() && !pending.front().second) {
            done_upto = pending.front().first;
            pending.pop_front();
        }
    }
    void wait(uint64_t id) {
        while (!completed(id)) {
            if (pending.empty()) die("waiting on a future nothing will complete");
            pump();
        }
    }
    uint32_t id() { return next_id++; }
    void release(uint32_t id) {
        std::lock_guard<std::recursive_mutex> g(lock);
        if (fd >= 0) doomed.push_back(id);
    }
};

inline Link & link() {
    static Link l;
    return l;
}

using Guard = std::lock_guard<std::recursive_mutex>;

/** A host object: its id, released when the last reference goes. */
struct Obj {
    uint32_t id = 0;
    virtual ~Obj() { if (id) link().release(id); }
};

/** What the host said about its GPU. */
struct HostInfo {
    std::string adapter, backend, vendor, architecture, device, description;
    uint32_t features = 0, subgroup_min = 0, subgroup_max = 0;
    Limits limits{};
};

inline void set_limit(Limits & l, const std::string & k, uint64_t v) {
#define ARMLESS_LIMIT(name) if (k == #name) { l.name = decltype(l.name)(v); return; }
    ARMLESS_LIMIT(maxTextureDimension1D) ARMLESS_LIMIT(maxTextureDimension2D) ARMLESS_LIMIT(maxTextureDimension3D)
    ARMLESS_LIMIT(maxTextureArrayLayers) ARMLESS_LIMIT(maxBindGroups) ARMLESS_LIMIT(maxBindGroupsPlusVertexBuffers)
    ARMLESS_LIMIT(maxBindingsPerBindGroup) ARMLESS_LIMIT(maxDynamicUniformBuffersPerPipelineLayout)
    ARMLESS_LIMIT(maxDynamicStorageBuffersPerPipelineLayout) ARMLESS_LIMIT(maxSampledTexturesPerShaderStage)
    ARMLESS_LIMIT(maxSamplersPerShaderStage) ARMLESS_LIMIT(maxStorageBuffersPerShaderStage)
    ARMLESS_LIMIT(maxStorageTexturesPerShaderStage) ARMLESS_LIMIT(maxUniformBuffersPerShaderStage)
    ARMLESS_LIMIT(maxUniformBufferBindingSize) ARMLESS_LIMIT(maxStorageBufferBindingSize)
    ARMLESS_LIMIT(minUniformBufferOffsetAlignment) ARMLESS_LIMIT(minStorageBufferOffsetAlignment)
    ARMLESS_LIMIT(maxVertexBuffers) ARMLESS_LIMIT(maxBufferSize) ARMLESS_LIMIT(maxVertexAttributes)
    ARMLESS_LIMIT(maxVertexBufferArrayStride) ARMLESS_LIMIT(maxInterStageShaderVariables)
    ARMLESS_LIMIT(maxColorAttachments) ARMLESS_LIMIT(maxColorAttachmentBytesPerSample)
    ARMLESS_LIMIT(maxComputeWorkgroupStorageSize) ARMLESS_LIMIT(maxComputeInvocationsPerWorkgroup)
    ARMLESS_LIMIT(maxComputeWorkgroupSizeX) ARMLESS_LIMIT(maxComputeWorkgroupSizeY)
    ARMLESS_LIMIT(maxComputeWorkgroupSizeZ) ARMLESS_LIMIT(maxComputeWorkgroupsPerDimension)
#undef ARMLESS_LIMIT
}

inline bool parse_hello(const uint8_t * p, size_t n, HostInfo & h) {
    Dec d{ p, n };
    d.u32();  // version
    h.adapter = d.str();
    h.backend = d.str();
    h.limits.maxBufferSize = d.u64();
    h.limits.maxStorageBufferBindingSize = d.u64();
    h.limits.maxComputeWorkgroupsPerDimension = d.u32();
    if (!d.ok) return false;
    if (d.n == 0) return true;  // a version 1 host
    h.features = d.u32();
    h.subgroup_min = d.u32();
    h.subgroup_max = d.u32();
    h.vendor = d.str();
    h.architecture = d.str();
    h.device = d.str();
    h.description = d.str();
    uint32_t k = d.u32();
    for (uint32_t i = 0; i < k && d.ok; i++) {
        std::string name = d.str();
        uint64_t v = d.u64();
        set_limit(h.limits, name, v);
    }
    return d.ok;
}

struct DeviceState;

}  // namespace armless

// ------------------------------------------------------------------------------- handles

template <class I> struct Handle {
    std::shared_ptr<I> p;
    Handle() = default;
    Handle(std::nullptr_t) {}
    explicit Handle(std::shared_ptr<I> i) : p(std::move(i)) {}
    explicit operator bool() const { return bool(p); }
    bool operator==(std::nullptr_t) const { return !p; }
    bool operator!=(std::nullptr_t) const { return bool(p); }
    bool operator==(const Handle & o) const { return p == o.p; }
    I * Get() const { return p.get(); }
};

struct BufferImpl;
struct ShaderModuleImpl;
struct BindGroupLayoutImpl;
struct ComputePipelineImpl;
struct BindGroupImpl;
struct QuerySetImpl;
struct CommandBufferImpl;
struct CommandEncoderImpl;
struct ComputePassEncoderImpl;
struct QueueImpl;
struct DeviceImpl;
struct AdapterImpl;
struct InstanceImpl;
struct PipelineLayoutImpl {};

struct ConstantEntry {
    const ChainedStruct * nextInChain = nullptr;
    StringView key;
    double value = 0;
};

struct Buffer : Handle<BufferImpl> {
    using Handle::Handle;
    template <class F> Future MapAsync(MapMode mode, size_t offset, size_t size, CallbackMode, F callback) const;
    const void * GetConstMappedRange(size_t offset = 0, size_t size = kWholeMapSize) const;
    void * GetMappedRange(size_t offset = 0, size_t size = kWholeMapSize) const;
    void Unmap() const;
    void Destroy() const;
    uint64_t GetSize() const;
    BufferUsage GetUsage() const;
};

struct ShaderModule : Handle<ShaderModuleImpl> { using Handle::Handle; };
struct BindGroupLayout : Handle<BindGroupLayoutImpl> { using Handle::Handle; };
struct PipelineLayout : Handle<PipelineLayoutImpl> { using Handle::Handle; };
struct BindGroup : Handle<BindGroupImpl> { using Handle::Handle; };
struct QuerySet : Handle<QuerySetImpl> {
    using Handle::Handle;
    void Destroy() const {}
};

struct ComputePipeline : Handle<ComputePipelineImpl> {
    using Handle::Handle;
    BindGroupLayout GetBindGroupLayout(uint32_t group) const;
};

struct BufferDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
    BufferUsage usage = BufferUsage::None;
    uint64_t size = 0;
    bool mappedAtCreation = false;
};

struct ShaderModuleDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
};

struct ComputeState {
    const ChainedStruct * nextInChain = nullptr;
    ShaderModule module;
    StringView entryPoint;
    size_t constantCount = 0;
    const ConstantEntry * constants = nullptr;
};

struct ComputePipelineDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
    PipelineLayout layout;
    ComputeState compute;
};

struct BindGroupEntry {
    const ChainedStruct * nextInChain = nullptr;
    uint32_t binding = 0;
    Buffer buffer;
    uint64_t offset = 0;
    uint64_t size = kWholeSize;
};

struct BindGroupDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
    BindGroupLayout layout;
    size_t entryCount = 0;
    const BindGroupEntry * entries = nullptr;
};

struct PassTimestampWrites {
    const ChainedStruct * nextInChain = nullptr;
    QuerySet querySet;
    uint32_t beginningOfPassWriteIndex = UINT32_MAX;
    uint32_t endOfPassWriteIndex = UINT32_MAX;
};

struct ComputePassDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
    const PassTimestampWrites * timestampWrites = nullptr;
};

struct CommandEncoderDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
};
struct CommandBufferDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
};

struct QuerySetDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
    QueryType type = QueryType::Timestamp;
    uint32_t count = 0;
};

struct CommandBuffer : Handle<CommandBufferImpl> { using Handle::Handle; };

struct ComputePassEncoder : Handle<ComputePassEncoderImpl> {
    using Handle::Handle;
    void SetPipeline(const ComputePipeline & p) const;
    void SetBindGroup(uint32_t group, const BindGroup & bg, size_t dynamicOffsetCount = 0, const uint32_t * dynamicOffsets = nullptr) const;
    void DispatchWorkgroups(uint32_t x, uint32_t y = 1, uint32_t z = 1) const;
    void End() const {}
};

struct CommandEncoder : Handle<CommandEncoderImpl> {
    using Handle::Handle;
    ComputePassEncoder BeginComputePass(const ComputePassDescriptor * desc = nullptr) const;
    void CopyBufferToBuffer(const Buffer & src, uint64_t srcOffset, const Buffer & dst, uint64_t dstOffset, uint64_t size) const;
    void ResolveQuerySet(const QuerySet &, uint32_t, uint32_t, const Buffer &, uint64_t) const {}
    CommandBuffer Finish(const CommandBufferDescriptor * desc = nullptr) const;
};

struct Queue : Handle<QueueImpl> {
    using Handle::Handle;
    void Submit(size_t count, const CommandBuffer * commands) const;
    void WriteBuffer(const Buffer & buffer, uint64_t offset, const void * data, size_t size) const;
    template <class F> Future OnSubmittedWorkDone(CallbackMode, F callback) const;
};

struct Device : Handle<DeviceImpl> {
    using Handle::Handle;
    Buffer CreateBuffer(const BufferDescriptor * desc) const;
    ShaderModule CreateShaderModule(const ShaderModuleDescriptor * desc) const;
    ComputePipeline CreateComputePipeline(const ComputePipelineDescriptor * desc) const;
    BindGroup CreateBindGroup(const BindGroupDescriptor * desc) const;
    CommandEncoder CreateCommandEncoder(const CommandEncoderDescriptor * desc = nullptr) const;
    QuerySet CreateQuerySet(const QuerySetDescriptor *) const { return QuerySet(std::make_shared<QuerySetImpl>()); }
    Queue GetQueue() const;
    Status GetLimits(Limits * limits) const;
    bool HasFeature(FeatureName f) const;
    void Destroy() const;
};

struct DeviceDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    StringView label;
    size_t requiredFeatureCount = 0;
    const FeatureName * requiredFeatures = nullptr;
    const Limits * requiredLimits = nullptr;
    std::function<void(const Device &, DeviceLostReason, StringView)> lost;
    std::function<void(const Device &, ErrorType, StringView)> error;
    template <class F> void SetDeviceLostCallback(CallbackMode, F f) { lost = f; }
    template <class F> void SetUncapturedErrorCallback(F f) { error = f; }
};

struct RequestAdapterOptions {
    const ChainedStruct * nextInChain = nullptr;
    FeatureLevel featureLevel = FeatureLevel::Core;
    PowerPreference powerPreference = PowerPreference::Undefined;
    bool forceFallbackAdapter = false;
    BackendType backendType = BackendType::Undefined;
};

struct Adapter : Handle<AdapterImpl> {
    using Handle::Handle;
    Status GetLimits(Limits * limits) const;
    Status GetInfo(AdapterInfo * info) const;
    bool HasFeature(FeatureName f) const;
    template <class F> Future RequestDevice(const DeviceDescriptor * desc, CallbackMode, F callback) const;
};

struct InstanceDescriptor {
    const ChainedStruct * nextInChain = nullptr;
    size_t requiredFeatureCount = 0;
    const InstanceFeatureName * requiredFeatures = nullptr;
    const Limits * requiredLimits = nullptr;
};

struct Instance : Handle<InstanceImpl> {
    using Handle::Handle;
    template <class F> Future RequestAdapter(const RequestAdapterOptions * options, CallbackMode, F callback) const;
    WaitStatus WaitAny(Future f, uint64_t timeoutNS) const;
    WaitStatus WaitAny(size_t count, FutureWaitInfo * infos, uint64_t timeoutNS) const;
    bool HasWGSLLanguageFeature(WGSLLanguageFeatureName f) const;
    void ProcessEvents() const;
};

// ------------------------------------------------------------------------- implementations

struct InstanceImpl {};

struct AdapterImpl {
    armless::HostInfo info;
};

struct QueueImpl {
    std::weak_ptr<DeviceImpl> device;
};

struct DeviceImpl {
    std::shared_ptr<AdapterImpl> adapter;
    std::shared_ptr<QueueImpl> queue = std::make_shared<QueueImpl>();
    std::function<void(const Device &, DeviceLostReason, StringView)> lost;
    std::function<void(const Device &, ErrorType, StringView)> error;
    bool destroyed = false;
    std::weak_ptr<DeviceImpl> self;
    void report(const uint8_t * p, size_t n) {
        std::string msg((const char *) p, n);
        if (error) error(Device(self.lock()), ErrorType::Validation, StringView(msg));
        else std::fprintf(stderr, "armless webgpu: device error: %s\n", msg.c_str());
    }
};

struct BufferImpl : armless::Obj {
    uint64_t size = 0;
    BufferUsage usage = BufferUsage::None;
    std::vector<uint8_t> shadow;  // what a map read brought, at its offset in the buffer
    bool destroyed = false;
};

struct ShaderModuleImpl : armless::Obj {};

struct BindGroupLayoutImpl {
    std::shared_ptr<ComputePipelineImpl> pipeline;
    uint32_t group = 0;
};

struct ComputePipelineImpl : armless::Obj {};

struct BindGroupImpl : armless::Obj {
    std::vector<Buffer> buffers;  // kept until the host has it
};

struct QuerySetImpl {};

/** A recorded command: a dispatch or a copy, kept with what it uses. */
struct Recorded {
    armless::Enc enc{ armless::Dispatch };
    std::vector<std::shared_ptr<void>> keep;
};

struct CommandBufferImpl {
    std::vector<Recorded> commands;
};

struct CommandEncoderImpl {
    std::vector<Recorded> commands;
};

struct ComputePassEncoderImpl {
    std::shared_ptr<CommandEncoderImpl> encoder;
    std::shared_ptr<ComputePipelineImpl> pipeline;
    std::vector<std::pair<uint32_t, std::shared_ptr<BindGroupImpl>>> groups;
};

// Instance

inline Instance CreateInstance(const InstanceDescriptor * = nullptr) {
    return Instance(std::make_shared<InstanceImpl>());
}

template <class F> Future Instance::RequestAdapter(const RequestAdapterOptions *, CallbackMode, F callback) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    // No GPU to reach: no adapter, as on a machine without one (the program goes on without).
    // Hello is answered here and now, so a connection that is taken and then closed (a host with
    // no GPU behind the name) is no adapter either, not a broken connection later
    auto unavailable = [&](const std::string & why) {
        if (l.fd >= 0) ::close(l.fd);
        l.fd = -1;
        l.out.clear();
        l.failed = why;
        callback(RequestAdapterStatus::Unavailable, Adapter(), StringView(why));
        return l.done();
    };
    if (!l.try_connect()) return unavailable(l.failed);
    if (!l.adapter_info) {
        l.send(armless::Enc(armless::Hello).u32(armless::kVersion));
        l.flush();
        std::vector<uint8_t> reply;
        auto info = std::make_shared<armless::HostInfo>();
        if (!l.try_read_frame(reply)) return unavailable("the GPU's host closed the connection (no GPU there)");
        if (reply.empty() || reply[0] != 0 || !armless::parse_hello(reply.data() + 1, reply.size() - 1, *info)) {
            return unavailable("the GPU's host refused: " + std::string(reply.begin() + (reply.empty() ? 0 : 1), reply.end()));
        }
        l.adapter_info = info;
    }
    auto a = std::make_shared<AdapterImpl>();
    a->info = *l.adapter_info;
    callback(RequestAdapterStatus::Success, Adapter(a), StringView(""));
    return l.done();
}

inline WaitStatus Instance::WaitAny(Future f, uint64_t) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    l.wait(f.id);
    return WaitStatus::Success;
}

inline WaitStatus Instance::WaitAny(size_t count, FutureWaitInfo * infos, uint64_t timeout) const {
    if (count == 0) return WaitStatus::Success;
    auto & l = armless::link();
    armless::Guard g(l.lock);
    uint64_t lowest = UINT64_MAX;
    for (size_t i = 0; i < count; i++) lowest = std::min(lowest, infos[i].future.id);
    if (timeout != 0) l.wait(lowest);
    bool any = false;
    for (size_t i = 0; i < count; i++) any |= (infos[i].completed = l.completed(infos[i].future.id));
    return any ? WaitStatus::Success : WaitStatus::TimedOut;
}

inline bool Instance::HasWGSLLanguageFeature(WGSLLanguageFeatureName) const {
    // Answered once the adapter is known (Adapter::HasFeature has the host's word); before that,
    // claim nothing
    return false;
}

inline void Instance::ProcessEvents() const {}

// Adapter

inline Status Adapter::GetLimits(Limits * limits) const {
    *limits = p->info.limits;
    return Status::Success;
}

inline Status Adapter::GetInfo(AdapterInfo * info) const {
    auto & h = p->info;
    info->vendor = StringView(h.vendor);
    info->architecture = StringView(h.architecture);
    info->device = StringView(h.device.empty() ? h.adapter : h.device);
    info->description = StringView(h.description);
    info->backendType = BackendType::WebGPU;
    info->subgroupMinSize = h.subgroup_min;
    info->subgroupMaxSize = h.subgroup_max;
    for (auto * c = info->nextInChain; c; c = c->nextInChain) {
        if (c->sType == SType::AdapterPropertiesSubgroupMatrixConfigs) {
            auto * m = const_cast<AdapterPropertiesSubgroupMatrixConfigs *>(static_cast<const AdapterPropertiesSubgroupMatrixConfigs *>(c));
            m->configCount = 0;
            m->configs = nullptr;
        }
    }
    return Status::Success;
}

inline bool Adapter::HasFeature(FeatureName f) const {
    switch (f) {
        case FeatureName::ShaderF16: return p->info.features & armless::kFeatureF16;
        case FeatureName::Subgroups: return p->info.features & armless::kFeatureSubgroups;
        case FeatureName::ImplicitDeviceSynchronization: return true;
        default: return false;
    }
}

template <class F> Future Adapter::RequestDevice(const DeviceDescriptor * desc, CallbackMode, F callback) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    auto d = std::make_shared<DeviceImpl>();
    d->adapter = p;
    d->self = d;
    d->queue->device = d;
    if (desc) {
        d->lost = desc->lost;
        d->error = desc->error;
    }
    // The host makes the device with the connection: nothing to ask
    callback(RequestDeviceStatus::Success, Device(d), StringView(""));
    return l.done();
}

// Device

inline Buffer Device::CreateBuffer(const BufferDescriptor * desc) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    auto b = std::make_shared<BufferImpl>();
    b->id = l.id();
    b->size = desc->size;
    b->usage = desc->usage;
    l.send(armless::Enc(armless::CreateBuffer).u32(b->id).u64(desc->size).u32(uint32_t(desc->usage)));
    return Buffer(b);
}

inline ShaderModule Device::CreateShaderModule(const ShaderModuleDescriptor * desc) const {
    std::string_view code;
    for (auto * c = desc->nextInChain; c; c = c->nextInChain) {
        if (c->sType == SType::ShaderSourceWGSL) code = static_cast<const ShaderSourceWGSL *>(c)->code;
    }
    auto & l = armless::link();
    armless::Guard g(l.lock);
    auto m = std::make_shared<ShaderModuleImpl>();
    m->id = l.id();
    l.send(armless::Enc(armless::CreateShader).u32(m->id).str(code));
    return ShaderModule(m);
}

inline ComputePipeline Device::CreateComputePipeline(const ComputePipelineDescriptor * desc) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    auto pl = std::make_shared<ComputePipelineImpl>();
    pl->id = l.id();
    std::string_view entry = desc->compute.entryPoint;
    armless::Enc e(armless::CreatePipeline);
    e.u32(pl->id).u32(desc->compute.module.p->id).str(entry.empty() ? "main" : entry);
    e.u32(uint32_t(desc->compute.constantCount));
    for (size_t i = 0; i < desc->compute.constantCount; i++) {
        e.str(desc->compute.constants[i].key).f64(desc->compute.constants[i].value);
    }
    l.send(e);
    return ComputePipeline(pl);
}

inline BindGroupLayout ComputePipeline::GetBindGroupLayout(uint32_t group) const {
    auto bgl = std::make_shared<BindGroupLayoutImpl>();
    bgl->pipeline = p;
    bgl->group = group;
    return BindGroupLayout(bgl);
}

inline BindGroup Device::CreateBindGroup(const BindGroupDescriptor * desc) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    auto bg = std::make_shared<BindGroupImpl>();
    bg->id = l.id();
    armless::Enc e(armless::CreateBindGroup);
    e.u32(bg->id).u32(desc->layout.p->pipeline->id).u32(desc->layout.p->group).u32(uint32_t(desc->entryCount));
    for (size_t i = 0; i < desc->entryCount; i++) {
        const auto & en = desc->entries[i];
        uint64_t size = en.size == kWholeSize ? 0 : en.size;
        e.u32(en.binding).u32(en.buffer.p->id).u64(en.offset).u64(size);
        bg->buffers.push_back(en.buffer);
    }
    l.send(e);
    return BindGroup(bg);
}

inline CommandEncoder Device::CreateCommandEncoder(const CommandEncoderDescriptor *) const {
    return CommandEncoder(std::make_shared<CommandEncoderImpl>());
}

inline Queue Device::GetQueue() const { return Queue(p->queue); }

inline Status Device::GetLimits(Limits * limits) const {
    *limits = p->adapter->info.limits;
    return Status::Success;
}

inline bool Device::HasFeature(FeatureName f) const { return Adapter(p->adapter).HasFeature(f); }

inline void Device::Destroy() const {
    if (p->destroyed) return;
    p->destroyed = true;
    if (p->lost) p->lost(*this, DeviceLostReason::Destroyed, StringView("destroyed"));
}

// Buffers

template <class F> Future Buffer::MapAsync(MapMode mode, size_t offset, size_t size, CallbackMode, F callback) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    auto b = p;
    if (size == kWholeMapSize) size = size_t(b->size - offset);
    if (mode != MapMode::Read) {
        // Mapping for writing: the guest fills the copy, Unmap would send it. Not needed so far.
        callback(MapAsyncStatus::Error, StringView("armless webgpu: only MapMode::Read is supported"));
        return l.done();
    }
    return l.ask(armless::Enc(armless::ReadBuffer).u32(b->id).u64(offset).u64(size),
                 [b, offset, size, callback](bool ok, const uint8_t * data, size_t n) mutable {
                     if (!ok) {
                         std::string msg((const char *) data, n);
                         callback(MapAsyncStatus::Error, StringView(msg));
                         return;
                     }
                     if (b->shadow.size() < b->size) b->shadow.resize(size_t(b->size));
                     std::memcpy(b->shadow.data() + offset, data, std::min(n, size));
                     callback(MapAsyncStatus::Success, StringView(""));
                 });
}

inline const void * Buffer::GetConstMappedRange(size_t offset, size_t) const {
    if (p->shadow.empty()) return nullptr;
    return p->shadow.data() + offset;
}

inline void * Buffer::GetMappedRange(size_t offset, size_t) const {
    if (p->shadow.empty()) return nullptr;
    return p->shadow.data() + offset;
}

inline void Buffer::Unmap() const {}

inline void Buffer::Destroy() const {
    // Freed on the host now (bind groups and commands that hold it keep what they need there)
    auto & l = armless::link();
    armless::Guard g(l.lock);
    if (p->destroyed) return;
    p->destroyed = true;
    if (p->id && l.fd >= 0) l.send(armless::Enc(armless::Destroy).u32(p->id));
    p->id = 0;
    p->shadow = {};
}

inline uint64_t Buffer::GetSize() const { return p->size; }
inline BufferUsage Buffer::GetUsage() const { return p->usage; }

// Encoding

inline ComputePassEncoder CommandEncoder::BeginComputePass(const ComputePassDescriptor *) const {
    auto pass = std::make_shared<ComputePassEncoderImpl>();
    pass->encoder = p;
    return ComputePassEncoder(pass);
}

inline void ComputePassEncoder::SetPipeline(const ComputePipeline & pl) const { p->pipeline = pl.p; }

inline void ComputePassEncoder::SetBindGroup(uint32_t group, const BindGroup & bg, size_t, const uint32_t *) const {
    for (auto & g : p->groups) {
        if (g.first == group) {
            g.second = bg.p;
            return;
        }
    }
    p->groups.emplace_back(group, bg.p);
}

inline void ComputePassEncoder::DispatchWorkgroups(uint32_t x, uint32_t y, uint32_t z) const {
    Recorded r;
    r.enc.u32(p->pipeline->id).u32(uint32_t(p->groups.size()));
    r.keep.push_back(p->pipeline);
    for (auto & [g, bg] : p->groups) {
        r.enc.u32(g).u32(bg->id);
        r.keep.push_back(bg);
    }
    r.enc.u32(x).u32(y).u32(z);
    p->encoder->commands.push_back(std::move(r));
}

inline void CommandEncoder::CopyBufferToBuffer(const Buffer & src, uint64_t so, const Buffer & dst, uint64_t dof, uint64_t size) const {
    Recorded r;
    r.enc = armless::Enc(armless::CopyBuffer);
    r.enc.u32(src.p->id).u64(so).u32(dst.p->id).u64(dof).u64(size);
    r.keep.push_back(src.p);
    r.keep.push_back(dst.p);
    p->commands.push_back(std::move(r));
}

inline CommandBuffer CommandEncoder::Finish(const CommandBufferDescriptor *) const {
    auto cb = std::make_shared<CommandBufferImpl>();
    cb->commands = std::move(p->commands);
    p->commands.clear();
    return CommandBuffer(cb);
}

// Queue

inline void Queue::Submit(size_t count, const CommandBuffer * commands) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    for (size_t i = 0; i < count; i++) {
        for (auto & r : commands[i].p->commands) l.send(r.enc);
        commands[i].p->commands.clear();
    }
}

inline void Queue::WriteBuffer(const Buffer & buffer, uint64_t offset, const void * data, size_t size) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    l.send(armless::Enc(armless::WriteBuffer).u32(buffer.p->id).u64(offset), data, size);
}

template <class F> Future Queue::OnSubmittedWorkDone(CallbackMode, F callback) const {
    auto & l = armless::link();
    armless::Guard g(l.lock);
    std::weak_ptr<DeviceImpl> dev = p->device;
    return l.ask(armless::Enc(armless::Finish), [dev, callback](bool ok, const uint8_t * data, size_t n) mutable {
        if (!ok) {
            if (auto d = dev.lock()) d->report(data, n);
            std::string msg((const char *) data, n);
            callback(QueueWorkDoneStatus::Error, StringView(msg));
            return;
        }
        callback(QueueWorkDoneStatus::Success, StringView(""));
    });
}

}  // namespace wgpu
