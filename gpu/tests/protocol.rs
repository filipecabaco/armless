//! The protocol against a real adapter (skipped where wgpu finds none; CI can install Mesa's
//! lavapipe for a software one).

use std::sync::Arc;

use armless_gpu::server::{connect, Duplex, Gpu, Limits};
use armless_gpu::wire::{usage, Client};

type Conn = Client<Duplex<std::io::BufReader<std::net::TcpStream>, std::net::TcpStream>>;

/// No adapter skips a test, unless ARMLESS_GPU_TEST_REQUIRE is set (CI, with lavapipe installed).
fn gpu() -> Option<Gpu> {
    let g = Gpu::new();
    if g.is_none() {
        assert!(std::env::var_os("ARMLESS_GPU_TEST_REQUIRE").is_none(), "no GPU adapter, and ARMLESS_GPU_TEST_REQUIRE is set");
        eprintln!("no GPU adapter: skipped");
    }
    g
}

fn client() -> Option<Conn> {
    let gpu = gpu()?;
    let s = connect(Arc::new(gpu)).unwrap();
    Some(Client::new(Duplex(
        std::io::BufReader::new(s.try_clone().unwrap()),
        s,
    )))
}

fn f32s(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn hello_says_what_the_adapter_is() {
    let Some(mut c) = client() else { return };
    let info = c.hello().unwrap();
    assert_eq!(info.version, armless_gpu::wire::VERSION);
    assert!(!info.adapter.is_empty());
    assert!(info.max_buffer_size >= 1 << 20);
}

#[test]
fn a_dispatch_then_a_copy_then_a_write_keep_their_order() {
    let Some(mut c) = client() else { return };
    let n = 256u64;
    let src = c
        .create_buffer(n * 4, usage::STORAGE | usage::COPY_DST | usage::COPY_SRC)
        .unwrap();
    let dst = c
        .create_buffer(n * 4, usage::STORAGE | usage::COPY_DST | usage::COPY_SRC)
        .unwrap();
    c.write_buffer(src, 0, &f32s(&vec![1.0; n as usize]))
        .unwrap();
    let shader = c
        .create_shader(
            "@group(0) @binding(0) var<storage, read_write> v: array<f32>;
             @compute @workgroup_size(64) fn double(@builtin(global_invocation_id) id: vec3<u32>) {
               v[id.x] = v[id.x] * 2.0;
             }",
        )
        .unwrap();
    let p = c.create_pipeline(shader, "double").unwrap();
    let g = c.create_bind_group(p, 0, &[(0, src, 0, 0)]).unwrap();
    c.dispatch(p, &[(0, g)], 4, 1, 1).unwrap();
    c.copy_buffer(src, 0, dst, 0, n * 4).unwrap();
    // After the copy was recorded: must not reach dst
    c.write_buffer(src, 0, &f32s(&vec![9.0; n as usize]))
        .unwrap();
    let out = c.read_buffer(dst, 0, n * 4).unwrap();
    assert!(out
        .chunks_exact(4)
        .all(|b| f32::from_le_bytes(b.try_into().unwrap()) == 2.0));
    let out = c.read_buffer(src, 0, 8).unwrap();
    assert_eq!(f32::from_le_bytes(out[..4].try_into().unwrap()), 9.0);
}

#[test]
fn a_bad_shader_is_reported_at_the_next_reply_and_the_connection_goes_on() {
    let Some(mut c) = client() else { return };
    let shader = c.create_shader("this is not wgsl").unwrap();
    let err = c.finish().unwrap_err().to_string();
    assert!(!err.is_empty(), "{err}");
    let _ = shader;
    c.finish().unwrap();
    c.hello().unwrap();
}

#[test]
fn an_unknown_buffer_is_an_error_not_a_crash() {
    let Some(mut c) = client() else { return };
    c.write_buffer(999, 0, &[0; 4]).unwrap();
    let err = c.finish().unwrap_err().to_string();
    assert!(err.contains("no such buffer"), "{err}");
    assert!(c
        .read_buffer(999, 0, 4)
        .unwrap_err()
        .to_string()
        .contains("no such buffer"));
}

#[test]
fn hello_reports_features_and_limits_by_name() {
    let Some(mut c) = client() else { return };
    let info = c.hello().unwrap();
    assert_eq!(info.version, 2);
    let get = |k: &str| info.limits.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    assert_eq!(get("maxBufferSize"), Some(info.max_buffer_size));
    assert!(get("minStorageBufferOffsetAlignment").is_some_and(|v| v.is_power_of_two()));
    assert!(get("maxComputeInvocationsPerWorkgroup").is_some_and(|v| v >= 64));
}

#[test]
fn pipeline_constants_reach_the_shader() {
    let Some(mut c) = client() else { return };
    let out = c.create_buffer(16, usage::STORAGE | usage::COPY_SRC).unwrap();
    let shader = c
        .create_shader(
            "override scale: f32 = 1.0;
             override wg_size: u32 = 1;
             @group(0) @binding(0) var<storage, read_write> o: array<f32>;
             @compute @workgroup_size(wg_size) fn main(@builtin(local_invocation_index) i: u32) {
               o[i] = scale * f32(i);
             }",
        )
        .unwrap();
    let p = c.create_pipeline_with(shader, "main", &[("scale", 2.5), ("wg_size", 4.0)]).unwrap();
    let g = c.create_bind_group(p, 0, &[(0, out, 0, 0)]).unwrap();
    c.dispatch(p, &[(0, g)], 1, 1, 1).unwrap();
    let r = c.read_buffer(out, 0, 16).unwrap();
    let v: Vec<f32> = r.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
    assert_eq!(v, [0.0, 2.5, 5.0, 7.5]);
}

#[test]
fn a_buffer_the_guest_maps_for_reading_is_read_through_the_host() {
    let Some(mut c) = client() else { return };
    // What ggml-webgpu does: copy into a MapRead | CopyDst buffer, then map it
    let src = c.create_buffer(8, usage::STORAGE | usage::COPY_DST | usage::COPY_SRC).unwrap();
    let staging = c.create_buffer(8, usage::MAP_READ | usage::COPY_DST).unwrap();
    c.write_buffer(src, 0, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    c.copy_buffer(src, 0, staging, 0, 8).unwrap();
    assert_eq!(c.read_buffer(staging, 0, 8).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
}

fn on(gpu: &Arc<Gpu>) -> Conn {
    let s = connect(gpu.clone()).unwrap();
    Client::new(Duplex(std::io::BufReader::new(s.try_clone().unwrap()), s))
}

fn limited_gpu(limits: Limits) -> Option<Arc<Gpu>> {
    Some(Arc::new(gpu()?.with_limits(limits)))
}

fn limited(limits: Limits) -> Option<Conn> {
    Some(on(&limited_gpu(limits)?))
}

#[test]
fn a_guest_gets_no_more_gpu_memory_than_its_budget_and_destroying_gives_it_back() {
    let Some(mut c) = limited(Limits { memory: 1 << 20, objects: 100, ..Default::default() }) else { return };
    let info = c.hello().unwrap();
    assert!(info.max_buffer_size <= 1 << 20, "Hello says the budget: {}", info.max_buffer_size);
    let a = c.create_buffer(768 << 10, usage::STORAGE).unwrap();
    let b = c.create_buffer(512 << 10, usage::STORAGE).unwrap();
    let err = c.finish().unwrap_err().to_string();
    assert!(err.contains("out of GPU memory"), "{err}");
    let _ = b;
    c.destroy(a).unwrap();
    c.create_buffer(512 << 10, usage::STORAGE).unwrap();
    c.finish().unwrap();
}

#[test]
fn a_guest_gets_no_more_objects_than_it_may_have() {
    let Some(mut c) = limited(Limits { memory: 1 << 30, objects: 3, ..Default::default() }) else { return };
    for _ in 0..3 {
        c.create_buffer(16, usage::STORAGE).unwrap();
    }
    c.finish().unwrap();
    c.create_buffer(16, usage::STORAGE).unwrap();
    assert!(c.finish().unwrap_err().to_string().contains("too many objects"));
}

#[test]
fn a_guest_gets_no_more_gpu_memory_for_opening_more_connections() {
    let Some(gpu) = limited_gpu(Limits { memory: 1 << 20, objects: 4, ..Default::default() }) else { return };
    let mut a = on(&gpu);
    a.create_buffer(768 << 10, usage::STORAGE).unwrap();
    a.finish().unwrap();
    let mut b = on(&gpu);
    b.create_buffer(512 << 10, usage::STORAGE).unwrap();
    let err = b.finish().unwrap_err().to_string();
    assert!(err.contains("out of GPU memory"), "{err}");
    // a holds one of the guest's 4 objects: b gets the other 3
    for _ in 0..4 {
        b.create_buffer(16, usage::STORAGE).unwrap();
    }
    let err = b.finish().unwrap_err().to_string();
    assert!(err.contains("too many objects"), "the objects are the guest's too: {err}");
    // A connection that ends gives back what it held
    drop(a);
    let mut c = on(&gpu);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        c.create_buffer(768 << 10, usage::STORAGE).unwrap();
        match c.finish() {
            Ok(()) => break,
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => panic!("the closed connection's memory never came back: {e}"),
        }
    }
}

#[test]
fn a_guest_may_have_so_many_connections_open() {
    let Some(gpu) = limited_gpu(Limits { connections: 2, ..Default::default() }) else { return };
    let (mut a, mut b) = (on(&gpu), on(&gpu));
    a.hello().unwrap();
    b.hello().unwrap();
    let err = connect(gpu.clone()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "a third is refused: {err}");
    drop(a);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while connect(gpu.clone()).is_err() {
        assert!(std::time::Instant::now() < deadline, "a closed connection's place never came back");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    b.hello().unwrap();
}

#[test]
fn a_read_counts_against_the_budget_and_fits_in_a_frame() {
    let Some(mut c) = limited(Limits { memory: 1 << 20, ..Default::default() }) else { return };
    let b = c.create_buffer(768 << 10, usage::STORAGE | usage::COPY_SRC).unwrap();
    let err = c.read_buffer(b, 0, 768 << 10).unwrap_err().to_string();
    assert!(err.contains("out of GPU memory"), "its staging is the guest's too: {err}");
    assert_eq!(c.read_buffer(b, 0, 256 << 10).unwrap().len(), 256 << 10);
    // Given back once read
    assert_eq!(c.read_buffer(b, 0, 256 << 10).unwrap().len(), 256 << 10);
    let err = c.read_buffer(b, 0, armless_gpu::wire::MAX_FRAME as u64).unwrap_err().to_string();
    assert!(err.contains("smaller than a frame"), "{err}");
    c.finish().unwrap();
}
