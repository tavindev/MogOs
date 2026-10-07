use std::path::Path;
use std::process::Command;

/// `crates/user` is outside the workspace, so `cargo test-host` reaches its host tests (msh's command table) here.
#[test]
fn user_host_tests_pass() {
    let user = Path::new(env!("CARGO_MANIFEST_DIR")).join("../user");
    let status = Command::new(env!("CARGO"))
        .args(["test", "--target", "aarch64-apple-darwin", "--lib"])
        .arg("--manifest-path")
        .arg(user.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(user.join("../../target/user"))
        .status()
        .unwrap();
    assert!(status.success(), "crates/user host tests failed");
}
