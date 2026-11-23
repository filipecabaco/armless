//! WebGPU compute for armless guests.
//!
//! A guest program opens a connection (to `gpu.internal:7070` by default) and sends [`wire`]
//! commands: buffers, WGSL shaders, compute pipelines, bind groups, dispatches and reads. The
//! host runs them on a real GPU: natively with [`server`] (wgpu), in the browser with WebGPU.
//! Shaders travel as WGSL text, so the guest compiles nothing and both hosts take them as they
//! are. Only compute is covered: what inference needs.

pub mod wire;

#[cfg(feature = "server")]
pub mod server;

/// Where guests find the GPU unless told otherwise.
pub const DEFAULT_HOST: &str = "gpu.internal";
pub const DEFAULT_PORT: u16 = 7070;

/// The link a host gives its guests for the GPU: `gpu.internal:7070`, answered in-process (the
/// unix:// address is a placeholder; the host's link dialer opens the connection with
/// [`server::connect`] instead).
pub fn link_spec() -> (String, u16, String) {
    (
        DEFAULT_HOST.to_string(),
        DEFAULT_PORT,
        "unix:///armless-gpu".to_string(),
    )
}
