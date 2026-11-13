// The `hypervisor` feature runs guests in hardware where the host has a hypervisor armless can
// drive: Hypervisor.framework on macOS with Apple silicon (cfg `armless_hvf`), KVM on Linux arm64
// (cfg `armless_kvm`); `armless_hw` is either. Elsewhere (x86_64, wasm) the feature does nothing.
//
// Native hosts (not wasm, where build-wasm.sh links it in) decode zstd with the same C decoder the
// wasm build uses, lib/zstd/zstddeclib.c, compiled here; src/zstd.rs provides its malloc and free.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=lib/zstd/zstddeclib.c");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("wasm32") {
        cc::Build::new()
            .file("lib/zstd/zstddeclib.c")
            .opt_level(3)
            .warnings(false)
            // Not exported from binaries and shared libraries: no clash with a libzstd of the process
            .define("ZSTDLIB_VISIBILITY", "")
            .define("ZSTDERRORLIB_VISIBILITY", "")
            .flag_if_supported("-fvisibility=hidden")
            .compile("zstddeclib");
    }
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
