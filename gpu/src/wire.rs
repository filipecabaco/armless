//! The wire protocol. Little-endian throughout.
//!
//! Every message, either way, is a frame: a `u32` length, then that many bytes. A command's
//! first byte is its [`Op`]. Ids are the guest's: it numbers its own buffers, shaders,
//! pipelines and bind groups, so no command waits for an answer except those that return
//! something ([`Op::Hello`], [`Op::ReadBuffer`], [`Op::Finish`]). Their reply starts with a
//! status byte: [`OK`], or [`ERR`] followed by the first error since the last reply (commands
//! that fail without a reply of their own are reported then).
//!
//! Buffer usages are WebGPU's `GPUBufferUsage` bits, which wgpu shares.

use std::io::{self, Read, Write};

pub const VERSION: u32 = 2;

/// Hello's feature bits: what the host's device has enabled, so guests' shaders may `enable` it.
pub const FEATURE_F16: u32 = 1;
pub const FEATURE_SUBGROUPS: u32 = 2;
pub const FEATURE_PACKED_DOT: u32 = 4;

/// The largest frame either side accepts (a write or a read of up to this, less a header).
pub const MAX_FRAME: usize = 256 << 20;

pub const OK: u8 = 0;
pub const ERR: u8 = 1;

pub mod usage {
    pub const MAP_READ: u32 = 0x1;
    pub const MAP_WRITE: u32 = 0x2;
    pub const COPY_SRC: u32 = 0x4;
    pub const COPY_DST: u32 = 0x8;
    pub const INDEX: u32 = 0x10;
    pub const VERTEX: u32 = 0x20;
    pub const UNIFORM: u32 = 0x40;
    pub const STORAGE: u32 = 0x80;
    pub const INDIRECT: u32 = 0x100;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    /// `u32 version` → `OK, u32 version, str adapter, str backend, u64 max_buffer_size,
    /// u64 max_storage_buffer_binding_size, u32 max_compute_workgroups_per_dimension`, then
    /// (version 2) `u32 features (FEATURE_*), u32 subgroup_min_size, u32 subgroup_max_size,
    /// str vendor, str architecture, str device, str description, u32 n, n × (str limit, u64)`:
    /// the device's limits by their WebGPU names (maxBufferSize…)
    Hello = 1,
    /// `u32 id, u64 size, u32 usage`. A guest may give map usages: the host reads and writes such
    /// a buffer only by copies (ReadBuffer, WriteBuffer), and makes it with those instead
    CreateBuffer = 2,
    /// `u32 id, u64 offset, bytes…` (the rest of the frame)
    WriteBuffer = 3,
    /// `u32 id, str wgsl`
    CreateShader = 4,
    /// `u32 id, u32 shader, str entry_point` (layout from the shader), then (version 2)
    /// `u32 n, n × (str key, f64 value)`: pipeline-overridable constants
    CreatePipeline = 5,
    /// `u32 id, u32 pipeline, u32 group, u32 n, n × (u32 binding, u32 buffer, u64 offset,
    /// u64 size)`; size 0 binds to the end of the buffer
    CreateBindGroup = 6,
    /// `u32 pipeline, u32 n, n × (u32 group, u32 bind_group), u32 x, u32 y, u32 z`
    Dispatch = 7,
    /// `u32 src, u64 src_offset, u32 dst, u64 dst_offset, u64 size`
    CopyBuffer = 8,
    /// `u32 id, u64 offset, u64 size` → `OK, bytes…`; waits for everything before it
    ReadBuffer = 9,
    /// `u32 id`: any buffer, shader, pipeline or bind group
    Destroy = 10,
    /// → `OK`: waits until the GPU has done everything sent so far
    Finish = 11,
}

impl Op {
    pub fn from_u8(b: u8) -> Option<Op> {
        use Op::*;
        Some(match b {
            1 => Hello,
            2 => CreateBuffer,
            3 => WriteBuffer,
            4 => CreateShader,
            5 => CreatePipeline,
            6 => CreateBindGroup,
            7 => Dispatch,
            8 => CopyBuffer,
            9 => ReadBuffer,
            10 => Destroy,
            11 => Finish,
            _ => return None,
        })
    }
}

/// Reads one frame (`None` at a clean end of the stream).
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {},
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(body)
}

/// Builds a frame's body.
#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn op(op: Op) -> Enc {
        Enc(vec![op as u8])
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// A `u32` length, then UTF-8.
    pub fn str(self, s: &str) -> Self {
        self.u32(s.len() as u32).bytes(s.as_bytes())
    }
    /// Raw bytes, no length (the rest of a frame).
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
}

/// Reads a frame's body.
pub struct Dec<'a> {
    b: &'a [u8],
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "message too short")
}

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec { b }
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(short());
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn str(&mut self) -> io::Result<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.take(n)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
    }
    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
    }
    /// What is left.
    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.b)
    }
}

/// A guest's side of the protocol, over any stream: what a guest program links.
pub struct Client<S: Read + Write> {
    s: S,
    next_id: u32,
}

/// What the host said about its GPU.
#[derive(Debug, Clone)]
pub struct Info {
    pub version: u32,
    pub adapter: String,
    pub backend: String,
    pub max_buffer_size: u64,
    pub max_storage_buffer_binding_size: u64,
    pub max_compute_workgroups_per_dimension: u32,
    /// FEATURE_* bits (0 from a version 1 host)
    pub features: u32,
    /// Every limit the host reported, by its WebGPU name
    pub limits: Vec<(String, u64)>,
}

