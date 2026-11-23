//! The host side on wgpu: one device per connection, running what the guest sends.
//!
//! Dispatches and copies go into one command encoder, submitted when something needs the GPU
//! to have caught up (a write, a read, `Finish`). A write is queued with `Queue::write_buffer`,
//! which runs before the next submission, so the encoder is submitted first to keep the guest's
//! order.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::wire::{
    read_frame, write_frame, Dec, Enc, Op, ERR, FEATURE_F16, FEATURE_SUBGROUPS, MAX_FRAME, OK,
    VERSION,
};

/// A GPU the connections share (each gets its own device on it). One `Gpu` is one guest's: its
/// [`Limits`] are for all of its connections together, so a guest that opens more of them gets no
/// more of the GPU.
pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    limits: Limits,
    used: Used,
}

/// What a guest (all its connections together) may take of the GPU.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes of buffers alive at once, the host's staging for reads included. Hello reports no
    /// larger maxBufferSize.
    pub memory: u64,
    /// Objects (buffers, shaders, pipelines, bind groups) alive at once
    pub objects: usize,
    /// Connections open at once (each is a device and a thread); one more is closed at once
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { memory: 4 << 30, objects: 1 << 20, connections: 16 }
    }
}

/// What the guest's connections hold of its [`Limits`] now.
#[derive(Default)]
struct Used {
    memory: AtomicU64,
    objects: AtomicU64,
    connections: AtomicU64,
}

/// Takes `n` of `used` if that stays within `max`; otherwise what is in use.
fn take(used: &AtomicU64, n: u64, max: u64) -> Result<(), u64> {
    let mut u = used.load(Ordering::Acquire);
    loop {
        let t = u.checked_add(n).filter(|&t| t <= max).ok_or(u)?;
        match used.compare_exchange_weak(u, t, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(now) => u = now,
        }
    }
}

impl Limits {
    /// The defaults, or ARMLESS_GPU_MEMORY_MB's budget.
    pub fn from_env() -> Limits {
        let mut l = Limits::default();
        if let Some(mb) = std::env::var("ARMLESS_GPU_MEMORY_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
            l.memory = mb << 20;
        }
        l
    }
}

impl Gpu {
    /// The system's preferred adapter, or `None` if it has none wgpu can use. Its guest gets
    /// `Limits::from_env()`.
    pub fn new() -> Option<Gpu> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        Some(Gpu { instance, adapter, limits: Limits::from_env(), used: Used::default() })
    }

    /// The same GPU, its guest limited to `limits`.
    pub fn with_limits(mut self, limits: Limits) -> Gpu {
        self.limits = limits;
        self
    }

    pub fn adapter_info(&self) -> wgpu::AdapterInfo {
        self.adapter.get_info()
    }

    /// Takes `n` bytes of the guest's memory, or says how far over it would go.
    fn take_memory(&self, n: u64) -> io::Result<()> {
        let max = self.limits.memory;
        take(&self.used.memory, n, max).map_err(|u| {
                let mib = |n: u64| n as f64 / (1u64 << 20) as f64;
                io::Error::other(format!(
                    "out of GPU memory: {:.1} MiB more, with {:.1} of this guest's {:.1} MiB in use",
                    mib(n),
                    mib(u),
                    mib(max)
                ))
            })
    }

    fn give_memory(&self, n: u64) {
        self.used.memory.fetch_sub(n, Ordering::AcqRel);
    }

    /// Takes a place among the guest's open connections, or says there is none.
    fn take_connection(&self) -> io::Result<()> {
        if take(&self.used.connections, 1, self.limits.connections as u64).is_ok() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("too many GPU connections (this guest may have {} open)", self.limits.connections),
            ))
        }
    }

    /// Serves one guest connection until it closes (refused at once if the guest has as many
    /// open as it may).
    pub fn serve(&self, stream: impl Read + Write) -> io::Result<()> {
        self.take_connection()?;
        self.serve_taken(stream)
    }

    /// Serves a connection whose place was taken, giving it back when it ends.
    fn serve_taken(&self, mut stream: impl Read + Write) -> io::Result<()> {
        let _ = &self.instance;
        struct Open<'a>(&'a AtomicU64);
        impl Drop for Open<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _open = Open(&self.used.connections);
        let (device, queue) =
            pollster::block_on(self.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("armless guest"),
                required_limits: self.adapter.limits(),
                required_features: self.adapter.features() & offered_features(),
                ..Default::default()
            }))
            .map_err(io::Error::other)?;
        let mut s = Session::new(self, device, queue);
        while let Some(frame) = read_frame(&mut stream)? {
            if let Some(reply) = s.handle(&frame) {
                write_frame(&mut stream, &reply)?;
                stream.flush()?;
            }
        }
        Ok(())
    }
}

