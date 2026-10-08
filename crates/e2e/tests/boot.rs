use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, Once};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

/// A boot is killed (and fails) once QEMU prints nothing for this long, or at the cap: a slow scenario under load
/// still passes while it makes progress, and a hang fails as fast as the old 30 s deadline. Silent phases (a timed
/// benchmark loop, httpd serving) reached 15.6 s under 12 `yes` with the suite in parallel (several were killed at 10).
const SILENCE: Duration = Duration::from_secs(30);
const CAP: Duration = Duration::from_secs(300);

/// Builds the kernel, boots it in QEMU with `extra` arguments (`-cpu cortex-a72` unless they name a `-cpu`), and
/// returns the exit status and serial lines.
fn boot(extra: &[&str]) -> (ExitStatus, Vec<String>) {
    boot_with_input(extra, None)
}

/// As `boot`, with a cap of `secs` instead of `CAP`, and a silence limit of a tenth of it if that is longer.
fn boot_for(secs: u64, extra: &[&str]) -> (ExitStatus, Vec<String>) {
    boot_with(Duration::from_secs(secs), extra, None)
}

/// As `boot`; with `input` = (`ready`, `chunks`), writes chunk `i` to QEMU's stdin once the output contains `ready`
/// `i + 1` times.
fn boot_with_input(extra: &[&str], input: Option<(&str, &[&[u8]])>) -> (ExitStatus, Vec<String>) {
    boot_with(CAP, extra, input)
}

