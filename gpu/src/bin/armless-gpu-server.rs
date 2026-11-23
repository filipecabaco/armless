//! Serves the armless GPU protocol on a TCP address, for guests whose host reaches it with a
//! tunnel (`--tunnel gpu.internal:7070=tcp://127.0.0.1:7070 --allow-private`).
//!
//!   armless-gpu-server [ADDRESS]    (default 127.0.0.1:7070)

use std::net::TcpListener;
use std::sync::Arc;

use armless_gpu::server::Duplex;

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7070".into());
    let gpu = Arc::new(armless_gpu::server::Gpu::new().unwrap_or_else(|| {
        eprintln!("armless-gpu-server: no GPU adapter");
        std::process::exit(1)
    }));
    let info = gpu.adapter_info();
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("armless-gpu-server: {addr}: {e}");
        std::process::exit(1)
    });
    eprintln!(
        "armless-gpu-server: {} ({:?}) on {addr}",
        info.name, info.backend
    );
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let _ = conn.set_nodelay(true);
        let gpu = gpu.clone();
        std::thread::spawn(move || {
            let peer = conn.peer_addr().ok();
            let r = gpu.serve(Duplex(std::io::BufReader::new(&conn), &conn));
            eprintln!("armless-gpu-server: {peer:?} closed ({r:?})");
        });
    }
}
