use std::path::Path;
use std::process::Command;
use std::{env, fs};

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");

    let root = Path::new(&dir).join("../../..");
    let user = root.join("crates/user");
    println!("cargo:rerun-if-changed={}", user.display());
    let target_dir = root.join("target/user");
    // A separate target dir, or the nested build waits on this build's lock; no clippy wrapper, so its builds stay cached.
    let status = Command::new(env::var("CARGO").unwrap())
        .args([
            "build",
            "--release",
            "--target",
            "aarch64-unknown-none-softfloat",
        ])
        .arg("--manifest-path")
        .arg(user.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .unwrap();
    assert!(status.success(), "user programs failed to build");

    let bin = target_dir.join("aarch64-unknown-none-softfloat/release");
    // The boot archive's files: every `crates/user` program, sorted so the archive is reproducible, plus a non-ELF.
    let mut files: Vec<_> = fs::read_dir(user.join("src/bin"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_stem().unwrap().to_str().unwrap().to_string();
            let data = fs::read(bin.join(&name)).unwrap();
            (name, data)
        })
        .collect();
    files.sort();
    files.push(("bad".into(), b"not an ELF".to_vec()));
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("boot.cpio");
    fs::write(out, cpio(&files)).unwrap();
}

/// A cpio archive in the newc format: per file, a 110-byte ASCII header, the name, the data, each padded to 4 bytes.
fn cpio(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let trailer = ("TRAILER!!!".into(), Vec::new());
    for (ino, (name, data)) in files.iter().chain([&trailer]).enumerate() {
        let mode = if data.is_empty() { 0 } else { 0o100_755 };
        // ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor, rdevmajor, rdevminor, namesize, check
        let fields = [
            ino,
            mode,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0,
        ];
        out.extend(b"070701");
        for field in fields {
            out.extend(format!("{field:08x}").bytes());
        }
        out.extend(name.bytes().chain([0]));
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}