fn boot_with(
    cap: Duration,
    extra: &[&str],
    input: Option<(&str, &[&[u8]])>,
) -> (ExitStatus, Vec<String>) {
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

    let cpu = match extra.contains(&"-cpu") {
        true => &[][..],
        false => &["-cpu", "cortex-a72"][..],
    };
    let mut qemu = Command::new("qemu-system-aarch64")
        .args(["-M", "virt,gic-version=3"])
        .args(cpu)
        .args([
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
        .args(match extra.contains(&"-smp") {
            true => &[][..],
            false => &["-smp", "4"],
        })
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
    let start = Instant::now();
    let (mut seen, mut last, mut silence) = (0, start, Duration::ZERO);
    let mut sent = 0;
    let status = loop {
        if let Some(status) = qemu.try_wait().unwrap() {
            break Some(status);
        }
        let now = Instant::now();
        let len = out.lock().unwrap().len();
        if len != seen {
            (seen, last) = (len, now);
        }
        silence = silence.max(now - last);
        // A longer cap (hundreds of TCG cores) allows a longer silence too: QEMU starts every vCPU before any output.
        if now - last > SILENCE.max(cap / 10) || now - start > cap {
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
    println!(
        "QEMU status: {status:?}, longest silence {} ms\nQEMU stderr:\n{err}\nQEMU stdout:\n{out}",
        silence.as_millis()
    );
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
        "spec: v1 mitigated, v2 vulnerable, bhb not mitigated (v2 vulnerable), ssb vulnerable, meltdown not affected, bse vulnerable, table plain on 4/4 cores",
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

/// `test=wx-<name>`: the kernel prints `wx: <address>`, then stores to its own text (`text`), branches to a `.data`
/// word (`exec`) or recurses past core 0's stack into its guard page (`guard`); each ends in the fault report.
#[test]
fn kernel_text_is_read_only_data_never_executes_and_the_boot_stack_has_a_guard() {
    // (scenario, exception class, fault status code: a permission fault on a level-2 block, a translation fault on a
    // level-3 page)
    for (name, class, status) in [
        ("text", 0x25, 0x0e),
        ("exec", 0x21, 0x0e),
        ("guard", 0x25, 0x07),
    ] {
        let (status_code, lines) = boot(&["-append", &format!("test=wx-{name}")]);
        let address = lines
            .iter()
            .find_map(|l| l.strip_prefix("wx: 0x"))
            .map(|a| u64::from_str_radix(a, 16).unwrap())
            .unwrap_or_else(|| panic!("{name}: missing wx line"));
        let fault = lines
            .iter()
            .find(|l| l.starts_with("unhandled sync exception from current EL"))
            .unwrap_or_else(|| panic!("{name}: missing fault report"));
        let field = |key: &str| {
            let hex = fault.split_once(key).unwrap().1.split(' ').next().unwrap();
            u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap()
        };
        let (esr, far) = (field("ESR_EL1="), field("FAR_EL1="));
        assert_eq!(esr >> 26, class, "{name}: exception class: {fault}");
        assert_eq!(esr & 0x3f, status, "{name}: fault status: {fault}");
        match name {
            "guard" => assert_eq!(far & !0xfff, address, "{name}: not the guard page: {fault}"),
            _ => assert_eq!(far, address, "{name}: fault address: {fault}"),
        }
        assert!(
            status_code.success(),
            "{name}: QEMU exited with {status_code}"
        );
    }
}

#[test]
fn tasks_alternate_on_yield() {
    let (status, lines) = boot(&["-smp", "1", "-append", "test=yield"]);
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
    let (status, lines) = boot(&["-smp", "1", "-append", "test=preempt"]);
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
    let (status, lines) = boot(&["-smp", "1", "-append", "test=bench-syscall"]);
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

/// Lines from QEMU 9.2.1's models (`target/arm/tcg/cpu64.c`): cortex-a72 is r0p3 without CSV2 or firmware, cortex-a76
/// has CSV2, CSV3 and SSBS but no SB, max has CSV2_3, CSV3, SSBS2 and SB.
#[test]
fn spec_line_counts_all_sixty_four_cores() {
    let (status, lines) = boot(&["-smp", "64"]);
    let expected = "spec: v1 mitigated, v2 vulnerable, bhb not mitigated (v2 vulnerable), ssb vulnerable, meltdown not affected, bse vulnerable, table plain on 64/64 cores";
    assert!(
        lines.iter().any(|l| l == expected),
        "missing line: {expected}"
    );
    assert!(status.success(), "QEMU exited with {status}");
}

/// The boot table panics on an exception from EL0, so no core enters the kernel from EL0 before it chose its table.
#[test]
fn el0_before_the_vector_table_is_chosen_panics() {
    let (status, lines) = boot(&["-smp", "1", "-append", "test=el0-before-spec"]);
    assert!(
        lines
            .iter()
            .any(|l| l == "exception from EL0 before this core chose its vector table"),
        "the boot table let EL0 in"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("syscall: ")),
        "the program ran to the end"
    );
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn spec_line_matches_the_cpu() {
    for (cpu, spec) in [
        (
            "cortex-a72",
            "v2 vulnerable, bhb not mitigated (v2 vulnerable), ssb vulnerable, meltdown not affected, bse vulnerable, table plain",
        ),
        (
            "cortex-a76",
            "v2 not affected, bhb mitigated, ssb mitigated, meltdown not affected, bse not affected, table loop24-dsb",
        ),
        (
            "max",
            "v2 not affected, bhb not affected, ssb mitigated, meltdown not affected, bse not affected, table plain",
        ),
    ] {
        let args = [
            "-cpu",
            cpu,
            "-smp",
            "4",
            "-append",
            "test=bench-syscall test=pipe",
        ];
        let (status, lines) = boot(&args);
        let expected = format!("spec: v1 mitigated, {spec} on 4/4 cores");
        assert!(lines.contains(&expected), "{cpu}: missing line: {expected}");
        assert!(
            lines.iter().any(|l| l.starts_with("syscall: ")),
            "{cpu}: missing syscall line"
        );
        assert_no_leak(&lines, "pipe");
        assert!(status.success(), "{cpu}: QEMU exited with {status}");
    }
}

#[test]
fn map_stops_at_the_end_of_user_memory_with_budget_left() {
    let (status, lines) = boot(&["-append", "test=map-end"]);
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("panic:") || l.starts_with("fault:")),
        "kernel panicked or the fixture faulted"
    );
    let n: Vec<_> = lines.iter().filter(|l| l.starts_with("N: ")).collect();
    // From two pages below the end: one page, then three would pass it, then the last page.
    assert_eq!(
        n,
        ["N: one page", "N: past the end: ENOMEM", "N: the last page"]
    );
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

/// Asserts that the lines starting with each of `prefixes` are, in order, the `expected` lines with that prefix: each
/// process's own order, whichever cores they ran on.
fn assert_each_in_order(lines: &[String], prefixes: &[&str], expected: &[&str]) {
    for prefix in prefixes {
        let got: Vec<_> = lines.iter().filter(|l| l.starts_with(prefix)).collect();
        let want: Vec<_> = expected.iter().filter(|l| l.starts_with(prefix)).collect();
        assert_eq!(got, want, "lines starting with {prefix:?}");
    }
}

#[test]
fn spawn_moves_handles_and_budget_to_the_child() {
    let (status, lines) = boot(&["-append", "test=spawn"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    assert_each_in_order(
        &lines,
        &["S: ", "C: "],
        &[
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
        ],
    );
    // The one-frame-short spawn fails after mapping everything but the kernel stack, so this checks its rollback.
    assert_no_leak(&lines, "spawn");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn parent_blocks_on_an_empty_pipe_until_the_child_writes() {
    // An ordering scenario: on several cores the writer may run before the reader blocks.
    let (status, lines) = boot(&["-smp", "1", "-append", "test=pipe"]);
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
    // A exits (waking the parent blocked on a pipe whose only write end A held) before B is spawned, so B would
    // take A's slot if exit freed it.
    assert_each_in_order(
        &lines,
        &["P: ", "C: "],
        &[
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
        ],
    );
    assert_no_leak(&lines, "wait");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn priority_inheritance_lets_the_mutex_owner_outrun_a_middle_priority_spinner() {
    let (status, lines) = boot(&["-smp", "1", "-append", "test=pi"]);
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
fn a_mutex_waiter_on_another_core_gets_it_at_unlock_and_every_frame_returns() {
    let (status, lines) = boot(&["-append", "test=pi"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    // On four cores Mid spins beside the others, so only each process's own order is fixed.
    for expected in [
        "L: locked",
        "H: locking",
        "H: acquired",
        "L: relocked",
        "P: high exited",
        "P: mid killed",
        "P: low exited",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "missing line: {expected}"
        );
    }
    assert_no_leak(&lines, "pi");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn pipe_bench_reports_round_trip() {
    let (status, lines) = boot(&["-smp", "1", "-append", "test=bench-pipe"]);
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
    lock_bench(4, 300);
}

#[test]
#[ignore = "the ticket lock convoys with 64 TCG vCPUs on 12 host cores: until step 32's queued lock"]
fn every_one_of_sixty_four_cores_adds_under_the_lock_and_the_count_is_exact() {
    lock_bench(64, 1800);
}

/// `test=bench-lock` on `cpus` cores within `secs`.
fn lock_bench(cpus: usize, secs: u64) {
    let (status, lines) = boot_for(
        secs,
        &["-smp", &cpus.to_string(), "-append", "test=bench-lock"],
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for lock in ["ticket", "test-and-set", "cpu", "per-cpu", "contended"] {
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
    // One adder per core, the boot context one of them, each adding 10^5.
    assert!(first > 100_000, "the adders never interleaved");
    let count = format!("lock: count {}", cpus * 100_000);
    assert!(lines.contains(&count), "missing line: {count}");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn smp_bench_reports_throughput_and_contention_for_each_worker_count() {
    let (status, lines) = boot(&["-append", "test=bench-smp"]);
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("panic:") || l.starts_with("fault:")),
        "kernel panicked or a worker faulted"
    );
    for mode in ["syscall", "pipe", "spawn"] {
        for k in [1, 2, 4] {
            let line = format!("bench-smp {mode} {k}: ");
            let rest = lines
                .iter()
                .find_map(|l| l.strip_prefix(&line))
                .unwrap_or_else(|| panic!("missing line: {line}"));
            let (rate, contended) = rest.split_once(" ops/s, ").unwrap();
            assert!(rate.parse::<u64>().unwrap() > 0);
            contended
                .strip_suffix(" contended")
                .unwrap()
                .parse::<u32>()
                .unwrap();
        }
    }
    assert_no_leak(&lines, "bench-smp");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn ipi_bench_reports_an_sgi_round_trip_between_cores() {
    let (status, lines) = boot(&["-append", "test=bench-ipi"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    lines
        .iter()
        .find_map(|l| l.strip_prefix("ipi: ")?.strip_suffix(" ns/round-trip"))
        .expect("missing ipi line")
        .parse::<u64>()
        .unwrap();
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn console_reads_edited_lines_typed_ahead() {
    console_echo(&[]);
}

#[test]
fn every_core_comes_online_runs_a_task_and_takes_a_timer_tick() {
    every_core_runs(4, 300);
}

/// TCG; about 7 s alone on a loaded host.
#[test]
fn sixty_four_cores_come_online_run_tasks_and_take_a_timer_tick() {
    every_core_runs(64, 300);
}

#[test]
#[ignore = "TCG at 128 cores: run with --ignored --test-threads=1"]
fn a_hundred_and_twenty_eight_cores_come_online_run_tasks_and_take_a_timer_tick() {
    every_core_runs(128, 600);
}

#[test]
#[ignore = "TCG at 512 cores: run with --ignored --test-threads=1"]
fn five_hundred_and_twelve_cores_come_online_run_tasks_and_take_a_timer_tick() {
    every_core_runs(512, 1800);
}

/// `test=smp` on `cpus` cores within `secs`.
fn every_core_runs(cpus: usize, secs: u64) {
    let (status, lines) = boot_for(secs, &["-smp", &cpus.to_string(), "-append", "test=smp"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let online = format!("smp: {cpus} cpus online in ");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(&online) && l.ends_with(" us")),
        "missing line: {online}<us> us"
    );
    // `threads`' victim spins on one core while its other thread blocks; the kill ends both from another core.
    assert!(
        lines
            .iter()
            .any(|l| l == "T: killed a process with a spinning and a blocked thread"),
        "the victim was not killed"
    );
    assert_no_leak(&lines, "smp");
    // Each spinner (one per core, at most 32) waits until all have started, so they ran at once, on distinct cores.
    let mut spun: Vec<usize> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("smp: spinner on cpu ")?.parse().ok())
        .collect();
    let count = spun.len();
    spun.sort();
    spun.dedup();
    assert_eq!((count, spun.len()), (cpus.min(32), cpus.min(32)));
    assert!(spun.iter().all(|&c| c < cpus));
    let ticked = format!("smp: {cpus} cpus ticked");
    assert!(lines.contains(&ticked), "missing line: {ticked}");
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
    // The benchmarks stay on one core.
    let smp = match test {
        "test=bench-disk" | "test=bench-fs" => "1",
        _ => "4",
    };
    boot(&[
        "-smp",
        smp,
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
    let drive = format!("file={},if=none,format=raw,id=d0", image.display());
    // A benchmark scenario: one core.
    let (status, got) = shell_on(&["-smp", "1", "-drive", &drive], &[script, "exit"]);
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

#[test]
fn threads_share_a_counter_keep_their_tls_and_end_with_their_process() {
    let (status, lines) = boot(&["-append", "test=threads"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    let t: Vec<_> = lines.iter().filter(|l| l.starts_with("T: ")).collect();
    // Each `ldxr`/`stxr` addition counts once, and each joined thread's exit code is the TPIDR_EL0 it started with.
    // The boot waits for every task, so a thread left running or blocked after a kill, or a process outliving its last
    // thread, would hang it.
    assert_eq!(
        t,
        [
            "T: joined 1 2 3 4",
            "T: count 400000",
            "T: killed a process with a spinning and a blocked thread",
            "T: a killed thread joins with KILLED",
            "T: a thread stays a zombie until its last handle closes",
            "T: the main thread exits first",
            "T: the last thread ends the process",
        ]
    );
    assert_no_leak(&lines, "threads");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn a_killed_process_refunds_its_zombie_child_to_itself_not_to_its_killer() {
    // One core: the killer ends the blocked last thread in place, in its own trap.
    let (status, lines) = boot(&["-smp", "1", "-append", "test=refund"]);
    assert!(
        lines.iter().any(|l| l == "R: the killer gained 0 pages"),
        "missing line: R: the killer gained 0 pages"
    );
    assert_no_leak(&lines, "refund");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn thread_bench_reports_round_trips() {
    let (status, lines) = boot(&["-append", "test=bench-threads"]);
    assert!(
        !lines.iter().any(|l| l.starts_with("panic:")),
        "kernel panicked"
    );
    for op in ["thread", "thread pipe"] {
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
    assert_no_leak(&lines, "bench-threads");
    assert!(status.success(), "QEMU exited with {status}");
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

/// A server on the host's loopback that answers every connection with an HTTP/1.0 page of `body`; returns its port.
fn host_page(body: &'static str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            // The whole head: replying and closing with request bytes unread would reset the connection, and on
            // several cores the guest's request may arrive in more than one segment.
            let (mut head, mut buf) = (Vec::new(), [0; 1024]);
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            let head = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = stream.write_all((head + body).as_bytes());
        }
    });
    port
}

/// Sends `request` to the host's `port` and reads the response to its end; `None` if the connection fails or closes
/// at once (nothing listening behind QEMU's forward yet).
fn exchange(port: u16, request: &str) -> Option<String> {
    // No read timeout: a slow answer is waited for (a retry would be a request the server counts twice), and QEMU
    // ending closes the connection.
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    (!response.is_empty()).then_some(response)
}

#[test]
fn httpd_echoes_more_sequential_requests_than_its_tables_hold_and_fetch_gets_a_host_page() {
    // More than the 16 TCP slots and 8 TIME_WAIT entries, so entries are reused.
    const REQUESTS: usize = 24;
    // Bad heads, each refused with its status, after which the server still answers: no body, so nothing is left
    // unread and the close is clean.
    const BAD: [(&str, &str); 5] = [
        ("Content-Length: 99999999999999999999", "413"),
        ("Content-Length: 18446744073709551615", "413"),
        ("Content-Length: -5", "400"),
        ("Content-Length: 5x", "400"),
        ("Content-Length: 0\r\nContent-Length: 0", "400"),
    ];
    let page = host_page("hello from the host\n");
    let forward = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // Exactly the requests httpd counts, in order: one good one, the bad ones, the rest good. Each is retried only while
    // nothing listens behind the forward (QEMU closes it at once), and the client stops only when QEMU has ended, so
    // the server's count and the client's always agree and the boot's own deadline is the only one.
    let good = |i: usize| {
        let body = format!("hello {i}");
        let request = format!(
            "POST /anything HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{request}",
            request.len()
        );
        (request, expected)
    };
    let bad = BAD.map(|(header, status)| {
        let request = format!("POST / HTTP/1.1\r\n{header}\r\n\r\n");
        (request, format!("HTTP/1.1 {status} "))
    });
    let exchanges: Vec<_> = [good(0)]
        .into_iter()
        .chain(bad)
        .chain((1..REQUESTS).map(good))
        .collect();
    let ended = Arc::new(AtomicBool::new(false));
    let client = thread::spawn({
        let (requests, ended) = (exchanges.clone(), ended.clone());
        move || {
            let mut responses = Vec::new();
            for (request, _) in requests {
                loop {
                    if ended.load(Relaxed) {
                        return responses;
                    }
                    if let Some(response) = exchange(forward, &request) {
                        responses.push(response);
                        break;
                    }
                    sleep(Duration::from_millis(100));
                }
            }
            responses
        }
    });
    let netdev = format!("user,id=n0,hostfwd=tcp:127.0.0.1:{forward}-10.0.2.15:80");
    let args = format!(
        "test=httpd net=10.0.2.15/24,gw=10.0.2.2 httpd={} fetch=10.0.2.2:{page}",
        REQUESTS + BAD.len()
    );
    let (status, lines) = boot(&[
        "-netdev",
        &netdev,
        "-device",
        "virtio-net-device,netdev=n0",
        "-append",
        &args,
    ]);
    ended.store(true, Relaxed);
    let responses = client.join().unwrap();
    assert_eq!(responses.len(), exchanges.len(), "requests answered");
    for (response, (request, expected)) in responses.iter().zip(&exchanges) {
        assert!(response.starts_with(expected), "{request:?}: {response:?}");
    }
    assert!(
        lines.iter().any(|l| l == "hello from the host"),
        "fetch did not print the host's page"
    );
    // QEMU's user network connects to the guest from the host's address, 10.0.2.2, each time from a new port.
    let peers: Vec<_> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("httpd: 10.0.2.2:")?.parse::<u16>().ok())
        .collect();
    assert_eq!(peers.len(), exchanges.len(), "accept's peer addresses");
    assert!(peers.iter().all(|&port| port != 0));
    assert_no_leak(&lines, "httpd");
    assert!(status.success(), "QEMU exited with {status}");
}

#[test]
fn sockets_echo_over_loopback_wait_for_any_and_need_the_net_handle_and_budget() {
    let (status, lines) = boot(&["-append", "test=sockets"]);
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("panic:") || l.starts_with("fault:")),
        "kernel panicked or a process faulted"
    );
    for expected in [
        // Two C processes on musl's BSD sockets.
        "tcpecho: served 5 bytes to 127.0.0.1, port set",
        "tcpecho: hello",
        // A C child inherits no network from its parent.
        "tcpecho: child socket: EBADF",
        // One process serves 8 connections at once through `io_wait`; another drives 8 clients the same way.
        "nettest: served 8",
        "nettest: 8 echoes",
        // A child spawned without the NetStack handle, then one with a listen-only duplicate.
        "nettest: no handle: EBADF",
        "nettest: listen-only connect: EACCES",
        // A connection accepted through a read-only listener handle cannot be written.
        "nettest: read-only accept send: EACCES",
        // Socket buffers are charged to the budget.
        "nettest: ENOBUFS after 3 sockets",
        // A socket's charge follows it to the process that holds it.
        "nettest: a moved socket is charged to its new holder",
        "nettest: moving a socket refunds its old holder",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "missing line: {expected}"
        );
    }
    assert_no_leak(&lines, "sockets");
    assert!(status.success(), "QEMU exited with {status}");
}