impl<S: Read + Write> Client<S> {
    pub fn new(s: S) -> Self {
        Client { s, next_id: 1 }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, e: Enc) -> io::Result<()> {
        write_frame(&mut self.s, &e.0)
    }

    /// The reply to the command just sent: its payload, or the host's error.
    fn reply(&mut self) -> io::Result<Vec<u8>> {
        self.s.flush()?;
        let f = read_frame(&mut self.s)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "host closed"))?;
        let mut d = Dec::new(&f);
        match d.u8()? {
            OK => Ok(d.rest().to_vec()),
            _ => Err(io::Error::other(
                String::from_utf8_lossy(d.rest()).into_owned(),
            )),
        }
    }

    pub fn hello(&mut self) -> io::Result<Info> {
        self.send(Enc::op(Op::Hello).u32(VERSION))?;
        let r = self.reply()?;
        let mut d = Dec::new(&r);
        Ok(Info {
            version: d.u32()?,
            adapter: d.str()?.to_string(),
            backend: d.str()?.to_string(),
            max_buffer_size: d.u64()?,
            max_storage_buffer_binding_size: d.u64()?,
            max_compute_workgroups_per_dimension: d.u32()?,
            features: if d.is_empty() { 0 } else { d.u32()? },
            limits: if d.is_empty() {
                Vec::new()
            } else {
                let _ = (d.u32()?, d.u32()?, d.str()?, d.str()?, d.str()?, d.str()?);
                let mut l = Vec::new();
                for _ in 0..d.u32()? {
                    l.push((d.str()?.to_string(), d.u64()?));
                }
                l
            },
        })
    }

    pub fn create_buffer(&mut self, size: u64, usage: u32) -> io::Result<u32> {
        let id = self.id();
        self.send(Enc::op(Op::CreateBuffer).u32(id).u64(size).u32(usage))?;
        Ok(id)
    }

    pub fn write_buffer(&mut self, id: u32, offset: u64, data: &[u8]) -> io::Result<()> {
        self.send(Enc::op(Op::WriteBuffer).u32(id).u64(offset).bytes(data))
    }

    pub fn create_shader(&mut self, wgsl: &str) -> io::Result<u32> {
        let id = self.id();
        self.send(Enc::op(Op::CreateShader).u32(id).str(wgsl))?;
        Ok(id)
    }

    pub fn create_pipeline(&mut self, shader: u32, entry: &str) -> io::Result<u32> {
        self.create_pipeline_with(shader, entry, &[])
    }

    /// With pipeline-overridable constants.
    pub fn create_pipeline_with(&mut self, shader: u32, entry: &str, constants: &[(&str, f64)]) -> io::Result<u32> {
        let id = self.id();
        let mut e = Enc::op(Op::CreatePipeline).u32(id).u32(shader).str(entry).u32(constants.len() as u32);
        for (k, v) in constants {
            e = e.str(k).u64(v.to_bits());
        }
        self.send(e)?;
        Ok(id)
    }

    /// `entries`: (binding, buffer, offset, size; 0 for the rest of the buffer).
    pub fn create_bind_group(
        &mut self,
        pipeline: u32,
        group: u32,
        entries: &[(u32, u32, u64, u64)],
    ) -> io::Result<u32> {
        let id = self.id();
        let mut e = Enc::op(Op::CreateBindGroup)
            .u32(id)
            .u32(pipeline)
            .u32(group)
            .u32(entries.len() as u32);
        for &(binding, buffer, offset, size) in entries {
            e = e.u32(binding).u32(buffer).u64(offset).u64(size);
        }
        self.send(e)?;
        Ok(id)
    }

    /// `groups`: (group index, bind group).
    pub fn dispatch(
        &mut self,
        pipeline: u32,
        groups: &[(u32, u32)],
        x: u32,
        y: u32,
        z: u32,
    ) -> io::Result<()> {
        let mut e = Enc::op(Op::Dispatch).u32(pipeline).u32(groups.len() as u32);
        for &(g, bg) in groups {
            e = e.u32(g).u32(bg);
        }
        self.send(e.u32(x).u32(y).u32(z))
    }

    pub fn copy_buffer(
        &mut self,
        src: u32,
        src_offset: u64,
        dst: u32,
        dst_offset: u64,
        size: u64,
    ) -> io::Result<()> {
        self.send(
            Enc::op(Op::CopyBuffer)
                .u32(src)
                .u64(src_offset)
                .u32(dst)
                .u64(dst_offset)
                .u64(size),
        )
    }

    pub fn read_buffer(&mut self, id: u32, offset: u64, size: u64) -> io::Result<Vec<u8>> {
        self.send(Enc::op(Op::ReadBuffer).u32(id).u64(offset).u64(size))?;
        self.reply()
    }

    pub fn destroy(&mut self, id: u32) -> io::Result<()> {
        self.send(Enc::op(Op::Destroy).u32(id))
    }

    pub fn finish(&mut self) -> io::Result<()> {
        self.send(Enc::op(Op::Finish))?;
        self.reply().map(|_| ())
    }
}