/// What a device asks of the adapter where it has it: what guests' shaders may `enable`.
fn offered_features() -> wgpu::Features {
    // (Not SUBGROUP: naga does not take WGSL's `enable subgroups;` yet)
    wgpu::Features::SHADER_F16
}

/// A guest's buffer as the host makes it. A buffer the guest maps is only ever read or written
/// through the host (ReadBuffer, WriteBuffer), so its map usages become the copies those need.
pub(crate) fn host_usage(usage: u32) -> u32 {
    use crate::wire::usage::*;
    let mut u = usage;
    if u & MAP_READ != 0 {
        u = (u & !MAP_READ) | COPY_SRC;
    }
    if u & MAP_WRITE != 0 {
        u = (u & !MAP_WRITE) | COPY_DST;
    }
    u
}

/// The device's limits by their WebGPU names, for Hello.
fn limit_pairs(l: &wgpu::Limits) -> Vec<(&'static str, u64)> {
    vec![
        ("maxTextureDimension1D", l.max_texture_dimension_1d as u64),
        ("maxTextureDimension2D", l.max_texture_dimension_2d as u64),
        ("maxTextureDimension3D", l.max_texture_dimension_3d as u64),
        ("maxTextureArrayLayers", l.max_texture_array_layers as u64),
        ("maxBindGroups", l.max_bind_groups as u64),
        ("maxBindingsPerBindGroup", l.max_bindings_per_bind_group as u64),
        ("maxDynamicUniformBuffersPerPipelineLayout", l.max_dynamic_uniform_buffers_per_pipeline_layout as u64),
        ("maxDynamicStorageBuffersPerPipelineLayout", l.max_dynamic_storage_buffers_per_pipeline_layout as u64),
        ("maxSampledTexturesPerShaderStage", l.max_sampled_textures_per_shader_stage as u64),
        ("maxSamplersPerShaderStage", l.max_samplers_per_shader_stage as u64),
        ("maxStorageBuffersPerShaderStage", l.max_storage_buffers_per_shader_stage as u64),
        ("maxStorageTexturesPerShaderStage", l.max_storage_textures_per_shader_stage as u64),
        ("maxUniformBuffersPerShaderStage", l.max_uniform_buffers_per_shader_stage as u64),
        ("maxUniformBufferBindingSize", l.max_uniform_buffer_binding_size),
        ("maxStorageBufferBindingSize", l.max_storage_buffer_binding_size),
        ("minUniformBufferOffsetAlignment", l.min_uniform_buffer_offset_alignment as u64),
        ("minStorageBufferOffsetAlignment", l.min_storage_buffer_offset_alignment as u64),
        ("maxVertexBuffers", l.max_vertex_buffers as u64),
        ("maxBufferSize", l.max_buffer_size),
        ("maxVertexAttributes", l.max_vertex_attributes as u64),
        ("maxVertexBufferArrayStride", l.max_vertex_buffer_array_stride as u64),
        ("maxInterStageShaderVariables", l.max_inter_stage_shader_variables as u64),
        ("maxColorAttachments", l.max_color_attachments as u64),
        ("maxColorAttachmentBytesPerSample", l.max_color_attachment_bytes_per_sample as u64),
        ("maxComputeWorkgroupStorageSize", l.max_compute_workgroup_storage_size as u64),
        ("maxComputeInvocationsPerWorkgroup", l.max_compute_invocations_per_workgroup as u64),
        ("maxComputeWorkgroupSizeX", l.max_compute_workgroup_size_x as u64),
        ("maxComputeWorkgroupSizeY", l.max_compute_workgroup_size_y as u64),
        ("maxComputeWorkgroupSizeZ", l.max_compute_workgroup_size_z as u64),
        ("maxComputeWorkgroupsPerDimension", l.max_compute_workgroups_per_dimension as u64),
    ]
}

