//! Compile the CO-RE BPF object and generate its Rust skeleton.
//!
//! This runs on every build (it needs clang and the committed `vmlinux.h`, not
//! root and not a BTF kernel). The generated skeleton only *loads* at runtime,
//! behind `probe()`, so a non-root / no-BTF box still builds and tests green.

use libbpf_cargo::SkeletonBuilder;
use std::env;
use std::path::PathBuf;

const SRC: &str = "src/bpf/drowse.bpf.c";

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("drowse.skel.rs");

    SkeletonBuilder::new()
        .source(SRC)
        // vmlinux.h lives next to the source; -I keeps CO-RE self-contained.
        .clang_args(["-I", "src/bpf", "-Wall", "-Wno-unused-function"])
        .build_and_generate(&out)
        .expect("bpf skeleton build failed");

    println!("cargo:rerun-if-changed={SRC}");
    println!("cargo:rerun-if-changed=src/bpf/vmlinux.h");
    println!("cargo:rerun-if-changed=build.rs");
}
