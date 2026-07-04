//! Compile the C++ image-processing bridge (see `src/imageproc.rs` and `cpp/`).
//!
//! `cxx_build` generates the glue between the `#[cxx::bridge]` module and our
//! C++ wrappers, then compiles both plus `cpp/imageproc.cpp` into a static lib
//! that links into the crate. The manifest dir is on the include path so the
//! bridge's `include!("cpp/imageproc.h")` and the `.cpp`'s own include resolve.

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");

    cxx_build::bridge("src/imageproc.rs")
        .file("cpp/imageproc.cpp")
        .include(&manifest)
        .std("c++17")
        .compile("cim_imageproc");

    println!("cargo:rerun-if-changed=src/imageproc.rs");
    println!("cargo:rerun-if-changed=cpp/imageproc.cpp");
    println!("cargo:rerun-if-changed=cpp/imageproc.h");
}
