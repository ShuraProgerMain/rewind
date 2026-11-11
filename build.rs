use std::{env, path::PathBuf};

fn main() {
    let ff = PathBuf::from(env::var("FFMPEG_DIR").expect("FFMPEG_DIR is not set (see .cargo/config.toml)"));
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rustc-link-search=native={}", ff.join("lib").display());
    for lib in ["avcodec", "avformat", "avutil", "swresample"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", ff.join("include").display()))
        .allowlist_function("av_.*|avcodec_.*|avformat_.*|avio_.*|swr_.*")
        .allowlist_type("AV.*|Swr.*")
        .allowlist_var("AV_.*|AVERROR.*|AVFMT_.*|AVIO_.*|FF_.*")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .layout_tests(false)
        .generate_comments(false)
        .derive_debug(false)
        .generate()
        .expect("bindgen failed");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ffmpeg.rs");
    bindings.write_to_file(out).expect("cannot write bindings");

    // The version the program reports. REWIND_VERSION overrides Cargo's (used to build a
    // "newer" copy when testing updates).
    println!("cargo:rerun-if-env-changed=REWIND_VERSION");
    let version = env::var("REWIND_VERSION").unwrap_or_else(|_| env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=REWIND_VERSION={version}");

    // Icon and file properties of the exe (what Explorer, the taskbar and Start show).
    println!("cargo:rerun-if-changed=assets/rewind.ico");
    let mut resources = winresource::WindowsResource::new();
    resources.set_icon("assets/rewind.ico");
    resources.set("ProductName", "Rewind");
    resources.set("FileDescription", "Rewind");
    resources.set("LegalCopyright", "ShuraProger");
    resources.set("FileVersion", &version);
    resources.set("ProductVersion", &version);
    resources.compile().expect("cannot compile the exe resources");
}
