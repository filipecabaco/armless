//! cargo run --release --example fetch -- URL   (a request through armless's native egress)
use std::io::Read;

fn main() {
    let url = std::env::args().nth(1).expect("URL");
    let egress = armless::native::Egress::from_env();
    eprintln!(
        "proxy: {:?}",
        egress
            .https_proxy
            .as_ref()
            .map(|p| format!("{}:{}", p.host, p.port))
    );
    let mut r = egress
        .request("GET", &url, &[("accept".into(), "*/*".into())], &[])
        .unwrap();
    let mut body = Vec::new();
    r.body.read_to_end(&mut body).unwrap();
    println!("{} {} ({} bytes)", r.status, r.reason, body.len());
}
