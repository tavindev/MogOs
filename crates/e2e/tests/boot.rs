use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, Once};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

/// Builds the kernel, boots it in QEMU with `extra` arguments, and returns the exit status and serial lines.
fn boot(extra: &[&str]) -> (ExitStatus, Vec<String>) {
    boot_with_input(extra, None)
}

/// As `boot`; with `input` = (`ready`, `chunks`), writes chunk `i` to QEMU's stdin once the output contains `ready`
/// `i + 1` times.
fn boot_with_input(extra: &[&str], input: Option<(&str, &[&[u8]])>) -> (ExitStatus, Vec<String>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Once per run: even a fresh `cargo build` replaces `mog_os`, so a build beside a booting test can leave QEMU no ELF.
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let build = Command::new(env!("CARGO"))
            .args(["build", "-p", "qemu-virt"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(build.success(), "kernel build failed");
    });

    let mut qemu = Command::new("qemu-system-aarch64")
        .args([
            "-M",
            "virt",
            "-cpu",
            "cortex-a72",
            "-m",
            "128M",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-global",
            "virtio-mmio.ioeventfd=off",
            "-nographic",
            "-kernel",
        ])
        .arg(root.join("target/aarch64-unknown-none-softfloat/debug/mog_os"))
        .args(extra)
        .stdin(match input {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let (out, out_reader) = drain(qemu.stdout.take().unwrap());
    let (err, err_reader) = drain(qemu.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut sent = 0;
    let status = loop {
        if let Some(status) = qemu.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            qemu.kill().unwrap();
            qemu.wait().unwrap();
            break None;
        }
        if let Some((ready, chunks)) = input
            && sent < chunks.len()
            && String::from_utf8_lossy(&out.lock().unwrap())
                .matches(ready)
                .count()
                > sent
        {
            let _ = qemu.stdin.as_mut().unwrap().write_all(chunks[sent]);
            sent += 1;
        }
        sleep(Duration::from_millis(50));
    };

    out_reader.join().unwrap();
    err_reader.join().unwrap();
    let out = String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
    let err = String::from_utf8_lossy(&err.lock().unwrap()).into_owned();
    // Captured, so a failing test shows how QEMU ended.
    println!("QEMU status: {status:?}\nQEMU stderr:\n{err}\nQEMU stdout:\n{out}");
    let status = status.expect("QEMU timed out");
    let lines = out
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    (status, lines)
}

/// Collects everything `pipe` yields, on a thread that ends at EOF.
fn drain(mut pipe: impl Read + Send + 'static) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let reader = thread::spawn({
        let out = out.clone();
        move || {
            let mut buf = [0; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => panic!("reading QEMU's output: {e}"),
                }
            }
        }
    });
    (out, reader)
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
        "disk: none",
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
            "S: spawn 4097 bytes of args: E2BIG",
            "S: spawn 33 args: E2BIG",
            "S: spawn args without a NUL: EINVAL",
            "S: spawn with args one frame short: ENOMEM",
            "S: console not moved",
            "S: spawned child with the console",
            "S: moved console: EBADF",
            "C: hello through handle 0",
            "C: statics work",
            "C: handle 1 not given: EBADF",
            "C: 32 args of 4096 bytes: child, a b",
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
    // again only once the writer exited. The second writer prints nothing (its handle 0 is a write end without write)
    // and runs only because the reader's `wait` blocked on it. An 8 KiB write or read moves at most 4 KiB.
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
            "R: EOF after the second writer exited",
            "R: 8 KiB write: 4096",
            "R: 8 KiB read: 4096",
        ]
    );
    assert_no_leak(&lines, "pipe");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn an_exited_child_keeps_its_slot_until_waited_for() {
    let (status, lines) = boot(&["-append", "test=wait"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let pc: Vec<_> = lines
        .iter()
        .filter(|l| l.starts_with("P: ") || l.starts_with("C: "))
        .collect();
    // A exits (waking the parent blocked on a pipe whose only write end A held) before B is spawned, so B would
    // take A's slot if exit freed it. B runs only once the parent's `wait` on it blocks.
    assert_eq!(
        pc,
        [
            "P: spawned A",
            "P: EOF once A exits",
            "P: spawned B",
            "P: A exited with 7",
            "C: hello through handle 0",
            "C: statics work",
            "C: handle 1 not given: EBADF",
            "P: B exited with 0",
            "P: both budgets returned",
            "P: third child exited",
            "P: close returned its budget",
        ]
    );
    assert_no_leak(&lines, "wait");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn priority_inheritance_lets_the_mutex_owner_outrun_a_middle_priority_spinner() {
    let (status, lines) = boot(&["-append", "test=pi"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let pi: Vec<_> = lines
        .iter()
        .filter(|l| {
            ["L: ", "M: ", "H: ", "P: "]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .collect();
    // The timer is on, yet M never runs: something outranks it at every switch. L (priority 1) unlocks only while
    // H's block lends it priority 3; without that, M (2) spins forever and the boot never powers off. Unlocking hands
    // the mutex to H at once, so L relocks only after H is done and M is killed.
    assert_eq!(
        pi,
        [
            "L: locked",
            "L: relock: EDEADLK",
            "H: unlock while L owns it: EPERM",
            "H: locking",
            "L: unlocking",
            "H: acquired",
            "P: high exited",
            "P: mid killed",
            "L: relocked",
            "P: low exited",
        ]
    );
    assert_no_leak(&lines, "pi");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn pipe_bench_reports_round_trip() {
    let (status, lines) = boot(&["-append", "test=bench-pipe"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    assert!(
        lines.iter().any(|l| l == "ping: done"),
        "ping did not finish every round trip"
    );
    lines
        .iter()
        .find_map(|l| l.strip_prefix("pipe: ")?.strip_suffix(" ns/round-trip"))
        .expect("missing pipe line")
        .parse::<u64>()
        .unwrap();
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn lock_bench_reports_round_trips_and_an_exact_count() {
    let (status, lines) = boot(&["-append", "test=bench-lock"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for lock in ["ticket", "test-and-set"] {
        lines
            .iter()
            .find_map(|l| {
                l.strip_prefix(&format!("lock: {lock} "))?
                    .strip_suffix(" ns/round-trip")
            })
            .unwrap_or_else(|| panic!("missing {lock} line"))
            .parse::<f64>()
            .unwrap();
    }
    let first = lines
        .iter()
        .find_map(|l| l.strip_prefix("lock: adder done at "))
        .expect("missing adder line")
        .parse::<u64>()
        .unwrap();
    assert!(first > 10_000_000, "the timer never interleaved the adders");
    assert!(
        lines.iter().any(|l| l == "lock: count 20000000"),
        "the two adders' count is not exact"
    );
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn console_reads_edited_lines_typed_ahead() {
    console_echo(&[]);
}

#[test]
fn console_input_reaches_core_0_on_four_cores() {
    console_echo(&["-smp", "4"]);
}

#[test]
fn every_core_comes_online_and_takes_a_timer_tick() {
    let (status, lines) = boot(&["-smp", "4", "-append", "test=smp"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for cpu in 0..4 {
        let online = format!("cpu {cpu}: online");
        assert!(lines.contains(&online), "missing line: {online}");
    }
    assert!(
        lines.iter().any(|l| l == "smp: 4 cpus ticked"),
        "missing line: smp: 4 cpus ticked"
    );
    assert!(status.success(), "QEMU exited with {status}");
}

fn console_echo(extra: &[&str]) {
    // Both lines in one write once `E: ready` is out, when the first read is already blocked.
    let input = Some(("E: ready", &[&b"hel\x7flo\rbye\r"[..]][..]));
    let (status, lines) = boot_with_input(&[extra, &["-append", "test=echo"]].concat(), input);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let start = lines
        .iter()
        .position(|l| l == "E: ready")
        .expect("missing ready line");
    let console: Vec<_> = lines[start..].iter().take(5).collect();
    assert_eq!(
        console,
        [
            "E: ready",
            "hel\u{8} \u{8}lo",
            "bye",
            "got: helo",
            "got: bye"
        ]
    );
    assert_no_leak(&lines, "echo");
    assert!(status.success(), "QEMU exited with {status}");
}

/// A zeroed raw disk image of `blocks` 4 KiB blocks in the temp dir, unique to `test`.
fn disk_image(test: &str, blocks: u64) -> PathBuf {
    let path = std::env::temp_dir().join(format!("mogos-{test}-{}.img", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(blocks * 4096).unwrap();
    path
}

/// Boots with `image` attached as a virtio-blk device and `test` as the boot argument.
fn boot_with_disk(image: &Path, test: &str) -> (ExitStatus, Vec<String>) {
    let drive = format!("file={},if=none,format=raw,id=d0", image.display());
    boot(&[
        "-drive",
        &drive,
        "-device",
        "virtio-blk-device,drive=d0",
        "-append",
        test,
    ])
}

/// A `-blockdev` value for `image` as drive `d0` behind blkdebug, which fails every host flush with EIO.
fn flush_fails(image: &Path) -> String {
    format!(
        r#"{{"driver":"raw","node-name":"d0","file":{{"driver":"blkdebug","inject-error":[{{"event":"flush_to_disk","errno":5}}],"image":{{"driver":"file","filename":"{}"}}}}}}"#,
        image.display()
    )
}

#[test]
fn a_flushed_block_survives_a_reboot() {
    let image = disk_image("disk", 16);
    let (status, lines) = boot_with_disk(&image, "test=disk");
    assert!(status.success(), "QEMU exited with {status}");
    let disk: Vec<_> = lines.iter().filter(|l| l.starts_with("disk: ")).collect();
    assert_eq!(disk, ["disk: 16 blocks", "disk: wrote"]);
    // Blocks 1 and 2 are bytes 4096..12288: a driver addressing 512-byte sectors by block number would miss them.
    let bytes = std::fs::read(&image).unwrap();
    let expected: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
    assert!(
        bytes[4096..12288] == expected[..],
        "blocks 1 and 2 not on the image"
    );

    let (status, lines) = boot_with_disk(&image, "test=disk");
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    let disk: Vec<_> = lines.iter().filter(|l| l.starts_with("disk: ")).collect();
    assert_eq!(disk, ["disk: 16 blocks", "disk: read ok"]);

    // A fresh image behind blkdebug, which fails every host flush with EIO: the kernel must see it, so it flushed.
    let image = disk_image("disk-flush", 16);
    let blockdev = flush_fails(&image);
    let (status, lines) = boot(&[
        "-blockdev",
        &blockdev,
        "-device",
        "virtio-blk-device,drive=d0",
        "-append",
        "test=disk",
    ]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    let disk: Vec<_> = lines.iter().filter(|l| l.starts_with("disk: ")).collect();
    assert_eq!(disk, ["disk: 16 blocks", "disk: flush failed"]);
}

#[test]
fn disk_bench_reports_throughput() {
    let image = disk_image("bench-disk", 2048);
    let (status, lines) = boot_with_disk(&image, "test=bench-disk");
    std::fs::remove_file(&image).unwrap();
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for op in [
        "4 KiB write+flush",
        "4 KiB read",
        "256 KiB write+flush",
        "256 KiB read",
    ] {
        lines
            .iter()
            .find_map(|l| {
                l.strip_prefix(&format!("disk: {op} "))?
                    .strip_suffix(" MiB/s")
            })
            .unwrap_or_else(|| panic!("missing disk {op} line"))
            .parse::<u64>()
            .unwrap();
    }
    assert!(status.success(), "QEMU exited with {status}");
}

/// A raw image file as a `mogfs::Disk`, like `crates/mogfs/examples/mkfs.rs`.
struct FileDisk(std::fs::File, u64);

impl mogfs::Disk for FileDisk {
    fn read(&mut self, block: u64, bufs: &mut [[u8; 4096]]) -> Result<(), mogfs::Error> {
        self.0
            .read_exact_at(bufs.as_flattened_mut(), block * 4096)
            .map_err(|_| mogfs::Error::Io)
    }

    fn write(&mut self, block: u64, bufs: &[[u8; 4096]]) -> Result<(), mogfs::Error> {
        self.0
            .write_all_at(bufs.as_flattened(), block * 4096)
            .map_err(|_| mogfs::Error::Io)
    }

    fn flush(&mut self) -> Result<(), mogfs::Error> {
        self.0.sync_data().map_err(|_| mogfs::Error::Io)
    }

    fn blocks(&self) -> u64 {
        self.1
    }
}

/// As `disk_image`, formatted as an empty MogFS.
fn mogfs_image(test: &str, blocks: u64) -> PathBuf {
    let path = disk_image(test, blocks);
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    mogfs::Fs::new(FileDisk(file, blocks)).format().unwrap();
    path
}

/// Boots `test=shell` on `image`, typing each command once msh prompts for it; returns the exit status and each
/// command with the lines msh printed for it.
fn shell(image: &Path, commands: &[&str]) -> (ExitStatus, Vec<(String, Vec<String>)>) {
    let drive = format!("file={},if=none,format=raw,id=d0", image.display());
    shell_on(&["-drive", &drive], commands)
}

/// As `shell`, with the drive `d0` given by `drive` (QEMU arguments).
fn shell_on(drive: &[&str], commands: &[&str]) -> (ExitStatus, Vec<(String, Vec<String>)>) {
    let typed: Vec<Vec<u8>> = commands.iter().map(|c| format!("{c}\r").into()).collect();
    let chunks: Vec<&[u8]> = typed.iter().map(Vec::as_slice).collect();
    let args = [
        drive,
        &[
            "-device",
            "virtio-blk-device,drive=d0",
            "-append",
            "test=shell",
        ],
    ]
    .concat();
    let (status, lines) = boot_with_input(&args, Some(("msh> ", &chunks)));
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    assert_no_leak(&lines, "shell");
    let mut session: Vec<(String, Vec<String>)> = Vec::new();
    for line in lines.iter().take_while(|l| !l.starts_with("shell: ")) {
        match line.strip_prefix("msh> ") {
            Some(command) => session.push((command.into(), Vec::new())),
            None => {
                if let Some((_, out)) = session.last_mut() {
                    out.push(line.clone());
                }
            }
        }
    }
    (status, session)
}

fn session(expected: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    expected
        .iter()
        .map(|(c, out)| (c.to_string(), out.iter().map(|l| l.to_string()).collect()))
        .collect()
}

#[test]
fn shell_files_survive_a_reboot_only_once_synced() {
    let image = mogfs_image("shell", 1024);
    let (status, boot1) = shell(
        &image,
        &[
            "mkdir docs",
            "cd docs",
            "pwd",
            "write a.txt hello",
            "cd ..",
            "mv docs/a.txt docs/b.txt",
            "mkdir tmp",
            "rm tmp",
            "write c.txt cross",
            "mkdir sub",
            "mv c.txt docs/c.txt",
            "mv sub docs/sub",
            "echo hi  there",
            "sync",
            "write docs/late.txt x",
            "ls docs",
            "frob",
            "mid",
            "msh",
            "help",
            "exit",
        ],
    );
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        boot1,
        session(&[
            ("mkdir docs", &[]),
            ("cd docs", &[]),
            ("pwd", &["/docs"]),
            ("write a.txt hello", &[]),
            ("cd ..", &[]),
            ("mv docs/a.txt docs/b.txt", &[]),
            ("mkdir tmp", &[]),
            ("rm tmp", &[]),
            ("write c.txt cross", &[]),
            ("mkdir sub", &[]),
            ("mv c.txt docs/c.txt", &[]),
            ("mv sub docs/sub", &[]),
            ("echo hi  there", &["hi there"]),
            ("sync", &[]),
            ("write docs/late.txt x", &[]),
            ("ls docs", &["b.txt", "c.txt", "sub/", "late.txt"]),
            ("frob", &["msh: frob: command not found"]),
            // Archive programs outside msh's command table do not run.
            ("mid", &["msh: mid: command not found"]),
            ("msh", &["msh: msh: command not found"]),
            (
                "help",
                &[
                    "builtins: cd pwd exit help",
                    "commands: cat ls echo sync mkdir rm touch write mv sh",
                ],
            ),
            ("exit", &[]),
        ])
    );

    // late.txt was written but never synced, so the reboot drops it; this boot commits after mounting.
    let (status, boot2) = shell(
        &image,
        &[
            "ls",
            "ls docs",
            "cat docs/b.txt",
            "cat docs/c.txt",
            "cat ../x",
            "cat /x",
            "rm docs",
            "rm docs/b.txt",
            "rm docs/c.txt",
            "rm docs/sub",
            "sync",
            "ls docs",
            "exit",
        ],
    );
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        boot2,
        session(&[
            ("ls", &["docs/"]),
            ("ls docs", &["b.txt", "c.txt", "sub/"]),
            ("cat docs/b.txt", &["hello"]),
            ("cat docs/c.txt", &["cross"]),
            ("cat ../x", &["msh: cat: EINVAL"]),
            ("cat /x", &["msh: cat: EINVAL"]),
            ("rm docs", &["msh: rm: ENOTEMPTY"]),
            ("rm docs/b.txt", &[]),
            ("rm docs/c.txt", &[]),
            ("rm docs/sub", &[]),
            ("sync", &[]),
            ("ls docs", &[]),
            ("exit", &[]),
        ])
    );

    let (status, boot3) = shell(
        &image,
        &[
            "cd docs", "ls", "cd nope", "touch f", "cd f", "pwd", "cd", "pwd", "ls", "exit",
        ],
    );
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        boot3,
        session(&[
            ("cd docs", &[]),
            ("ls", &[]),
            ("cd nope", &["msh: cd: ENOENT"]),
            ("touch f", &[]),
            ("cd f", &["msh: cd: ENOTDIR"]),
            ("pwd", &["/docs"]),
            ("cd", &[]),
            ("pwd", &["/"]),
            ("ls", &["docs/"]),
            ("exit", &[]),
        ])
    );
}

#[test]
fn busybox_sh_changes_files_that_survive_a_reboot_once_synced() {
    let image = mogfs_image("busybox", 1024);
    let script = "sh -c 'mkdir d; echo hi > d/f; echo x > d/x; mv d/x d/y; rm d/y; cat d/f; ls d; sync; echo late > d/g'";
    let (status, boot1) = shell(&image, &[script, "exit"]);
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(boot1, session(&[(script, &["hi", "f"]), ("exit", &[])]));

    // d/g was written after the sync, so the reboot drops it.
    let script = "sh -c 'cat d/f; ls d; cd d; cat f'";
    let (status, boot2) = shell(&image, &[script, "exit"]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        boot2,
        session(&[(script, &["hi", "f", "hi"]), ("exit", &[])])
    );
}

#[test]
fn busybox_redirects_and_pipes_reach_spawned_programs_and_runs_only_c_programs() {
    let image = mogfs_image("busybox-io", 1024);
    let script = "sh -c 'echo a > f; cat < f; mkdir d; echo 1 > d/aaaa; echo 2 > d/b; ls d > out; echo x >> out; cat out; echo hi | cat; mid'";
    let (status, got) = shell(&image, &[script, "exit"]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        got,
        session(&[
            (
                script,
                &[
                    "a",
                    "aaaa",
                    "b",
                    "x",
                    "hi",
                    "sh: can't execute 'mid': No such file or directory",
                    "msh: sh: 127"
                ]
            ),
            ("exit", &[]),
        ])
    );
}

#[test]
fn a_c_program_on_musl_prints_gets_enosys_and_exits_with_its_code() {
    let image = mogfs_image("hello", 1024);
    let (status, got) = shell(&image, &["sh -c hello", "exit"]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        got,
        session(&[
            (
                "sh -c hello",
                &[
                    "hello from musl: hello, 1 args",
                    "syscall 999: ENOSYS",
                    "fork: ENOSYS",
                    "msh: sh: 42",
                ]
            ),
            ("exit", &[]),
        ])
    );
}

#[test]
fn musl_bench_reports_round_trips() {
    let image = mogfs_image("cbench", 1024);
    let (status, got) = shell(&image, &["sh -c cbench", "exit"]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    let lines = &got[0].1;
    for (i, prefix) in ["musl syscall: ", "busybox spawn: "].iter().enumerate() {
        let ns: u64 = lines[i]
            .strip_prefix(prefix)
            .and_then(|l| l.strip_suffix(" ns/round-trip"))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no {prefix} line in {lines:?}"));
        assert!(ns > 0);
    }
}

#[test]
fn oscb_runs_the_cross_os_benchmarks() {
    let image = mogfs_image("oscb", 1024);
    let script = "sh -c 'oscb syscalls / oscnop; oscb pipe / oscnop; oscb spawn / oscnop; oscb files / oscnop'";
    let (status, got) = shell(&image, &[script, "exit"]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    let names: Vec<&str> = got[0]
        .1
        .iter()
        .map(|l| {
            l.strip_prefix("oscb: ")
                .and_then(|l| l.split(' ').next())
                .unwrap_or(l)
        })
        .collect();
    assert_eq!(
        names,
        [
            "getppid",
            "write0",
            "pipe",
            "spawn",
            "create+write+fsync",
            "open+close"
        ]
    );
}

#[test]
fn sync_reports_a_failed_flush() {
    let image = mogfs_image("shell-flush", 1024);
    let blockdev = flush_fails(&image);
    let (status, got) = shell_on(&["-blockdev", &blockdev], &["mkdir x", "sync", "exit"]);
    std::fs::remove_file(&image).unwrap();
    assert!(status.success(), "QEMU exited with {status}");
    assert_eq!(
        got,
        session(&[
            ("mkdir x", &[]),
            ("sync", &["msh: sync: EIO"]),
            ("exit", &[])
        ])
    );
}

#[test]
fn fs_bench_reports_round_trips() {
    let image = mogfs_image("bench-fs", 1024);
    let (status, lines) = boot_with_disk(&image, "test=bench-fs");
    std::fs::remove_file(&image).unwrap();
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for op in ["open+write+sync", "open+close"] {
        lines
            .iter()
            .find_map(|l| {
                l.strip_prefix(&format!("{op}: "))?
                    .strip_suffix(" ns/round-trip")
            })
            .unwrap_or_else(|| panic!("missing {op} line"))
            .parse::<u64>()
            .unwrap();
    }
    assert_no_leak(&lines, "bench-fs");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn spawn_bench_reports_round_trip() {
    let (status, lines) = boot(&["-append", "test=bench-spawn"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for op in ["spawn", "spawn+args"] {
        lines
            .iter()
            .find_map(|l| {
                l.strip_prefix(&format!("{op}: "))?
                    .strip_suffix(" ns/round-trip")
            })
            .unwrap_or_else(|| panic!("missing {op} line"))
            .parse::<u64>()
            .unwrap();
    }
    assert_no_leak(&lines, "bench-spawn");
    assert!(status.success(), "QEMU exited with {status}");
}

/// The `bench <name>: <ns> ns` lines' times by name, in boot order.
fn bench_lines(lines: &[String]) -> Vec<(String, f64)> {
    lines
        .iter()
        .filter_map(|l| {
            let (name, ns) = l.strip_prefix("bench ")?.rsplit_once(": ")?;
            Some((name.to_string(), ns.strip_suffix(" ns")?.parse().ok()?))
        })
        .collect()
}

#[test]
fn syscall_benches_report_every_call() {
    let image = mogfs_image("bench-syscalls", 1024);
    let (status, lines) = boot_with_disk(&image, "test=bench-syscalls");
    std::fs::remove_file(&image).unwrap();
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let names: Vec<_> = bench_lines(&lines).into_iter().map(|b| b.0).collect();
    assert_eq!(
        names,
        [
            "console-write",
            "console-read",
            "pipe-write",
            "pipe-read",
            "file-write",
            "file-read",
            "dup",
            "close",
            "open",
            "open-create",
            "open-trunc",
            "mkdir",
            "readdir",
            "unlink",
            "rename",
            "sync",
            "sync-change",
            "map",
            "pipe",
            "spawn",
            "spawn-args",
            "wait",
            "kill",
            "mutex",
            "lock",
            "unlock",
            "enosys",
        ]
    );
    assert_no_leak(&lines, "bench-syscalls");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn shell_bench_times_each_command_from_spawn_to_reap() {
    let image = mogfs_image("bench-shell", 1024);
    let (status, lines) = boot_with_disk(&image, "test=bench-shell");
    std::fs::remove_file(&image).unwrap();
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("panic:") || l.starts_with("msh: ")),
        "kernel panicked or a command failed"
    );
    let commands = [
        "ls d1",
        "ls d100",
        "ls d390",
        "cat small",
        "cat big",
        "write w hello",
        "mkdir m",
        "rm m",
        "mv a b",
        "mv b a",
        "echo hi",
    ];
    let names: Vec<_> = bench_lines(&lines).into_iter().map(|b| b.0).collect();
    // Five rounds; the 390 entries are the most MogFS v1 has inodes for beside the other fixtures.
    assert_eq!(names, commands.repeat(5));
    assert_eq!(lines.iter().filter(|l| *l == "hi").count(), 5);
    assert_no_leak(&lines, "bench-shell");
    assert!(status.success(), "QEMU exited with {status}");
}

/// Calls the fuzzer makes per seed, sized to the test-host time budget under TCG.
const FUZZ_CALLS: u64 = 20000;

#[test]
fn fuzzer_never_crashes_the_kernel_or_leaks_frames() {
    for seed in [1, 2, 3] {
        // Each seed boots on a fresh image, so `test=fuzz fuzz=<seed>,<calls>` reproduces it exactly.
        let image = mogfs_image(&format!("fuzz-{seed}"), 1024);
        let (status, lines) =
            boot_with_disk(&image, &format!("test=fuzz fuzz={seed},{FUZZ_CALLS}"));
        std::fs::remove_file(&image).unwrap();
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("panic:") || l.starts_with("fault:")),
            "seed {seed}: the kernel panicked or the fuzzer faulted"
        );
        let ok = format!("fuzz: seed {seed}: {FUZZ_CALLS} calls ok");
        assert!(
            lines.iter().any(|l| l.starts_with(&ok)),
            "seed {seed}: missing line: {ok}"
        );
        assert_no_leak(&lines, "fuzz");
        assert!(status.success(), "QEMU exited with {status}");
    }
}

/// QEMU's user networking (guest 10.0.2.15, host 10.0.2.2) with a virtio-net device, then `extra`.
fn boot_with_nic(extra: &[&str]) -> (ExitStatus, Vec<String>) {
    let nic = [
        "-netdev",
        "user,id=n0",
        "-device",
        "virtio-net-device,netdev=n0",
    ];
    boot(&[extra, &nic[..]].concat())
}

/// A UDP echo on the host's loopback, which QEMU's user networking shows the guest as 10.0.2.2; returns its port.
fn udp_echo() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut buf = [0; 2048];
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            let _ = socket.send_to(&buf[..n], from);
        }
    });
    port
}

#[test]
fn virtio_net_pings_the_gateway_and_echoes_udp_through_the_host() {
    let port = udp_echo();
    let args = format!("test=net net=10.0.2.15/24,gw=10.0.2.2 udp={port}");
    // The disk comes first, so it takes the highest transport and the probe must look past it for the NIC.
    let image = disk_image("net", 16);
    let drive = format!("file={},if=none,format=raw,id=d0", image.display());
    let disk = ["-drive", &drive, "-device", "virtio-blk-device,drive=d0"];
    let (status, lines) = boot_with_nic(&[&disk[..], &["-append", &args]].concat());
    std::fs::remove_file(&image).unwrap();
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for expected in [
        "disk: 16 blocks".to_string(),
        "ping: reply from 10.0.2.2".to_string(),
        format!("udp: echo mog from 10.0.2.2:{port}"),
    ] {
        assert!(lines.contains(&expected), "missing line: {expected}");
    }
    let counters = lines
        .iter()
        .find_map(|l| l.strip_prefix("net: rx "))
        .expect("missing counters line");
    let (rx, tx) = counters.split_once(" tx ").unwrap();
    let tx = tx.split(' ').next().unwrap();
    assert!(rx.parse::<u64>().unwrap() >= 2 && tx.parse::<u64>().unwrap() >= 2);
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn a_net_bootarg_without_a_nic_boots_as_before() {
    let (status, lines) = boot(&["-append", "net=10.0.2.15/24,gw=10.0.2.2"]);
    assert!(lines.iter().any(|l| l == "net: no nic"), "missing line");
    assert!(status.success(), "QEMU exited with {status}");
}