enum Object {
    Buffer(wgpu::Buffer),
    Shader(wgpu::ShaderModule),
    Pipeline(wgpu::ComputePipeline),
    BindGroup(wgpu::BindGroup),
}

struct Session<'a> {
    gpu: &'a Gpu,
    device: wgpu::Device,
    queue: wgpu::Queue,
    objects: HashMap<u32, Object>,
    encoder: Option<wgpu::CommandEncoder>,
    /// Small writes, recorded in the encoder as copies from staging buffers filled at submission.
    /// wgpu makes a staging buffer per `write_buffer`, and a write after recorded commands would
    /// need them submitted first; a guest writing a few bytes of parameters before every dispatch
    /// paid for both hundreds of times per step. Copies keep the order in one submission.
    staging: Vec<Staging>,
    /// Dispatches recorded since the last submission (`SUBMIT_EVERY`)
    recorded: u32,
    /// The last periodic submission: the next one waits for it, so a guest can't queue work
    /// without bound ahead of the GPU (at most two batches are in flight)
    in_flight: Option<wgpu::SubmissionIndex>,
    /// Bytes of large writes since the queue was last submitted. wgpu keeps each one's copy until
    /// then, outside the guest's memory budget: past `PENDING_WRITES` the queue is submitted.
    pending_writes: u64,
    /// The first error since the last reply.
    error: Arc<Mutex<Option<String>>>,
    /// Why the device was lost, once it is: every reply from then on says so
    lost: Arc<Mutex<Option<String>>>,
}

/// Bytes of large writes wgpu may hold copies of before the queue is submitted (and waited for).
const PENDING_WRITES: u64 = 256 << 20;

/// Dispatches recorded before what was recorded is submitted, though the guest asks for nothing
/// back yet. A guest's forward pass is thousands of dispatches; as one command buffer it can run
/// long enough for the OS's GPU watchdog to take the device (macOS does, as a lost device).
const SUBMIT_EVERY: u32 = 64;

/// Writes up to this size are staged; larger ones go to the queue (after what was recorded).
const SMALL_WRITE: usize = 64 << 10;
/// A staging buffer's size.
const STAGING: u64 = 1 << 20;

/// A staging buffer the encoder copies from, and its bytes, put in when it is submitted.
struct Staging {
    buffer: wgpu::Buffer,
    bytes: Vec<u8>,
}

impl<'a> Session<'a> {
    fn new(gpu: &'a Gpu, device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let error = Arc::new(Mutex::new(None::<String>));
        let e = error.clone();
        device.on_uncaptured_error(Arc::new(move |err: wgpu::Error| {
            e.lock().unwrap().get_or_insert_with(|| err.to_string());
        }));
        // A lost device is what the guest is told, at every reply from then on, rather than the
        // invalid objects that follow
        let lost = Arc::new(Mutex::new(None::<String>));
        let l = lost.clone();
        device.set_device_lost_callback(move |reason, msg| {
            *l.lock().unwrap() = Some(format!("the GPU device was lost ({reason:?}): {msg}"));
        });
        Session {
            gpu,
            device,
            queue,
            objects: HashMap::new(),
            encoder: None,
            staging: Vec::new(),
            recorded: 0,
            in_flight: None,
            pending_writes: 0,
            error,
            lost,
        }
    }

