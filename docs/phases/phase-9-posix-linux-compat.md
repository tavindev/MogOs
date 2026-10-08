# Phase 9: POSIX completeness and Linux binary compatibility

Goal (milestone): unmodified Linux aarch64 programs run on MogOs: Alpine's static busybox, then Alpine's dynamic musl
`redis-server`, then a Debian glibc userland, then an Alpine image as a container. The native ABI is frozen at the end
of the phase against a corpus of prebuilt binaries. The Linux layer translates Linux syscalls into native ones in user
space and never shapes kernel internals (AGENTS.md): the kernel gains only generic mechanisms native programs use too
(an exception handler and notify word, user FP/SIMD, `protect` and `seal`, a readiness op, a local IPC socket, a
redirect range), never a Linux structure, flag
or number.

## Steps

Each done-when names its e2e test (a QEMU boot test in `crates/e2e/tests/boot.rs`; host tests only for tricky pure
logic), the benchmarks it holds and adds, and its invariants (Step details). Benchmarks are hvf medians of 21
interleaved boots against the step's base commit (`docs/BENCHMARKS.md`): yield (`test=bench`), syscall
(`test=bench-syscall`), pipe (`test=bench-pipe`) and boot, at `-smp 1` and `-smp 4`, plus the cross-OS rows
(`scripts/oscompare.sh`) a step names. "Hold" means within noise; a slowdown is a failure, removed or shown
unavoidable with numbers. Programs from outside (Alpine, Debian) are fetched by `c/Makefile` at pinned SHA-256, like
musl and busybox, and bundled or written to the test disk.

