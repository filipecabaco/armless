// The `hypervisor` feature runs guests in hardware where the host has a hypervisor armless can
// drive: Hypervisor.framework on macOS with Apple silicon (cfg `armless_hvf`), KVM on Linux arm64
// (cfg `armless_kvm`); `armless_hw` is either. Elsewhere (x86_64, wasm) the feature does nothing.
fn main() {
    for cfg in ["armless_hvf", "armless_kvm", "armless_hw"] {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    let wanted = std::env::var_os("CARGO_FEATURE_HYPERVISOR").is_some();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let hvf = wanted && os == "macos" && arch == "aarch64";
    let kvm = wanted && os == "linux" && arch == "aarch64";
    if hvf {
        println!("cargo::rustc-cfg=armless_hvf");
    }
    if kvm {
        println!("cargo::rustc-cfg=armless_kvm");
    }
    if hvf || kvm {
        println!("cargo::rustc-cfg=armless_hw");
    }
}