    /// Keeps an object, under the guest's object limit. A buffer's bytes were taken when it was
    /// made, and are given back when it goes.
    fn keep(&mut self, id: u32, o: Object) -> io::Result<()> {
        let max = self.gpu.limits.objects;
        if !self.objects.contains_key(&id) && take(&self.gpu.used.objects, 1, max as u64).is_err() {
            return Err(io::Error::other(format!("too many objects (this guest may have {max})")));
        }
        if let Some(old) = self.objects.insert(id, o) {
            self.forget(old);
        }
        Ok(())
    }

    /// Drops the object `id`, giving back what it held.
    fn remove(&mut self, id: u32) {
        if let Some(o) = self.objects.remove(&id) {
            self.gpu.used.objects.fetch_sub(1, Ordering::AcqRel);
            self.forget(o);
        }
    }

    fn forget(&mut self, o: Object) {
        if let Object::Buffer(b) = o {
            self.gpu.give_memory(b.size());
            if self.encoder.is_some() {
                self.submit();
            }
            b.destroy();
        }
    }

    fn fail(&self, msg: impl Into<String>) {
        self.error.lock().unwrap().get_or_insert_with(|| msg.into());
    }

    /// A reply: `OK` and `payload`, or the pending error (the lost device's, once it is lost).
    fn reply(&self, payload: Vec<u8>) -> Vec<u8> {
        let error = self.error.lock().unwrap().take();
        match self.lost.lock().unwrap().clone().or(error) {
            Some(e) => [ERR].into_iter().chain(e.into_bytes()).collect(),
            None => [OK].into_iter().chain(payload).collect(),
        }
    }

    /// Submits what was recorded, staged writes in their places. The submission, if there was one.
    fn submit(&mut self) -> Option<wgpu::SubmissionIndex> {
        for st in self.staging.drain(..) {
            // Only what was written (the rest of the buffer is never copied from), as a range wgpu
            // maps: a multiple of 8 bytes
            let mut bytes = st.bytes;
            bytes.resize(bytes.len().next_multiple_of(wgpu::MAP_ALIGNMENT as usize).min(STAGING as usize), 0);
            if !bytes.is_empty() {
                if let Ok(mut view) = st.buffer.slice(..bytes.len() as u64).get_mapped_range_mut() {
                    view.copy_from_slice(&bytes);
                }
            }
            st.buffer.unmap();
        }
        self.recorded = 0;
        let enc = self.encoder.take()?;
        self.pending_writes = 0;
        Some(self.queue.submit([enc.finish()]))
    }

    /// The periodic submission (`SUBMIT_EVERY`): waits for the one before it, so the GPU has
    /// this batch to run while that one finishes, and the guest no more than that ahead of it.
    fn submit_batch(&mut self) {
        let index = self.submit();
        if let Some(prev) = std::mem::replace(&mut self.in_flight, index) {
            let _ = self.device.poll(wgpu::PollType::Wait { submission_index: Some(prev), timeout: None });
        }
    }

