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
        .arg(root.join("target/aarch64-unknown-none-softfloat/debug/mog_os"))
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
    assert!(
        !lines.iter().any(|l| l.contains("panic:")),
        "kernel panicked"
    );
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

#[test]
fn tasks_alternate_on_yield() {
    let (status, lines) = boot(&["-append", "test=yield"]);
    let tasks: Vec<_> = lines.iter().filter(|l| l.starts_with("task ")).collect();
    assert_eq!(
        tasks,
        [
            "task a: 0",
            "task b: 0",
            "task a: 1",
            "task b: 1",
            "task a: 2",
            "task b: 2",
        ]
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn timer_preempts_spinning_task() {
    let (status, lines) = boot(&["-append", "test=preempt"]);
    let tasks: Vec<_> = lines.iter().filter(|l| l.starts_with("task ")).collect();
    assert_eq!(tasks, ["task b: 0", "task b: 1", "task b: 2"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn faulting_process_is_killed_and_others_keep_running() {
    let (status, lines) = boot(&["-append", "test=user"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let a: Vec<_> = lines
        .iter()
        .filter(|l| l.starts_with("A: "))
        .cloned()
        .collect();
    let mut expected = vec!["A: bad pointers rejected".to_string()];
    expected.extend((0..10).map(|i| format!("A: {i}")));
    assert_eq!(a, expected);
    let fault = |line: &str| {
        lines
            .iter()
            .position(|l| l == line)
            .unwrap_or_else(|| panic!("missing line: {line}"))
    };
    // B: data abort (EC 0x24) on A's code address; C, in B's reused slot and ASID: on kernel RAM.
    let b = fault("fault: 2 ec=0x24 far=0x100000000");
    let c = fault("fault: 2 ec=0x24 far=0x40000000");
    let last = fault("A: 9");
    assert!(b < last, "B was not killed while A was running");
    assert!(b < c, "C ran before B was killed");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn handles_enforce_rights_and_generations() {
    let (status, lines) = boot(&["-append", "test=handles"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let h: Vec<_> = lines.iter().filter(|l| l.starts_with("H: ")).collect();
    assert_eq!(
        h,
        [
            "H: console write ok",
            "H: dup without write: EACCES",
            "H: closed handle: EBADF",
            "H: stale handle: EBADF",
        ]
    );
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn syscall_bench_reports_round_trip() {
    let (status, lines) = boot(&["-append", "test=bench-syscall"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    lines
        .iter()
        .find_map(|l| l.strip_prefix("syscall: ")?.strip_suffix(" ns/round-trip"))
        .expect("missing syscall line")
        .parse::<u64>()
        .unwrap();
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn map_stops_at_budget_and_exit_returns_every_frame() {
    let (status, lines) = boot(&["-append", "test=budget"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let m: Vec<_> = lines.iter().filter(|l| l.starts_with("M: ")).collect();
    // Budget 25 minus 9 fixed frames (3 tables, code, stack, 4 kernel stack) minus the map region's level-3 table;
    // a rollback leak in the failed 16-page map before the loop would lower it.
    assert_eq!(m, ["M: ENOMEM after 15 pages", "M: still running"]);
    assert_no_leak(&lines, "budget");
    assert!(status.success(), "QEMU exited with {status}");
}

/// Asserts the kernel's `<test>: free frames <n> before, <n> after` line shows the same count twice.
fn assert_no_leak(lines: &[String], test: &str) {
    let (before, after) = lines
        .iter()
        .find_map(|l| {
            l.strip_prefix(&format!("{test}: free frames "))?
                .split_once(" before, ")
        })
        .unwrap_or_else(|| panic!("missing {test} free frames line"));
    assert_eq!(after, format!("{before} after"), "frames leaked");
}

#[test]
fn spawn_moves_handles_and_budget_to_the_child() {
    let (status, lines) = boot(&["-append", "test=spawn"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let s: Vec<_> = lines
        .iter()
        .filter(|l| l.starts_with("S: ") || l.starts_with("C: "))
        .collect();
    // The parent never yields and the timer is off, so all its lines precede the child's.
    assert_eq!(
        s,
        [
            "S: open missing: ENOENT",
            "S: empty write: 0",
            "S: spawn non-ELF: ENOEXEC",
            "S: spawn over budget: ENOMEM",
            "S: spawn without handles over budget: ENOMEM",
            "S: spawn one frame short: ENOMEM",
            "S: console not moved",
            "S: spawned child with the console",
            "S: moved console: EBADF",
            "C: hello through handle 0",
            "C: statics work",
            "C: handle 1 not given: EBADF",
        ]
    );
    // The one-frame-short spawn fails after mapping everything but the kernel stack, so this checks its rollback.
    assert_no_leak(&lines, "spawn");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn parent_blocks_on_an_empty_pipe_until_the_child_writes() {
    let (status, lines) = boot(&["-append", "test=pipe"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let rw: Vec<_> = lines
        .iter()
        .filter(|l| l.starts_with("R: ") || l.starts_with("W: "))
        .collect();
    // The timer is off, so the writer runs only because the reader blocked on the empty pipe, and the reader runs
    // again only once the writer exited.
    assert_eq!(
        rw,
        [
            "R: spawned writer",
            "R: reading the empty pipe",
            "W: writing to the pipe",
            "W: exiting with 7",
            "R: read: hello",
            "R: EOF",
            "R: writer exited with 7",
            "R: budget returned: spawned writer again",
            "R: stale process handle: EBADF",
            "R: second writer exited with 7",
        ]
    );
    assert_no_leak(&lines, "pipe");
    assert!(status.success(), "QEMU exited with {status}");
}
