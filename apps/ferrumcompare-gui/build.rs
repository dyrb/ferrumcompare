//! Embed the manifest, the icon and the version information with the MSVC linker itself
//! (no resource compiler, no extra crates).

#[path = "res.rs"]
mod res;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let name = std::env::var("CARGO_PKG_NAME").unwrap();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(format!("{name}.res"));
    let resources = std::path::Path::new(&dir).join("../ferrumcompare-gui");
    let manifest = resources.join("app.manifest");
    for file in ["app.manifest", "res.rs", "build.rs"] {
        println!("cargo:rerun-if-changed={}", resources.join(file).display());
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    res::write(
        &out,
        &res::Version {
            file_description: "FerrumCompare",
            internal_name: &name,
            original_filename: &format!("{name}.exe"),
            product_name: "FerrumCompare",
            version: &version,
            copyright: "Copyright (c) 2026 FerrumCompare contributors",
        },
    )
    .expect("write resources");
    println!("cargo:rustc-link-arg-bins={}", out.display());
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    // The manifest carries its own trustInfo; do not let the linker add another.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}