    /// Records a small write: its bytes into a staging buffer, and a copy into the encoder.
    fn stage_write(&mut self, b: &wgpu::Buffer, offset: u64, data: &[u8]) {
        let len = data.len().next_multiple_of(4);
        if self.staging.last().is_none_or(|st| st.bytes.len() + len > STAGING as usize) {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("armless writes"),
                size: STAGING,
                usage: wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            });
            self.staging.push(Staging { buffer, bytes: Vec::with_capacity(STAGING as usize) });
        }
        let st = self.staging.last_mut().unwrap();
        let at = st.bytes.len() as u64;
        st.bytes.extend_from_slice(data);
        st.bytes.resize(st.bytes.len().next_multiple_of(4), 0);
        let src = st.buffer.clone();
        self.encoder().copy_buffer_to_buffer(&src, at, b, offset, data.len() as u64);
    }

    fn encoder(&mut self) -> &mut wgpu::CommandEncoder {
        self.encoder
            .get_or_insert_with(|| self.device.create_command_encoder(&Default::default()))
    }

    fn buffer(&self, id: u32) -> Option<&wgpu::Buffer> {
        match self.objects.get(&id) {
            Some(Object::Buffer(b)) => Some(b),
            _ => None,
        }
    }

    fn handle(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let mut d = Dec::new(frame);
        let op = d.u8().ok().and_then(Op::from_u8);
        let Some(op) = op else {
            self.fail("unknown command");
            return None;
        };
        match self.run(op, &mut d) {
            Ok(r) => r,
            Err(e) => {
                self.fail(format!("{op:?}: {e}"));
                // A command that owed a reply still gets one, carrying the error
                matches!(op, Op::Hello | Op::ReadBuffer | Op::Finish)
                    .then(|| self.reply(Vec::new()))
            },
        }
    }

    fn run(&mut self, op: Op, d: &mut Dec) -> io::Result<Option<Vec<u8>>> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_string());
        match op {
            Op::Hello => {
                let _guest_version = d.u32()?;
                let info = self.gpu.adapter.get_info();
                let l = self.device.limits();
                let f = self.device.features();
                let mut features = 0;
                if f.contains(wgpu::Features::SHADER_F16) {
                    features |= FEATURE_F16;
                }
                if f.contains(wgpu::Features::SUBGROUP) {
                    features |= FEATURE_SUBGROUPS;
                }
                let mut l = l;
                l.max_buffer_size = l.max_buffer_size.min(self.gpu.limits.memory);
                let pairs = limit_pairs(&l);
                let mut p = Enc::default()
                    .u32(VERSION)
                    .str(&info.name)
                    .str(&format!("{:?}", info.backend))
                    .u64(l.max_buffer_size)
                    .u64(l.max_storage_buffer_binding_size)
                    .u32(l.max_compute_workgroups_per_dimension)
                    .u32(features)
                    .u32(info.subgroup_min_size)
                    .u32(info.subgroup_max_size)
                    .str(&format!("{:#06x}", info.vendor))
                    .str("")
                    .str(&info.name)
                    .str(&info.driver_info)
                    .u32(pairs.len() as u32);
                for (k, v) in pairs {
                    p = p.str(k).u64(v);
                }
                return Ok(Some(self.reply(p.0)));
            },
            Op::CreateBuffer => {
                let (id, size, usage) = (d.u32()?, d.u64()?, d.u32()?);
                let usage = wgpu::BufferUsages::from_bits(host_usage(usage)).ok_or_else(|| bad("bad usage"))?;
                self.gpu.take_memory(size)?;
                let b = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size,
                    usage,
                    mapped_at_creation: false,
                });
                if let Err(e) = self.keep(id, Object::Buffer(b.clone())) {
                    self.gpu.give_memory(size);
                    b.destroy();
                    return Err(e);
                }
            },
            Op::WriteBuffer => {
                let (id, offset) = (d.u32()?, d.u64()?);
                let data = d.rest();
                let b = self.buffer(id).ok_or_else(|| bad("no such buffer"))?.clone();
                if data.is_empty() {
                    return Ok(None);
                }
                // After what was recorded before it, before what is recorded after it
                if data.len() > SMALL_WRITE || !data.len().is_multiple_of(4) || !offset.is_multiple_of(4) {
                    self.submit();
                    self.queue.write_buffer(&b, offset, data);
                    // wgpu holds the write's copy until the queue is next submitted: a guest that
                    // only writes would have the host hold them without bound
                    self.pending_writes += data.len() as u64;
                    if self.pending_writes >= PENDING_WRITES {
                        let index = self.queue.submit([]);
                        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None });
                        self.pending_writes = 0;
                    }
                } else {
                    self.stage_write(&b, offset, data);
                }
            },
            Op::CreateShader => {
                let (id, src) = (d.u32()?, d.str()?);
                let m = self
                    .device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: None,
                        source: wgpu::ShaderSource::Wgsl(src.into()),
                    });
                self.keep(id, Object::Shader(m))?;
            },
            Op::CreatePipeline => {
                let (id, shader, entry) = (d.u32()?, d.u32()?, d.str()?);
                // Pipeline-overridable constants (version 2): u32 n, n × (str key, f64)
                let mut constants = Vec::new();
                if !d.is_empty() {
                    for _ in 0..d.u32()? {
                        constants.push((d.str()?, f64::from_bits(d.u64()?)));
                    }
                }
                let Some(Object::Shader(m)) = self.objects.get(&shader) else {
                    return Err(bad("no such shader"));
                };
                let p = self
                    .device
                    .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: None,
                        layout: None,
                        module: m,
                        entry_point: Some(entry),
                        compilation_options: wgpu::PipelineCompilationOptions {
                            constants: &constants,
                            ..Default::default()
                        },
                        cache: None,
                    });
                self.keep(id, Object::Pipeline(p))?;
            },
            Op::CreateBindGroup => {
                let (id, pipeline, group, n) = (d.u32()?, d.u32()?, d.u32()?, d.u32()?);
                let Some(Object::Pipeline(p)) = self.objects.get(&pipeline) else {
                    return Err(bad("no such pipeline"));
                };
                let layout = p.get_bind_group_layout(group);
                let mut spec = Vec::new();
                for _ in 0..n {
                    spec.push((d.u32()?, d.u32()?, d.u64()?, d.u64()?));
                }
                let mut entries = Vec::new();
                for &(binding, buffer, offset, size) in &spec {
                    let b = self.buffer(buffer).ok_or_else(|| bad("no such buffer"))?;
                    entries.push(wgpu::BindGroupEntry {
                        binding,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: b,
                            offset,
                            size: wgpu::BufferSize::new(size),
                        }),
                    });
                }
                let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &layout,
                    entries: &entries,
                });
                self.keep(id, Object::BindGroup(bg))?;
            },
            Op::Dispatch => {
                let pipeline = d.u32()?;
                let n = d.u32()?;
                let mut groups = Vec::new();
                for _ in 0..n {
                    groups.push((d.u32()?, d.u32()?));
                }
                let (x, y, z) = (d.u32()?, d.u32()?, d.u32()?);
                let mut enc = self
                    .encoder
                    .take()
                    .unwrap_or_else(|| self.device.create_command_encoder(&Default::default()));
                {
                    let Some(Object::Pipeline(p)) = self.objects.get(&pipeline) else {
                        self.encoder = Some(enc);
                        return Err(bad("no such pipeline"));
                    };
                    let mut pass = enc.begin_compute_pass(&Default::default());
                    pass.set_pipeline(p);
                    for &(g, bg) in &groups {
                        match self.objects.get(&bg) {
                            Some(Object::BindGroup(b)) => pass.set_bind_group(g, b, &[]),
                            _ => {
                                drop(pass);
                                self.encoder = Some(enc);
                                return Err(bad("no such bind group"));
                            },
                        }
                    }
                    pass.dispatch_workgroups(x, y, z);
                }
                self.encoder = Some(enc);
                self.recorded += 1;
                if self.recorded >= SUBMIT_EVERY {
                    self.submit_batch();
                }
            },
            Op::CopyBuffer => {
                let (src, so, dst, dof, size) = (d.u32()?, d.u64()?, d.u32()?, d.u64()?, d.u64()?);
                let (s, t) = (
                    self.buffer(src)
                        .ok_or_else(|| bad("no such buffer"))?
                        .clone(),
                    self.buffer(dst)
                        .ok_or_else(|| bad("no such buffer"))?
                        .clone(),
                );
                self.encoder().copy_buffer_to_buffer(&s, so, &t, dof, size);
            },
            Op::ReadBuffer => {
                let (id, offset, size) = (d.u32()?, d.u64()?, d.u64()?);
                if offset % 4 != 0 || size % 4 != 0 {
                    return Err(bad("reads must be 4-byte aligned"));
                }
                // The reply carries the bytes after its status byte, in one frame
                if size >= MAX_FRAME as u64 {
                    return Err(bad("a read must be smaller than a frame (256 MiB)"));
                }
                let src = self
                    .buffer(id)
                    .ok_or_else(|| bad("no such buffer"))?
                    .clone();
                // The staging buffer is the guest's while the read lasts
                self.gpu.take_memory(size)?;
                let data = self.read(&src, offset, size);
                self.gpu.give_memory(size);
                return Ok(Some(self.reply(data?)));
            },
            Op::Destroy => {
                let id = d.u32()?;
                // A buffer: what was recorded for it runs first, as it was sent first
                self.remove(id);
            },
            Op::Finish => {
                self.submit();
                let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
                return Ok(Some(self.reply(Vec::new())));
            },
        }
        Ok(None)
    }

    /// `size` bytes of `src` from `offset`, once what was sent before has run.
    fn read(&mut self, src: &wgpu::Buffer, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.encoder()
            .copy_buffer_to_buffer(src, offset, &staging, 0, size);
        self.submit();
        let slice = staging.slice(..);
        let mapped = Arc::new(Mutex::new(None));
        let m = mapped.clone();
        slice.map_async(wgpu::MapMode::Read, move |r| *m.lock().unwrap() = Some(r));
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = match mapped.lock().unwrap().take() {
            Some(Ok(())) => slice
                .get_mapped_range()
                .map(|v| v.to_vec())
                .map_err(io::Error::other),
            Some(Err(e)) => Err(io::Error::other(e)),
            None => Err(io::Error::other("the read never finished")),
        };
        staging.destroy();
        data
    }
}

