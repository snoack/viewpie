// libdrm and libav are plain C libraries with public headers, so the bindings
// are generated from those headers rather than written out by hand.
//
// This is not incidental. AVFormatContext, AVStream and AVCodecContext are
// fully declared in the headers, and every field offset in them differs
// between a 64-bit Pi 5 and a 32-bit Pi 3 -- streams sits at 48 against 28,
// hw_device_ctx at 560 against 488. Reaching a field by a hand-written offset
// is a garbage pointer on whichever board the number was not written for, and
// a garbage pointer is a crash at runtime rather than an error at build time.
// bindgen reads the target's own headers and gets this right by construction.
use std::path::PathBuf;
use std::process::Command;

// libdrm is the `drm` crate's business; bindgen only needs libav.
const LIBS: [&str; 3] = ["libavcodec", "libavformat", "libavutil"];

fn main() {
    let mut cflags: Vec<String> = Vec::new();
    for lib in LIBS {
        emit_link_flags(lib);
        cflags.extend(pkg_config(lib, "--cflags"));
    }

    let bindings = bindgen::Builder::default()
        .header("src/wrapper.h")
        .clang_args(&cflags)
        // Only what the crate actually calls. Binding the whole of libav
        // produces tens of thousands of items and a minute of build time.
        .allowlist_function("av_.*|avcodec_.*|avformat_.*")
        .allowlist_type("AV.*|cec_.*")
        .allowlist_var("AV_.*|AVERROR.*|CEC_.*")
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("could not generate bindings for libav and linux/cec.h");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("could not write bindings.rs");

    println!("cargo:rerun-if-changed=src/wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
}

fn pkg_config(lib: &str, what: &str) -> Vec<String> {
    let out = Command::new("pkg-config")
        .args([what, lib])
        .output()
        .unwrap_or_else(|e| panic!("pkg-config {lib}: {e}"));
    if !out.status.success() {
        panic!("pkg-config {lib} failed; install the -dev package");
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect()
}

fn emit_link_flags(lib: &str) {
    for flag in pkg_config(lib, "--libs") {
        if let Some(name) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}
