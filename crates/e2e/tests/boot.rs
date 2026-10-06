use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Builds the kernel, boots it in QEMU with `extra` arguments, and returns the exit status and serial lines.
fn boot(extra: &[&str]) -> (ExitStatus, Vec<String>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let build = Command::new(env!("CARGO"))
        .args(["build", "-p", "qemu-virt"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(build.success(), "kernel build failed");

    let mut qemu = Command::new("qemu-system-aarch64")
        .args([
            "-M",
            "virt",
            "-cpu",
            "cortex-a72",
            "-m",
            "128M",
            "-nographic",
            "-kernel",
        ])
        .arg(root.join("target/aarch64-unknown-none/debug/mog_os"))
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = qemu.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            qemu.kill().unwrap();
            panic!("QEMU timed out");
        }
        sleep(Duration::from_millis(50));
    };

    let mut out = String::new();
    qemu.stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    println!("{out}");
    let lines = out
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    (status, lines)
}

#[test]
fn boots_and_powers_off() {
    let (status, lines) = boot(&[]);
    for expected in [
        "MogOs: hello from EL1",
        "exceptions: ok",
        "ram: 0x40000000..0x48000000",
        "mmu: on",
        "heap: ok",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "missing line: {expected}"
        );
    }
    let free: usize = lines
        .iter()
        .find_map(|l| l.strip_prefix("frames: ")?.strip_suffix(" free"))
        .expect("missing frames line")
        .parse()
        .unwrap();
    assert!(free > 0 && free < 32768, "frames free: {free}");
    lines
        .iter()
        .find_map(|l| l.strip_prefix("boot: ")?.strip_suffix(" us"))
        .expect("missing boot line")
        .parse::<u64>()
        .unwrap();
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn unmapped_access_reports_data_abort() {
    let (status, lines) = boot(&["-append", "test=mmu-fault"]);
    assert!(
        lines.iter().any(|l| l == "mmu: on"),
        "missing line: mmu: on"
    );
    let fault = lines
        .iter()
        .find(|l| l.contains("FAR_EL1=0x80000000"))
        .expect("missing fault report");
    let esr = fault
        .split_once("ESR_EL1=0x")
        .and_then(|(_, rest)| rest.split(' ').next())
        .expect("missing ESR_EL1");
    let esr = u64::from_str_radix(esr, 16).unwrap();
    assert_eq!(esr >> 26, 0x25, "not a data abort from EL1: {fault}");
    assert_eq!(
        esr & 0x3f,
        0b00_0101,
        "not a level-1 translation fault: {fault}"
    );
    assert!(status.success(), "QEMU exited with {status}");
}