Needs from earlier phases: threads, futexes and musl pthreads (phase 5), CoW `fork`, file mmap, memory objects and
the higher-half kernel with mapping at a chosen address (phase 6: a non-PIE Linux binary loads at 0x400000, which
is the kernel's device identity map in TTBR0 until the kernel moves to TTBR1, and `map` picks addresses today), async file ops and MogFS v2 (phase 7 steps 42, 39b), sockets (phase 8 step 50). 53, 53b and 54b start as soon
as phase 5 and phase 8 are in; 53a follows 53; the kernel half of 55 lands as a spike right after 53 and its numbers
decide the compat design (Decided); 54, 54a and 54c wait for phase 6; the rest of 55 needs 53, 53b and 54; 56 follows
55; 56a follows phase 7 step 42; 57 follows 56; 59 follows phase 7 step 40; 59a needs 55 and 59; 58, the freeze,
lands last.

| # | Step | Done when |
| --- | --- | --- |
| 53 | Exception handler, notify word, libc signals | Kernel: `set_handler(entry)` per process and `alt_stack(stack)` per thread: a user fault (data or instruction abort, undefined instruction, alignment, `brk`) pushes the trap frame below the faulting thread's SP and returns to `entry` with x0 = frame, x1 = ESR, x2 = FAR, x4 = the thread's FP-live bit (53b). The alternate stack is used only when SP is not already inside it (Linux's `on_sig_stack` rule); a push that faults kills, as today. Two threads fault into the handler at once, each on its own stack. `frame_return(frame)` reloads a frame (EL0 PSTATE only, checked). `notify(thread, bits)` (new `notify` right; on a process handle it targets the main thread) ORs `bits` into the thread's 64-bit notify word: a blocked wait returns `EINTR`, a running thread is kicked by the reschedule SGI, and the thread enters its handler with the word (read and cleared) in x3 at its next return to EL0 through the switch or IRQ path. Process exit becomes a completion op (`OP_WAIT` on a process handle through `io_submit`/`io_wait`), so `wait4(-1)` is wait-any. libc (signals live in `mogos.c`, which builds Linux's signal frame, `ucontext` with `fpsimd_context`, so step 55's runtime inherits them; its entry stub saves V0-V31, FPCR and FPSR when the FP-live bit is set): `sigaction`, `sigprocmask`, `kill`, `raise`, `alarm` and `setitimer` (a timer thread), `SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGTRAP` from the handler, `SIGCHLD` from `OP_WAIT`, `SIGPIPE` on `EPIPE`, `sigsuspend`. e2e `test=signals` (C): a `SIGSEGV` handler catches a null store and `siglongjmp`s out; a handler that uses FP returns to a loop whose FP registers are intact; `kill(child, SIGUSR1)` runs the child's handler; `alarm(1)` interrupts a blocked pipe read with `EINTR`; `wait(-1)` over three children returns them in exit order. |
| 53a | Ptys, job control, the line discipline out of the kernel | Console input becomes a raw byte-stream handle and `kernel::console::Line` is deleted; a user-space `tty` service (`crates/user`) owns input and echo and runs the line discipline (canonical mode, `^C`, `^Z`, `^\`). Console output stays the direct handle every program writes, so output never takes a hop through the service. A pty is a pipe pair the service hands out with a control channel; termios calls in libc are messages to it. New `suspend(process)` / `resume(process)` (a `stop` right): the stop takes the kill path's mark and SGI, so a running process stops at its next switch. The shell gives the service `notify` and `stop` rights on its foreground job. e2e: `^C` ends a foreground `cat`, `^Z` stops it and `fg` resumes it, `isatty` and `tcgetattr` answer through the service; the `test=shell` transcript is unchanged. |
| 53b | User FP/SIMD, hard-float C, Rust `std` | A thread starts with EL0 FP/SIMD trapped (`CPACR_EL1.FPEN`); its first FP instruction traps and makes it FP-live for life, with zeroed registers. Its 528-byte state (V0-V31, FPCR, FPSR) is reserved beside its kernel stack and charged with it, so the trap never allocates. Switching away from an FP-live thread saves its registers; switching in restores them unless both sides agree the core still holds them (this core's last owner is that thread, and that thread's last core is this one: Linux's `fpsimd_last_state` and `fpsimd_cpu`). A thread that is not FP-live never runs with another process's values readable: they are zeroed at switch-in when it follows another process's FP-live thread. Restore and zeroing happen at switch-in, never on the return to EL0. The kernel stays FP-free, with one named exception: step 65's SHA2 scope in `arch`, which saves the interrupted thread's state through this machinery. musl, busybox and the C programs go hard-float (hard cutover: `-march=armv8-a`, musl's FP assembly and `setjmp`'s d8-d15 restored, `mog-cc` and `c/CLAUDE.md` updated); `crates/user` stays soft-float. A Rust `std` program built on stable for `aarch64-unknown-linux-musl` and linked against MogOs's musl runs (the ROADMAP open decision's route). e2e: two hard-float processes yield to each other while computing a double-precision checksum and both match the host's value; a fresh thread's first FP read is 0, never another process's value; the Rust `std` program spawns threads, writes a file and makes a TCP connection. |
| 54 | `protect`, `seal`, dynamic linking | New `protect(addr, len, rights)`: read, write or execute on mapped pages, never write and execute together (`EINVAL`), with a TLB flush on the changed range only. Mappings can be sealed (OpenBSD's `mimmutable` model, moved here from phase 6): the kernel seals the image it maps at `spawn`, and `seal(addr, len)` lets the loader seal libraries and RELRO after relocation; `protect` or unmapping a sealed range is `EPERM`. `spawn` of an ELF with `PT_INTERP` loads the interpreter (musl's `libc.so`, which is its own loader) and hands it the main image as a file handle; the loader maps images and libraries by file mmap (phase 6). e2e: a dynamically linked C program runs; `dlopen` of a test `.so` calls into it; a store to a page mapped `RX` faults and a `protect` asking for write and execute together is `EINVAL`; `protect` of the sealed main image is `EPERM`. |
| 54a | Readiness op and POSIX gaps by suite | A generic `OP_READY` completion op (readable, writable, hang-up) on any stream handle (pipe, socket, console), its rights checked at submit; `poll`, `select` and `ppoll` in libc submit one per handle, take results from `io_wait`, and cancel the ops still outstanding with phase 7 step 42's `io_cancel` before returning (`epoll`, step 56, keeps its ops armed instead); `getrandom` from a new `random(ptr, len)` call (ChaCha20 keyed from the DT's `rng-seed`, reseeded by step 77's virtio-rng); `uname`, uid and gid answers stay libc data with no authority. Measured by musl's `libc-test`, built hard-float and run in the e2e harness: the pass count is recorded and every failure is listed with its cause in Step details; a new failure later is a regression. |
| 54b | DHCP and DNS | Deferred here from phase 8. UDP socket handles (`socket(net, kind)`), a native `dhcp` program that configures the stack through the NetStack handle's new `admin` right, and musl's resolver over UDP with `/etc/resolv.conf` written by `dhcp`. e2e: under QEMU user networking the guest gets 10.0.2.15 by DHCP (no `net=` bootarg) and `fetch` resolves a name the e2e test serves from its own DNS stub on the host. |
| 54c | Local IPC sockets | A kernel IPC object for AF_UNIX stream and datagram sockets (phase 12's Wayland needs it): a connected pair of bounded queues charged to their creators, carrying bytes and handles (`SCM_RIGHTS` is handle transfer, with the transfer right checked at send). Naming without a global namespace: `bind` creates a socket node under a directory handle the binder holds, and `connect` resolves it through a directory handle the client holds, like any file. The node is in memory only: a bind commits nothing to MogFS, and the name is gone with its socket or a reboot (a socket outlives neither, so a durable name would only dangle). e2e: two C processes exchange data and a pipe handle over a named socket; a process without the directory handle cannot connect; `socketpair` works across `fork`. |
| 55 | Syscall redirect and compat runtime: static Linux binaries | Kernel (lands first, as a spike right after 53): `redirect(entry, lo, hi)` per thread, Syscall User Dispatch's shape: every `svc` from a PC outside `[lo, hi)`, whatever its immediate, is not dispatched and copies nothing: every GPR and SPSR stay as they are, the return PC goes to `TPIDRRO_EL0`, and the kernel erets to `entry`. The runtime inside `[lo, hi)` makes native calls as usual and returns with `svc #1` (the immediate decoded from `ESR_EL1.ISS`, honored only inside the range), for which the kernel sets ELR from `TPIDRRO_EL0` and keeps every register. `TPIDRRO_EL0` is switched per thread whenever the outgoing or incoming thread is redirected. `SCTLR_EL1.UCT` and `UCI` are set (EL0 reads `CTR_EL0` and does its own cache maintenance, as Linux allows; libgcc's `__clear_cache` and JITs need both). Fast path: the runtime rewrites each trapped site once, when the instruction before it is `mov x8, #nr`, into a `b` (which leaves x30 alone) to a trampoline of its own within `b`'s ±128 MiB; the trampoline's `bl` into the runtime is what clobbers x30, so it first pre-indexes SP by 16 and stores x30 there (it never writes below SP), restores it after the call and branches back to the instruction after the site. The runtime's signal entry treats a PC on a trampoline page (a per-page flag in the runtime's own table of trampoline pages) as inside the runtime, so the signals-only-at-return rule covers trampolines too; so the runtime is effectively a JIT: it writes trampoline pages and patched text RW, flips them RX (never write and execute together) and seals trampoline pages once stable. Another thread executing a page while it is RW takes an instruction abort, which the runtime's fault handler recognizes and retries, never a `SIGSEGV`. Rewriting a private file-mapped text page makes a CoW copy charged to the process's budget, and the runtime never forwards a Linux `mseal` covering text it may rewrite. In steady state a call the runtime answers enters the kernel 0 times and a forwarded call once; other sites stay on the redirect. The compat runtime is `mogos.c`'s translation layer built freestanding and soft-float (so it never touches the Linux program's FP state) with an ELF loader: it maps the Linux binary at its own addresses, builds the Linux stack and auxv, enables the redirect and jumps. An unmapped Linux number returns `ENOSYS` and is counted (`compat: unmapped <nr> x<count>` at exit). e2e `test=linux-static`: Alpine's `busybox.static` runs `ls`, `cat` and a shell pipeline (`sh -c` piping `echo` into `cat`); a multi-threaded static binary keeps four threads making syscalls while their sites are rewritten, and none faults. Gate: the same static Linux binary's `write0`, `pipe` and `open+close` rows on MogOs must not lose to Linux's (`write0` already forwards; `getppid` is forwarded too, never cached); the spike is measured against this gate before the runtime is built on it (Decided). |
| 56 | Readiness shims, typed introspection, generated `/proc` | `epoll`, `eventfd`, `timerfd`, `signalfd` in the translation layer (so in libc and the runtime alike) over `io_wait` completions. Two typed calls with size-prefixed versioned structs: `process_info(process, ptr, len)` and `system_info(ptr, len)`; the runtime generates `/proc/self/{exe,maps,fd,status,cmdline}`, `/proc/{cpuinfo,meminfo,stat}` and `/sys/devices/system/cpu/online` from them. e2e: Alpine's `redis-server` (dynamic musl, from step 54 on) answers `PING`, `SET` and `GET` from the e2e test through `hostfwd`, and `BGSAVE` (a `fork`) writes its dump. |
| 56a | Change notification | Deferred here from phase 7 (survey 42). A watch handle on a directory subtree yields create, write, rename and unlink events as completions; queue overflow is an explicit event, never a silent drop; `changed_since(generation)` lists paths touched since a MogFS commit generation. `inotify` is a shim over it. e2e: each event is seen once in order; an overflow is reported; a C `inotify` test passes. |
| 57 | glibc dynamic binaries | A Debian arm64 rootfs (pinned image) on the test disk runs `bash`, `ls -l`, `sort`, `tar` and `apt-cache policy` (read-only). The runtime supplies `rseq` and `clone3` as `ENOSYS` (glibc falls back), `set_robust_list`, `prlimit64`, `statx`, `getrandom`, and a vDSO image (`AT_SYSINFO_EHDR`) whose `clock_gettime` reads `CNTVCT_EL0`, all in user space. Syscall coverage of the run is recorded. Cross-OS: the glibc binaries' `getppid`, `write0`, `pipe`, `open+close` and `spawn` rows on MogOs against the same binaries on Linux. |
| 58 | ABI freeze | Lands last. Every native call is audited: non-hot calls take size-prefixed versioned structs, every flags field rejects unknown bits (`EINVAL`), every call has a documented error set, every blocking operation is a completion op, every right is checked at submit, including those this phase adds (`notify`, `stop`, `admin`) and those phase 10 adds as it lands (`debug`, `exec_signed`, revokers). The numbering and structs are published in `docs/ABI.md`. A corpus of prebuilt binaries (native Rust, musl C, Linux static, glibc) is pinned and run by the e2e on every build. Frozen means no incompatible change; additions get new numbers (phase 10 adds handles). |
| 59 | Writable clones | Deferred here from phase 7. `crates/mogfs`: a writable clone of a snapshot, CoW against it, deleted like a snapshot. Host: writes to a clone never reach its snapshot or a sibling clone; the power-cut and 200-seed tests pass with clones; deleting a clone frees only blocks no other root reaches. |
| 59a | Containers | An OCI runtime shim (`ocirun`, native) reads `config.json`'s process (args, env, cwd), root and `linux.resources` (CPU quota and memory limit) and spawns the entrypoint through step 55's compat runtime in a resource group (phase 5 step 30) with a writable clone of the image's snapshot as its root directory handle; no NetStack unless the config asks, then one with `connect` only. No namespaces: the runtime fakes pids and uid 0 per container as data. e2e: an Alpine rootfs container runs `sh -c` with a 25% CPU cap that holds within 3%; `wget` fails (no network); files it writes do not appear in the image. |

### Step details

- **53.** Needs phase 5 step 27 (futex, for libc's signal state across threads) and phase 8 step 50 (`io_submit`).
  Benchmark (hvf): signal round trip (`kill` to a handler and back) and fault-to-handler-to-`frame_return`, both against
  Linux on the cross-OS setup. Invariants: no signal state in the kernel beyond the notify word; the syscall exit gains
  nothing (no pending-work test: delivery rides the switch and IRQ return paths that already check the kill mark, the
  debt ledger's Signals row); a chunked call's frame holds advanced x2-x4 between chunks (phase 5 Notes), so delivery
  happens only at a call's final return, or after restoring x2-x4 from the task slot; `frame_return` never loads a PSTATE
  that leaves EL0 or unmasks what EL0 cannot. Avoids: POSIX signal state in the kernel (masks, queues, dispositions
  live in libc; the kernel delivers only faults and the notify word), per-thread versus per-process delivery rules, and
  `signalfd`-style retrofits (waiting is already a completion).
- **53a.** Benchmark: the shell command baselines (`docs/BENCHMARKS.md`) hold; keystroke-to-echo latency recorded.
  Invariants: the kernel parses no terminal bytes; only the UART receive interrupt stays. Avoids: the TTY layer (M10):
  N_TTY line disciplines and `TIOCSTI` input injection.
- **53b.** Benchmark: yield, syscall and pipe (soft-float Rust programs, never FP-live) hold; the cross-OS C rows
  and the shell command baselines are measured before and after the hard-float cutover, their change pre-declared (FP-live C threads now pay a save
  and restore per switch, as Linux's do); new: yield between two FP-live threads, against Linux's `yield` row. Invariants: a thread runs at EL0 with FP enabled only while the
  registers hold its own state; the first-use trap cannot fail; kernel code never touches V registers. Avoids:
  LazyFP-style leaks (CVE-2018-3665; whether any Arm core forwards trapped FP values is unverified, so the zeroing
  stays) and eager save and restore for every thread.
- **54.** Benchmark: `spawn` of a dynamic C program against the static one and against Linux's dynamic `spawn`.
  Invariants: W^X on every user page; the kernel parses only the interpreter's and the main image's headers. Avoids:
  `LD_PRELOAD`/`LD_LIBRARY_PATH` as ambient authority (the loader resolves libraries only below a directory handle it
  was given).
- **54a.** Benchmark: `OP_READY` submit-to-completion on a ready pipe, and `poll` over 1, 64 and 1024 pipes, against
  Linux's `poll` and `epoll_wait`. Invariants: readiness is a completion like any other op, never a second wait API.
  Avoids: POSIX record locks (M15): `fcntl(F_SETLK)` stays `ENOLCK` until a target program needs it, then
  locks are handle-bound (OFD semantics).
- **54b.** Invariants: the address is set only through the `admin` right; the resolver has no kernel part.
- **54c.** Benchmark: AF_UNIX stream round trip (64 bytes) against the pipe's and Linux's. Invariants: a socket name
  is reachable only through a directory handle; a handle in flight is charged to the sender until received. Avoids:
  Linux's abstract socket namespace (a global name space) and `SO_PEERCRED`-style ambient identity.
- **55.** Benchmark (cross-OS): `write0`, `pipe` and `open+close` of the same static
  Linux binary on MogOs and Linux (the gate), for the redirect alone (the spike) and with rewritten sites. Yield and
  pipe between redirected threads include the `TPIDRRO_EL0` switch (one `msr` per side), recorded; between native
  threads it is skipped and they hold. `UCT` and `UCI` are pre-declared zero-cost (control bits set once at boot). A non-redirected thread
  pays one load and branch on a byte beside data the syscall path already loads: pre-declared at 0 ns within noise
  on the syscall benchmark (55 ns minimum, about 66 median after 60b) and measured; if it is not within noise,
  redirected threads get their own vector tables, chosen at switch like 60a's. Invariants: the runtime holds no
  authority the process does not (the redirect is not a sandbox, as Linux's own Syscall User Dispatch documents: the
  Linux program can call native syscalls by jumping into the range, with only its own handles); a rewritten site is
  equivalent to the trap it replaces; images the runtime rewrites are not sealed (54); the runtime copies
  `TPIDRRO_EL0` into its per-thread frame before anything else, and Linux signals are delivered to the program only
  at the runtime's return, never while it runs inside the range. Avoids: WSL1's in-kernel provider and
  starnix's shared kernel region (one runtime per process, so nothing to isolate inside it).
- **56.** Avoids: `/proc` and `/sys` as kernel text ABI (M5, M16): the kernel returns binary structs, the text is
  generated in user space; epoll in the kernel (M4).
- **56a.** Invariants: a watch reaches only below its directory handle; its queue is charged to the owner's budget.
  Avoids: inotify's `max_user_watches` and per-inode watches.
- **57.** Invariants: every syscall glibc makes is either translated or `ENOSYS` and counted, never silently
  succeeded. The coverage list goes in What was done.
- **58.** Avoids: ABI by accident (M2, M16): ignored unknown flags, register-packed calls that cannot grow.
- **59a.** Benchmark: container start time (shim to entrypoint's first instruction), recorded. Invariants: a
  container's authority is exactly the handles the shim passed (D7). Avoids: user namespaces (M7) and namespace-by-
  namespace container construction (M18).

## Decided

- **Linux compatibility runs in user space, in the Linux process: a copy-nothing redirect plus lazy rewriting of
  `svc` sites** (research, 2026-10-07): Linux's Syscall User Dispatch shape (a per-thread selector and an allowed PC
  range, docs.kernel.org/admin-guide/syscall-user-dispatch.html) with Fuchsia starnix's same-thread return to a
  handler instead of a signal (fuchsia.dev, "Making Linux syscalls in Fuchsia" and `zx_restricted_enter`), and the
  lazypoline (DSN 2024) and ASC-Hook (arxiv.org/html/2412.05784v4: 33.5 ns per hooked call on Neoverse N1) hybrid of
  trapping a site once and rewriting it into a direct branch. Starnix switches address spaces because one starnix
  kernel serves many processes; here one runtime serves one process and holds only that process's handles. Two
  kernel entries per call cannot win on our own numbers (a native round trip is 55 ns minimum, about 66 median after
  60b, against Linux's 105.3 ns `getppid`), hence the rewrite: 0 entries for calls the runtime answers, 1 for
  forwarded ones. Rejected unless 55's spike fails the gate: in-kernel translation (FreeBSD's linux(4), WSL1's
  `lxcore.sys`), one trap but Linux state and quirks in the kernel; seccomp-trap style (gVisor's systrap) costs a
  signal per call.
- **One translation layer**: `mogos.c`'s Linux-number dispatcher serves native musl programs linked in, and Linux
  binaries as the compat runtime, so `epoll`, signals and `/proc` exist once.
- **User FP/SIMD with eager save and restore skipped at switch-in when the core still holds the state** (Linux
  arm64's `fpsimd.c`: restore skipped while `fpsimd_last_state` and the task's `fpsimd_cpu` agree), with first-use trapping so threads that never use FP pay
  nothing; the native Rust programs that hold the tracked benchmarks stay soft-float.
- **Rust `std`**: the stable route in ROADMAP's open decision (`aarch64-unknown-linux-musl` linked against MogOs's
  musl) is verified in 53b; a custom `aarch64-unknown-mogos` target stays out (nightly).
- **Sealing** (from phase 6, 2026-10-07): the kernel seals the spawned image and the loader seals what it
  relocated, OpenBSD's `mimmutable(2)` model (phase 6's research; not re-read here), with `protect` in step 54.
- **Dropped, not scheduled** (research, 2026-10-07): the packet filter (the NetStack capability already decides who
  reaches the network), the net-queue handle (revisit only if the stack is measured as the bottleneck), the
  file-system server channel (drivers and file systems stay in the kernel). IPv6 and NDP move to phase 11 step 71a.

## Notes

- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12) against this doc: 53 is split into 53
  (signals) and 53a (ptys); 53b, 54a, 54b, 54c, 56a and 59 are new (FP/SIMD, the POSIX suite, phase 8's DHCP and DNS,
  phase 7's change notification and writable clones); 55-58 keep their numbers; 59 is 59-59a.
- Not scheduled, no step needs them yet: POSIX record locks,
  `io_uring` for Linux binaries (`ENOSYS`; glibc and most runtimes fall back), and `ptrace` for Linux binaries
  (phase 10 step 61's debug handle is its base).
- Linux binaries see a 39-bit user address space (`USER_END` is 1 << 39); Android kernels have shipped 39-bit
  user space, so mainstream binaries do not assume 48 bits (general knowledge, not re-verified).
- Cross-OS rows change meaning in 53b: MogOs's C programs become hard-float like Linux's, so the `docs/BENCHMARKS.md`
  rows are rerun after it.

## What was done

Filled in as each step lands.
