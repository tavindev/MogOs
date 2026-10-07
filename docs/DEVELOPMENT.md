# Development

## Toolchain

- Pinned to Rust `1.99.0` in `rust-toolchain.toml` with target `aarch64-unknown-none-softfloat` and components `clippy`, `rustfmt`.
- Pinned on purpose: the host's `stable` rustup toolchain is corrupted. Never modify `~/.rustup`'s stable toolchain or global rustup config.
- Do not add `llvm-tools` or `rust-src` components (they conflicted). Stable only; no nightly flags.
- Links with the bundled `rust-lld`; no system linker needed. QEMU (`qemu-system-aarch64`) is the runner.

### C (musl, busybox)

Nothing is installed system-wide; the first build on a machine needs the network. The board's `build.rs` runs
`make -j3 -C c` (`c/Makefile`, ownership in [c/CLAUDE.md](../c/CLAUDE.md)). One cache serves every worktree: the
main checkout's `target/c-cache/` (found through the repository's common git directory), with the downloads in
`dl/` and each build in `<key>/`, the key a hash of every file in `c/` the build reads and `rustc --version`. A
worktree with the same `c/` reuses the build (about 0.4 s to check); changing any of those files builds a new key
(about 75 s on a loaded machine) and leaves the old one. `lockf` on `target/c-cache/.lock` runs one C build at a time
across worktrees. `target/c` in each worktree links to its key's directory. `rm -rf target/c-cache` in the main
checkout clears it (`cargo clean` there does too). Inside the key's directory (`$OUT`) the Makefile:

1. downloads `musl-1.2.5.tar.gz` and `busybox-1.36.1.tar.bz2` into `dl/` and checks their SHA-256 (in the
   Makefile);
2. links `$OUT/host/ld.lld` to the pinned toolchain's `rust-lld` and `$OUT/host/sed` to Homebrew `gsed`;
3. unpacks musl into `$OUT/musl`, deletes its FP/SIMD assembly, copies `c/musl/` over it, and runs
   `./configure --target=aarch64-linux-musl --disable-shared` and `make install` into `$OUT/sysroot` with
   `/opt/homebrew/opt/llvm/bin/clang` (Homebrew LLVM 19) and `llvm-ar`;
4. unpacks busybox into `$OUT/busybox` with `c/busybox.config`, `make oldconfig` and `make busybox_unstripped`
   with `CC=c/mog-cc` (host tools with the system `cc`, GNU `sed` first in `PATH`), and strips it into
   `$OUT/bin/busybox`;
5. builds `c/hello.c`, `c/cbench.c`, `c/oscb.c` and `c/oscnop.c` with `c/mog-cc` into `$OUT/bin`.

