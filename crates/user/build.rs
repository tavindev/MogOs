fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/link.ld");
    // Segment file offsets aligned to 4 KiB, not lld's 64 KiB default, which pads every program to 64 KiB.
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
    println!("cargo:rerun-if-changed=link.ld");
}
