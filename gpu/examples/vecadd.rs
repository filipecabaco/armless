//! A guest program: adds two vectors on the host's GPU.
//!
//!   cargo build -p armless-gpu --example vecadd --no-default-features --target aarch64-unknown-linux-musl
//!
//! Connects to `ARMLESS_GPU` (HOST:PORT), or gpu.internal:7070.

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::time::Instant;

use armless_gpu::wire::{usage, Client};

const SHADER: &str = "
@group(0) @binding(0) var<storage, read> a: array<f32>;
@group(0) @binding(1) var<storage, read> b: array<f32>;
@group(0) @binding(2) var<storage, read_write> o: array<f32>;
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x < arrayLength(&o)) { o[id.x] = a[id.x] + b[id.x]; }
}";

struct Stream(BufReader<TcpStream>, BufWriter<TcpStream>);
impl Read for Stream {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for Stream {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.1.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.1.flush()
    }
}

fn f32s(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn main() -> std::io::Result<()> {
    let addr = std::env::var("ARMLESS_GPU").unwrap_or_else(|_| {
        format!(
            "{}:{}",
            armless_gpu::DEFAULT_HOST,
            armless_gpu::DEFAULT_PORT
        )
    });
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1 << 20);
    let t = Instant::now();
    let s = TcpStream::connect(&addr)?;
    let _ = s.set_nodelay(true);
    let mut gpu = Client::new(Stream(BufReader::new(s.try_clone()?), BufWriter::new(s)));
    let info = gpu.hello()?;
    println!(
        "gpu: {} ({}), max buffer {} MiB",
        info.adapter,
        info.backend,
        info.max_buffer_size >> 20
    );

    let a: Vec<f32> = (0..n).map(|i| i as f32).collect();
    let b: Vec<f32> = (0..n).map(|i| 2.0 * i as f32).collect();
    let bytes = n as u64 * 4;
    let ba = gpu.create_buffer(bytes, usage::STORAGE | usage::COPY_DST)?;
    let bb = gpu.create_buffer(bytes, usage::STORAGE | usage::COPY_DST)?;
    let bo = gpu.create_buffer(bytes, usage::STORAGE | usage::COPY_SRC)?;
    gpu.write_buffer(ba, 0, &f32s(&a))?;
    gpu.write_buffer(bb, 0, &f32s(&b))?;
    let shader = gpu.create_shader(SHADER)?;
    let pipeline = gpu.create_pipeline(shader, "main")?;
    let group =
        gpu.create_bind_group(pipeline, 0, &[(0, ba, 0, 0), (1, bb, 0, 0), (2, bo, 0, 0)])?;
    let workgroups = n.div_ceil(64);
    let (x, y) =
        if workgroups > 65535 { (65535, workgroups.div_ceil(65535)) } else { (workgroups, 1) };
    // The shader indexes by x only: a 2-D grid would need it to fold y in. Keep to one row.
    assert!(y == 1, "use at most {} elements", 65535 * 64);
    gpu.dispatch(pipeline, &[(0, group)], x, y, 1)?;
    let out = gpu.read_buffer(bo, 0, bytes)?;
    let wrong = out
        .chunks_exact(4)
        .enumerate()
        .filter(|(i, c)| f32::from_le_bytes((*c).try_into().unwrap()) != 3.0 * *i as f32)
        .count();
    println!(
        "vector add of {n} floats on the host GPU: {} in {:.0} ms",
        if wrong == 0 { "correct".to_string() } else { format!("{wrong} WRONG") },
        t.elapsed().as_secs_f64() * 1e3
    );
    std::process::exit(if wrong == 0 { 0 } else { 1 });
}