`c/mog-cc` is clang with `--target=aarch64-linux-musl -march=armv8-a+nofp -mabi=aapcs-soft -mno-outline-atomics`,
the sysroot's headers, and for a link `rust-lld` (through `-fuse-ld=lld --ld-path`, with
`DYLD_FALLBACK_LIBRARY_PATH` at the toolchain's `lib`, where its `libLLVM.dylib` lives), `c/link.ld`, `crt1.o`,
`libc.a` and the toolchain's `aarch64-unknown-none-softfloat` `libcompiler_builtins-*.rlib` for the soft-float
libcalls. Needs: Homebrew `llvm` (clang 19 or later for `-mabi=aapcs-soft`), `gsed`, `curl`, `shasum`, GNU make
3.81 (Xcode's). A missing tool fails the kernel build with the `make` output.

## Settings and why

| Setting | Where | Reason |
| --- | --- | --- |
| target `aarch64-unknown-none-softfloat` | `rust-toolchain.toml`, `.cargo/config.toml` | No FP/SIMD in the kernel, so traps and context switches never save v-registers (and boot needs no `CPACR_EL1` FP enable). |
| `jobs = 6` | `.cargo/config.toml` | Half of the 12 host cores so builds never take the whole CPU; rustc codegen threads share this jobserver. |
| `link-arg=--threads=6` | `.cargo/config.toml` | `rust-lld` ignores cargo's jobserver and would otherwise use every core. |
| `-T linker.ld` | `crates/board/qemu-virt/build.rs` | Kernel memory layout (load address, BSS, stack); binary only. |
| nested `cargo build` of `crates/user` | `crates/board/qemu-virt/build.rs` | The boot archive must exist before the kernel compiles (`include_bytes!`); stable cargo has no artifact dependencies, and QEMU `-initrd` would need DTB parsing, frame reservation and a runner change. So the board's build script builds the user programs (release, stripped, own `target/user` dir so it does not wait on the outer build's lock, wrappers removed so `cargo clippy` does not invalidate them) and writes a newc cpio into `OUT_DIR`; `cargo build`/`run` stay one command. |
| `crates/user` excluded from the workspace | root `Cargo.toml`, `[workspace]` in `crates/user/Cargo.toml` | Its bins would make `cargo run` ambiguous and get built for the host by `test-host`. Lint and format it with `--manifest-path crates/user/Cargo.toml` (commands below). Its own `[workspace]` table keeps a worktree nested under the main checkout from attaching it to the outer workspace. |
| `-T link.ld`, `-zmax-page-size=4096` | `crates/user/build.rs` | User programs: one RX `PT_LOAD` (text, rodata) and one RW (data, bss), page-aligned from 4 GiB; lld's 64 KiB default page size padded each program to 64 KiB. |
| load address `0x4020_0000` | `crates/board/qemu-virt/linker.ld` | QEMU only places its 1 MiB DTB at RAM base (`0x4000_0000`) if it fits below the ELF image. |
| `-M virt,gic-version=3` | runners in `.cargo/config.toml`, `crates/e2e`, `scripts/bench.sh`, `scripts/oscompare.sh` | GICv3 only (GICv2, QEMU `virt`'s default, takes at most 8 cores): affinity routing, a redistributor per core, SGIs by one system-register write. |
| `-global virtio-mmio.force-legacy=false` | runners in `.cargo/config.toml`, `crates/e2e` | QEMU 9.2 defaults virtio-mmio to legacy (version 1); the driver speaks modern (version 2), which takes three 64-bit queue addresses instead of legacy's page-size register and one page-aligned ring block. |
| `-global virtio-mmio.ioeventfd=off` | same | QEMU handles a queue notify in the vCPU thread instead of handing it to the main loop: hvf 4 KiB requests +9% write+flush, +19% read (21 interleaved boots); 256 KiB unchanged. |
| `test-host` alias | `.cargo/config.toml` | Runs tests for the host target, excluding the bare-metal-only `qemu-virt` and `arch`. A string, not an array, so a nested worktree's copy overrides it instead of concatenating. A cargo alias cannot chain a second command, so `crates/user`'s host tests run from `crates/e2e/tests/user.rs`, a nested `cargo test` like the boot tests' nested build. |
| `bench-host` alias | `.cargo/config.toml` | Runs the host `benches/*.rs` targets (`--bench '*'`) for the same crates as `test-host`. |
| `mkfs`, `shell`, `httpd` aliases | `.cargo/config.toml` | `cargo mkfs` writes an empty 64 MiB MogFS `disk.img` in the current directory; `cargo shell` boots into msh with it attached (`-drive`/`-device` after the runner's `-kernel <path>`); `cargo httpd` boots into the HTTP echo server with a virtio-net NIC on QEMU's user network, the host's 127.0.0.1:8080 forwarded to the guest's port 80 (`test=httpd` alone implies `net=10.0.2.15/24,gw=10.0.2.2`, since a string alias cannot quote a two-word `-append`). Strings, so a worktree's copy overrides them. |
| `make -C c` in the board's `build.rs` | `crates/board/qemu-virt/build.rs`, `c/Makefile` | busybox and the C programs join the boot archive in the same one-command build; the build is cached across worktrees by a hash of `c/` (about 0.4 s to check when it is built). |
| soft-float C (`-march=armv8-a+nofp -mabi=aapcs-soft`) | `c/Makefile` | The kernel never enables FP/SIMD at EL0 (no `CPACR_EL1` FPEN), so traps and switches never save v-registers; C code follows the Rust programs. |
| busybox `CONFIG_EXTRA_CFLAGS="-DBB_GLOBAL_CONST="` | `c/busybox.config` | clang 19 hoists the read of busybox's `const` globals pointer above its assignment (hush faulted at `0x140`); busybox's documented switch. |
| `linked_list_allocator` (no features) | `crates/board/qemu-virt` | Kernel heap with `free` (phase 2 task stacks need it); a bare `Heap` behind an `arch::Lock` (which masks IRQs), not its own spinlock, which could deadlock on one core. In the board crate because it is the binary that owns `#[global_allocator]` and `unsafe` heap init. |
| `criterion` (no default features), `cpu-time` | `[dev-dependencies]` of `mm`, `mogfs` | Host benchmarks: criterion gives each row a confidence interval and the change against a saved baseline, which `scripts/bench.sh host` turns into an interleaved A/B; `cpu-time` reads the thread's CPU time for `benches/thread_time.rs`, since wall time on this loaded host counts other processes' time. Without default features: no plotters, no rayon. Dev-only, so never in the kernel build; `cargo test-host` compiles them for these crates' tests. |
| `panic = "abort"` | both profiles | No unwinding in a kernel. |
| dev `opt-level = 1` | root `Cargo.toml` | Opt-level 0 kernel code has bloated stack frames and slow MMIO loops; measured build cost is zero. Trade-off: some locals show as optimized out in the debugger. |
| release `lto = true`, `codegen-units = 1` | root `Cargo.toml` | Smallest/fastest release image; release only, so the inner loop does not pay for it. |
| `unsafe_code = "forbid"` | `[workspace.lints.rust]` | Every crate is safe Rust by default; the compiler rejects `unsafe` outside `arch` and board crates. |
| `unsafe_op_in_unsafe_fn = "deny"` | `qemu-virt`, `arch`, `user` `[lints.rust]` | Each unsafe op inside an `unsafe fn` needs its own `unsafe {}` block and justification. |
| `clippy::undocumented_unsafe_blocks = "deny"` | `qemu-virt`, `arch`, `user` `[lints.clippy]` | Enforces the `// SAFETY:` comment rule mechanically. |
| `clippy::multiple_unsafe_ops_per_block = "warn"` | `qemu-virt`, `arch`, `user` `[lints.clippy]` | Keeps unsafe blocks small so each `SAFETY` comment covers one operation. |

Evaluated and not applied (all within noise on this crate): `debug = "line-tables-only"`, dev `codegen-units`, toggling `incremental`. No `rustfmt.toml`: defaults already pass.

Re-measure (`time cargo build`, median) before changing any of the above.

## Inner loop

```sh
cargo check            # type-check only, fastest
cargo clippy           # lints; must be clean
cargo fmt              # format (CI-style check: cargo fmt --check)
cargo clippy --manifest-path crates/user/Cargo.toml --target-dir target/user  # user programs (outside the workspace)
cargo fmt --manifest-path crates/user/Cargo.toml
cargo test --manifest-path crates/user/Cargo.toml --target aarch64-apple-darwin --target-dir target/user --lib  # msh's command table
cargo build            # dev build
cargo test-host        # host tests (crates/user's too) + QEMU boot e2e tests (crates/e2e); must pass
for i in $(seq 50); do cargo test -q --target aarch64-apple-darwin -p e2e --test boot -- handles_enforce_rights_and_generations --exact || break; done  # flake hunt: one test N times; drop `-- <name> --exact` to loop the whole suite (its parallel boots add load)
cargo bench-host       # host benchmarks (criterion for mm and mogfs, min/median for net and mogfs2); see docs/BENCHMARKS.md
scripts/bench.sh host 11 main mm  # host criterion A/B, working tree against main, interleaved; see docs/BENCHMARKS.md
cargo run              # boot in QEMU; prints hello, exceptions, mmu, ram, frames, heap, boot, disk lines and powers off
cargo run -- -append test=mmu-fault  # reads an unmapped address after MMU on; prints the data abort
cargo run -- -append test=wx-text    # stores to kernel text (wx-exec: branches to a .data word; wx-guard: core 0's stack overflows into its guard page); prints the fault
cargo run -- -append test=yield      # tasks a and b print 0..2 in turn via `svc` yield
cargo run -- -append test=bench      # prints the yield round trip in ns
cargo run -- -append test=preempt    # timer preempts spinning task a; task b prints 0..2
cargo run -- -append test=user       # EL0 process A writes A: 0..9 to its console handle; B reads A's address, then C (B's process index) kernel RAM: both killed (fault: 2 ec=0x24 far=...)
cargo run -- -append test=bench-syscall  # EL0 loop of no-op syscalls, prints the round trip in ns
cargo run -- -append test=handles    # EL0 process writes via its console handle, then a no-write duplicate, a closed and a stale handle fail (H: lines)
cargo run -- -append test=map-end    # asm fixture whose map cursor starts two pages below USER_END: one page maps, three are ENOMEM, the last page maps (N: lines)
cargo run -- -append test=budget     # EL0 process maps pages until ENOMEM (M: lines), exits; free frames before/after its lifetime match
cargo run -- -append test=spawn      # spawner (from the boot archive) checks failing spawns (budget, argument limits) move nothing, spawns child with only the console and 32 arguments in 4096 bytes (S: and C: lines); free frames before/after match
cargo run -- -append test=pipe       # reader blocks on an empty pipe until its child writer writes, reads EOF, waits for exit code 7, respawns into the reused slot; a stale process handle is EBADF; 8 KiB I/O moves 4 KiB (R: and W: lines); free frames before/after match
cargo run -- -append test=wait       # waiter's child A exits before child B is spawned; wait still returns both codes and budgets; closing a third, exited child's handle returns its budget too (P: and C: lines); free frames before/after match
cargo run -- -append test=pi         # timer on: L (priority 1) holds a mutex H (3) blocks on while Mid (2) is ready to spin forever; H acquires only through priority inheritance, then init kills Mid (L:, H:, P: lines; no M: line); free frames before/after match
cargo run -- -append test=echo       # readlines prints E: ready, reads two lines typed on the console (echoed, backspace erases), prints got: <line> for each
cargo run -- -append test=bench-spawn # spawnbench spawns nop, waits and closes it 1000 times without and then with two arguments; prints each round trip in ns
cargo run -- -append test=bench-pipe # ping and pong echo one byte over two pipes 100000 times; prints the round trip in ns
cargo run -- -append test=threads    # timer on: four threads add to a shared counter (T: count 400000) and are joined with their TLS as exit codes; a process with a spinning and a blocked thread is killed; free frames before/after match
cargo run -- -append test=bench-threads # threadbench: thread create + join + close 1000 times, then one byte to a thread of the same process and back over two pipes 100000 times; prints each round trip in ns
cargo run -- -smp 12 -append test=bench-smp  # 1, 2, 4, 8, 12 smpwork processes at once (up to the cores) for 0-byte writes, pipe round trips with their own pong, spawn + wait of nop; prints ops/s and KERNEL's contended acquisitions per run
cargo run -- -append test=bench-ipi  # core 0 sends core 1 an SGI that it answers with one, 1000 times; prints the round trip in ns
cargo run -- -append test=bench-lock # uncontended acquire + release of the ticket and a test-and-set lock in ns; two timer-preempted tasks add 10^7 each under the lock (lock: count 20000000)
cargo run -- -append test=smp  # cores 1-3 start (PSCI CPU_ON, as a tree), smp: 4 cpus online in <us> us; threads' victim is killed while one thread spins on another core; with the timer off four kernel tasks each print a distinct core (smp: spinner on cpu <n>); once every core took a timer tick, smp: 4 cpus ticked
cargo run -p mogfs --example mkfs --target aarch64-apple-darwin -- b.img 1024; cargo run -- -drive file=b.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=bench-syscalls  # fresh image; prints `bench <call>: <ns> ns` for every syscall's fast path
cargo run -p mogfs --example mkfs --target aarch64-apple-darwin -- b.img 1024; cargo run -- -drive file=b.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=bench-shell  # fresh image; shellsetup makes fixtures, msh times 11 commands 5 times (`bench <command>: <ns> ns`)
scripts/bench.sh bench-syscalls 21 new_mog_os base_mog_os  # hvf A/B, interleaved; see docs/BENCHMARKS.md
cargo run -p mogfs --example mkfs --target aarch64-apple-darwin -- fuzz.img 1024; cargo run -- -drive file=fuzz.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append "test=fuzz fuzz=7,1000000"  # syscall fuzzer on a fresh image: seed 7, a million calls ("Testing strategy")
cargo run -- -drive file=disk.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=disk  # attach a raw image (`truncate -s 1M disk.img`); every boot prints `disk: <n> blocks` (`disk: none` without a disk); the first writes blocks 1-2 and flushes (disk: wrote), the next reads them back (disk: read ok); a failed flush prints disk: flush failed
cargo run -- -drive file=disk.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=bench-disk  # image of at least 8 MiB; sequential write+flush and read throughput in MiB/s, 4 KiB and 256 KiB per request
cargo run -p mogfs --example mkfs --target aarch64-apple-darwin -- disk.img 16384  # empty 64 MiB MogFS image
[ -f disk.img ] || cargo mkfs; cargo shell  # formats disk.img if missing, boots into msh with it; files survive a reboot once synced (see "Using the shell")
cargo run -- -drive file=disk.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=bench-fs  # MogFS image; open(CREATE|TRUNC)+write+sync and open+close round trips in ns
[ -f disk.img ] || cargo mkfs; cargo shell, then: sh -c cbench  # musl syscall round trip and busybox spawn in ns
cargo httpd, then from the Mac: curl -v http://localhost:8080/anything -d hello  # the echo server answers 200 OK, text/plain, the request as received (see "The HTTP echo server")
cargo run -- -netdev user,id=n0 -device virtio-net-device,netdev=n0 -append "test=httpd fetch=10.0.2.2:8000/,100"  # fetch GETs a host page 100 times (bench http-get), then the server runs
scripts/oscompare.sh [runs]  # same C benchmarks (c/oscb.c) on MogOs, Linux and macOS, interleaved; docs/BENCHMARKS.md "Cross-OS comparison"
cargo run -- -netdev user,id=n0 -device virtio-net-device,netdev=n0 -append "test=net net=10.0.2.15/24,gw=10.0.2.2 udp=7777"  # needs a UDP echo on the host's 127.0.0.1:7777; pings 10.0.2.2 (ping: reply from ...), echoes mog over UDP (udp: echo ...), prints the frame counters; a net= bootarg without a NIC prints net: no nic
QEMU_ARGS="-netdev user,id=n0 -device virtio-net-device,netdev=n0" scripts/bench.sh "bench-net net=10.0.2.15/24,gw=10.0.2.2 udp=7777" 21 <mog_os>  # UDP round trip, burst send and 16-in-flight stream to the host echo (docs/BENCHMARKS.md)
cargo run -- -append test=sockets   # loopback only: C tcpecho server and client on musl's BSD sockets, nettest serving 8 connections at once through io_wait, a child without the NetStack handle (EBADF), a listen-only one (EACCES), one whose budget holds 3 sockets (ENOBUFS); free frames before/after match
cargo run -- -append test=bench-sockets  # loopback TCP: 64-byte round trip, connect + close, 4 KiB stream sends (bench lines)
cargo run -- -s -S     # boot halted, gdbstub on localhost:1234; attach lldb/gdb
cargo build --release  # LTO release image
```

Quit a hung QEMU with `Ctrl-A` then `X`.

## Testing strategy

- End to end first: a behavior is proven by a QEMU boot scenario in `crates/e2e/tests/boot.rs` that asserts exact
  serial lines. Host tests (a crate's public API, `cargo test-host`) cover the edge cases a boot reaches only slowly or
  not at all: corrupt and crafted images, power cuts, table limits, argument checks.
- Fuzzing: `test=fuzz` boots `crates/user/src/bin/fuzz.rs`, which makes seeded random syscalls (every number, unknown
  ones too) with boundary and random arguments and handles, and fails on any result that is not a count or a known
  errno. `fuzzer_never_crashes_the_kernel_or_leaks_frames` runs seeds 1-3, 20000 calls each, each on a fresh
  1024-block image, and asserts no `panic:`, no `fault:`, the `fuzz: seed <s>: <n> calls ok` line and no leak.
  Longer runs by hand: the `fuzz=<seed>,<calls>[,<from>]` bootarg (inner loop above); `<from>` prints every call from
  that one on with its result, before making it, so a crash's last `fuzz: call` line is the culprit. The same seed and
  calls on a fresh image of the same size replay the same calls. A kernel bug the fuzzer finds is fixed with a
  failing scenario first, never skipped in the fuzzer.
- Leak checks: every scenario that frees frames prints `<test>: free frames <n> before, <n> after` around all it
  spawned, and its e2e test asserts the two match (`assert_no_leak`).
- Speed: per-call benchmarks run base and new kernels interleaved; any per-call slowdown fails (`docs/BENCHMARKS.md`).
  Host benchmarks use criterion, in-guest ones the kernel's timer; host A/B is `scripts/bench.sh host`.

## The HTTP echo server

`cargo httpd` boots MogOs with a NIC on QEMU's user network and runs `httpd` (`crates/user/src/bin/httpd.rs`) on port
80, which QEMU forwards from the host's `127.0.0.1:8080`. For each request it answers `HTTP/1.1 200 OK` with
`Content-Type: text/plain` and the request it received (request line, headers, body) as the body, then closes the
connection; one connection at a time, forever. So `curl -v http://localhost:8080/anything -d hello` shows its own
request back, and the console logs `httpd: <peer ip>:<port>` for each connection (10.0.2.2 through `hostfwd`). A head
over 8 KiB is `431`, a malformed or repeated `Content-Length` `400`, one over 1 GiB `413` (`body_length` in
`crates/user/src/lib.rs`, host-tested and fuzzed). The server holds only the console and a listen-only NetStack. Bootargs: `httpd=<n>` stops after `n`
requests (the power-off follows); `fetch=<ip>:<port>[/<path>][,<times>]` first runs `fetch`, which prints that page's
body (or, with `<times>`, GETs it that many times and prints `bench http-get: <ns> ns`). Quit with `Ctrl-A` then `X`.

## Using the shell

`[ -f disk.img ] || cargo mkfs; cargo shell` boots into `msh> ` on a persistent `disk.img`. Paths are relative to the
current directory and stay below the root: `..` (except `cd ..`), `.` and a leading `/` are `EINVAL`.

| Command | Does |
| --- | --- |
| `cd [dir]`, `pwd` | builtins: change the current directory (`cd ..` up one, `cd` alone to the root), print it |
| `help`, `exit` | builtins: list the builtins and the commands; leave (the boot powers off) |
| `ls [dir]` | list a directory (`name/` for a directory) |
| `mkdir <dir>`, `touch <file>` | make a directory; make an empty file if missing |
| `write <file> <text>` | replace a file with `text` and a newline |
| `cat <file>` | print a file |
| `rm <path>`, `mv <from> <to>` | remove a file or empty directory; move or rename (never over an existing name) |
| `echo <text>` | print `text` |
| `sync` | make every change durable; nothing written since the last `sync` survives a reboot |
| `sh [-c '<script>']` | busybox sh (hush) on musl, with `ls`, `cat`, `echo`, `mkdir`, `rm`, `mv`, `true`, `sync` as applets; without `-c` it reads commands until `exit` |

Every command but the builtins is a program from the boot archive (`crates/user/src/bin`, busybox for `sh`), never
from disk, listed in msh's table (`COMMANDS` in `crates/user/src/lib.rs`); msh passes it only the handles its job
needs. `sh` gets the console (read and write) as stdin, stdout and stderr, the root and the boot archive (a shell
reads, changes files anywhere and runs programs), and starts in msh's current directory. A word in single quotes
keeps its spaces (`sh -c 'mkdir d; ls'`). A failure prints `msh: <command>: <errno>`, an unknown command
`msh: <name>: command not found`.

## Rules for agents

- Run `cargo fmt` before finishing.
- `cargo clippy` must be clean (no warnings, no errors), and so must `crates/user` (command above).
- `cargo test-host` must pass, including the e2e boot test; extend `crates/e2e/tests/boot.rs` when boot output changes.
- `cargo run` must still boot and print the hello line.
- Do not raise the `jobs` or linker `--threads` caps.
- New crates use `[lints] workspace = true`. Never opt a crate out of `unsafe_code = "forbid"` unless it is an arch/board crate; put `unsafe` behind a safe API there.
- Hot-path changes report before/after benchmark numbers; any regression fails unless no safe faster form exists (`docs/BENCHMARKS.md`).
- Do not add dependencies without a stated reason.
- Keep linker and profile settings unless a measurement (before/after `time cargo build`) justifies a change; record it in this file.
- Do not touch the toolchain pin, `~/.rustup`, or global rustup config.
- Every change gets a `reviewer` pass (correctness and simplicity) and its findings fixed before it is reported done (`docs/WORKFLOW.md`).