/// A connection that ends gives its guest back what it held.
impl Drop for Session<'_> {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.objects.keys().copied().collect();
        for id in ids {
            self.remove(id);
        }
    }
}

/// A connection to `gpu`, served on a thread of its own: one end of a socket pair, carried as a
/// `TcpStream` the way armless carries a link's Unix socket. What a host's link dialer returns
/// for the guest's connections to the GPU (see [`crate::link`]).
#[cfg(unix)]
pub fn connect(gpu: Arc<Gpu>) -> io::Result<std::net::TcpStream> {
    use std::os::unix::net::UnixStream;
    let (ours, theirs) = UnixStream::pair()?;
    let w = ours.try_clone()?;
    // Taken here, not on the thread: one past the guest's limit is refused as it connects
    gpu.take_connection()?;
    let g = gpu.clone();
    let spawned = std::thread::Builder::new()
        .name("armless-gpu".into())
        .spawn(move || {
            if let Err(e) = g.serve_taken(Duplex(io::BufReader::new(ours), io::BufWriter::new(w))) {
                eprintln!("armless-gpu: a guest connection ended: {e}");
            }
        });
    if let Err(e) = spawned {
        gpu.used.connections.fetch_sub(1, Ordering::AcqRel);
        return Err(e);
    }
    Ok(std::net::TcpStream::from(std::os::fd::OwnedFd::from(
        theirs,
    )))
}

/// A reader and a writer as one stream.
pub struct Duplex<R, W>(pub R, pub W);

impl<R: Read, W> Read for Duplex<R, W> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}

impl<R, W: Write> Write for Duplex<R, W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.1.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.1.flush()
    }
}
