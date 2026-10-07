# What Linux gets right, what it got wrong, and what MogOs does next

Date: 2026-10-07. Scope: decision-ready survey for MogOs (read from AGENTS.md, docs/ROADMAP.md, docs/phases/*.md).

Legend. Links are reachable (HTTP 200 checked) and either search-confirmed or search-associated with the claim; a few LWN pages were rate-limited, so their titles were not re-read. A claim with no link is general knowledge (inferred, not re-verified this session). Sizes: S = days, M = 1-2 weeks, L = a phase's worth of steps, XL = more than a phase. Tiers: P0 table stakes, P1 servers/cloud, P2 desktop/embedded breadth, P3 later.
Secondary-only figures (blogs/aggregators) are marked (secondary).

## Summary (read this first)

Top 15 P0/P1 items, in build order: (1) lock model + per-CPU data, decided before anything concurrent is added; (2) wait-on-address (futex) with PI; (3) threads; (4) SMP with per-CPU run queues; (5) EEVDF-style fair class under the RT levels; (6) resource groups (CPU/task caps on top of existing budgets); (7) demand paging with up-front commit charge; (8) page cache (kernel-owned, global cap) + file mmap via one memory-object type; (9) CoW fork in libc over a budget-charged clone; (10) MogFS v2 (extents, snapshots, reserve, scrub); (11) safe TCP/IP crate + sockets as handles; (12) signals/ptys/exception channel and dynamic linking; (13) Linux binary compat server + ABI freeze; (14) async file ops + cancel, then multi-queue block/NVMe; (15) kernel W^X + KASLR + Spectre baseline.
Top 10 mistakes and status: fork/overcommit/OOM (avoids, fork compat partial); ioctl sprawl (avoids by rule, needs audit); signals (partial); select/poll/API accretion (avoids, needs cancel); procfs sprawl (avoids so far); capabilities(7) (avoids); user namespaces (avoids); fsync/ext4 (avoids, sticky errors + CoW commit); blocking-I/O legacy + io_uring security (partial: `open`/`readdir` still synchronous, ring unbuilt); TTY (partial: line discipline in kernel). Also: ptrace (needs), C memory safety (avoids in logic crates).
Phases: 5 SMP/threads/fair; 6 virtual memory; 7 storage that scales; 8 networking; 9 POSIX + Linux compat + containers; 10 observability/security hardening; 11 real hardware/boot/power; 12 graphics/desktop/virtualization.
Biggest owner decisions: D2 (page cache outside budgets), user-space third-party drivers vs monolithic, native-ABI freeze timing.

## 0. Where MogOs stands (verified in the docs)

- Done: phases 1-3 (single-core, 4 KiB pages, ASID per process, handle table with rights, fixed-capacity object tables, eager-zeroed `map` charged to budgets, `spawn` from a cpio archive, pipes, completion I/O via `io_submit_wait`, mutex with one-level priority inheritance, strict-priority RR with 4 levels, `kill`). Phase 4: steps 18 (console), 20 (virtio-blk), 21 (MogFS crate), 22 (files + msh) are logged in docs/phases/phase-4-io-storage.md; step 23 (musl + busybox) has no "what was done" entry, so it is the last open step. This survey treats "after phase 4" as starting at step 24.
- Verified gaps that shape everything below:
  - No threads (grep of docs/phases: only "thread pointer for libc" and the test harness). No shared-address-space execution, no wait-on-address primitive. musl pthreads and every server-shaped program need both.
  - Single core by design: "Scheduler state needs no lock on one core: every access runs with IRQs masked" (phase-2 notes). `MAX_TASKS` is 8, mutexes 128. Every subsystem added before SMP bakes that assumption in.
  - Eager map (step 13): no demand paging, no page cache, no file mmap, no CoW.
  - MogFS v1 limits: 504 inodes, 14 direct pointers, files up to 57,232 bytes, names up to 51 bytes (phase-4 step 21 entry). Not competitive yet.
  - `fork` returns `ENOSYS` in step 23 plan; signals are libc-synthesized ("`^C` kills the foreground child through its process handle").
  - `unsafe` allowed only in arch/board; kernel crates `forbid(unsafe_code)`.

### Cross-cutting design calls this survey makes (flagged because they bind later phases)

D1. Demand paging and no-overcommit are compatible. The budget is the commit charge; frames can materialize lazily. Step 13's eager zeroing is a simplicity choice, not a requirement of no-overcommit. Lazy materialization is only needed for file mmap, page cache, and sparse/large reservations.
D2. Page cache is kernel-owned and reclaimable, outside per-process budgets, under a global cap. Budgets cover anonymous memory and kernel objects only. Linux charges cache to the first toucher's memcg, which is its most-complained-about memcg behavior (see section 2, "memory cgroups"). Dirty cache is bounded by writeback throttling, not by a victim process.
D3. Locking model is decided before the page cache, network stack, or more object tables exist. Lock types (spinlock, per-CPU cell) must live in `arch`/`board` because `UnsafeCell` access needs `unsafe`; the kernel crates take a safe `Lock<T>` from there. See roadmap phase 5.
D4. Keep an RT class (strict priority + PI, what exists) above a fair class (EEVDF-style) for everything else. Strict priorities plus round-robin alone cannot compete on desktop/server fairness.
D5. `fork` is not in the native ABI. Libc's `fork` = a budget-charged CoW clone of the address space plus handle table (so a fork of a big process can fail cleanly under budget, which Linux cannot do). `spawn` stays the fast path. Needed only for Linux binary compat and unmodified POSIX programs.
D6. Signals: the kernel offers exactly two things: an exception channel (a faulting thread's fault is delivered as a message on a handle its supervisor waits on) and a "notify" bit on handles. Libc builds POSIX signals from those. Synchronous SIGSEGV/SIGFPE for the faulting process itself needs the kernel exception path; pure libc synthesis (current plan) is not enough.
D7. Containers need no namespace machinery in the native ABI: no global names means a "container" is a process tree whose directory/network handles are restricted. Linux-compat processes get a per-process Linux view built in the compat layer. Network namespaces are still needed once a stack exists, but as an object (a "network stack handle"), not a flag on every process.
D8. io_uring trap avoidance: small fixed op set, every op checked against handle rights at submit, no op that needs ambient privilege, no kernel worker thread running with the caller's credentials, no registered-resource tables that outlive the ring's owner, ring memory owned and sized by budget. See section 5 and mistake M8.

# 1. Process and scheduling

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Threads + wait-on-address (futex) | P0 | M | lock model, TLB shootdown |
| SMP + per-CPU run queues | P0 | L | lock model, GIC SGIs, PSCI |
| Fair class (EEVDF-style) | P0 | M | SMP, timer |
| CPU resource control (cgroup-v2 equivalent) | P1 | M | fair class, budgets |
| Real-time (PREEMPT_RT lessons) | P1 | M | SMP, PI mutex (exists) |
| Containers / namespaces equivalent | P1 | M | handle restriction, net stack |
| Signals | P0 | M | exception channel, libc |
| Debug/ptrace equivalent | P1 | M | threads, exception channel |
| vDSO-equivalent | P1 | S | time source, user mapping |
| Pluggable scheduling (sched_ext lesson) | P3 | M | stable sched trait |

### SMP and per-CPU scheduling (P0, L)
1. Several cores each running their own run queue; migrate by push/pull.
2. Every server, and every desktop made after 2006. A single-core kernel is a toy to buyers.
3. Linux: per-CPU runqueues with per-rq locks, scheduling domains mirroring cache/NUMA topology, periodic and idle load balancing, wake-affine heuristics. Pain: the balancer is a tangle of heuristics (the reason EEVDF replaced CFS's tuning knobs, see below), lock ordering across two rqs, and scalability work on 100s of cores consumed a decade. Source: [EEVDF doc](https://docs.kernel.org/scheduler/sched-eevdf.html), [sched_ext doc](https://docs.kernel.org/scheduler/sched-ext.html).
4. Copy per-CPU run queues and idle-pull. Differ: no scheduling-domain tree at first (one level: all cores) with affinity as a handle right; deterministic placement (a thread stays where it was last unless idle cores exist); avoid wake-affine heuristics. Because there are fixed tables and no `Arc`, queues are index arrays; a ticket/MCS lock type lives in `arch` (D3).
5. Depends on: lock type (arch), PSCI CPU_ON bring-up, GICv2 SGIs for IPIs, TLB shootdown, per-CPU storage via TPIDR_EL1. Rough size L (about 7 steps; see phase 5). Do this before the page cache or network stack (D3).

### Fairness: CFS to EEVDF (P0, M)
1. A proportional-share scheduler that gives each runnable thread CPU in proportion to weight with bounded lag, and lets latency-sensitive threads ask for earlier deadlines.
2. Desktop responsiveness and server tail latency; without it a CPU hog starves a shell (strict priorities starve by design).
3. Linux 6.6 replaced CFS's vruntime heuristics with EEVDF (eligibility from lag, pick earliest virtual deadline; slice length gives latency control). Pain: CFS needed years of tunables (min_granularity, wakeup_granularity); EEVDF still carries many hacks (sleeper lag, delayed dequeue). Sources: [kernel doc](https://docs.kernel.org/scheduler/sched-eevdf.html), LWN [An EEVDF CPU scheduler for Linux](https://lwn.net/Articles/925779), [LWN 925371](https://lwn.net/Articles/925371).
4. Copy the algorithm (it is small: weights, lag, deadline; a few hundred lines), skip tunables. Fixed task tables make an augmented tree unnecessary at MAX_TASKS up to a few thousand: a linear scan over a bounded runnable set is acceptable below about 1,000 tasks, then an indexed heap. Priority bands: RT class above fair class (D4).
5. Depends on SMP rq structure; M.

### Real-time and PREEMPT_RT (P1, M)
1. Bounded worst-case latency: preemptible kernel, PI everywhere, threaded IRQs.
2. Embedded, industrial, audio, robotics, telco; also an easy "deterministic" marketing win that MogOs already promises.
3. Linux took 19 years: PREEMPT_RT merged in 6.12 ([LWN 992184](https://lwn.net/Articles/992184)). It required converting spinlocks to sleeping PI mutexes, threaded IRQs, a printk rewrite. Pain: a kernel designed non-preemptible and retrofitted; the huge audit cost is the lesson.
4. MogOs advantage: design it in. Rules: (a) IRQ handlers do bounded work and wake a thread; (b) every kernel wait that can be long uses the existing PI mutex or a bounded-time spinlock with IRQs masked only inside `arch`; (c) syscalls already "do bounded work" (phase 3 ABI rule), so the missing piece is wake preemption (phase 3 notes "a wake does not preempt yet") and an in-kernel measured IRQ-to-thread latency (phase-2 notes say unmeasurable under hvf; needs real HW or an in-kernel GIC). Add PI to wait-on-address (Linux futex has PI variants, see [futex(2)](https://man7.org/linux/man-pages/man2/futex.2.html)).
5. Depends on wake preemption, SMP; M.

### cgroups v2 resource control (P1, M)
1. A hierarchy that groups processes and applies CPU, memory, IO, and PID limits to the group.
2. Cloud and containers (Kubernetes, systemd). Without limits you cannot run multi-tenant.
3. Linux: unified hierarchy with controllers (cpu weight/max, memory.high/max/min, io.max/weight, pids.max), and pressure-stall information that systemd-oomd uses ([cgroup-v2](https://docs.kernel.org/admin-guide/cgroup-v2.html), [PSI](https://docs.kernel.org/accounting/psi.html)). Pain: v1/v2 split lasted a decade; "no internal processes" rule; memcg charges page cache to the first toucher; charging of kernel memory incomplete; the cgroup filesystem interface is a text API with races.
4. MogOs already has hierarchical memory by construction: `spawn` moves budget parent-to-child ("budgets never exceed RAM", phase-3 step 14). Extend the same idea: a "resource group" is a handle; threads created into it inherit; limits are numbers on that object (cpu weight, cpu bandwidth, io weight, max tasks). No text files, no controller negotiation. CPU bandwidth: a token bucket refilled per period, enforced in the fair class. IO: weight in the block queue (later). Page cache: D2 (global).
5. Depends on fair class, SMP; M for CPU, S each for tasks/io.

### Namespaces and containers (P1, M)
1. Per-process views of the global state: mount, PID, net, user, IPC, UTS, cgroup, time.
2. Cloud (OCI runtimes: runc, containerd) and desktop sandboxes (Flatpak). Without OCI compat there is no cloud story.
3. Linux: eight namespace types added one at a time over 2002-2013, composed with cgroups and seccomp to approximate a container; there is no "container" object. User namespaces let unprivileged code gain capabilities inside, which expanded attack surface (see M6). Source: [namespaces(7)](https://man7.org/linux/man-pages/man7/namespaces.7.html), [user_namespaces(7)](https://man7.org/linux/man-pages/man7/user_namespaces.7.html).
4. MogOs capability model makes most of this free (D7): the native ABI has no global names; a container is a process tree given a restricted directory handle, a resource-group handle, and (later) a network-stack handle. For Linux binary compat, the compat layer fakes `/proc`, PID numbers and user ids per container, as pure data in the compat crate, without kernel mechanisms. OCI runtime shim maps config.json to spawn arguments. No user namespaces: uid is a compat-layer illusion with no kernel authority attached.
5. Depends on phase 9 (compat), net stack for net namespace; M. Note this beats Linux in attack surface, not in feature count.

### Signals (P0, M)
1. Asynchronous notifications interrupting a thread at arbitrary points.
2. Every POSIX program (SIGCHLD, SIGINT, SIGPIPE, SIGSEGV handlers; Go and JVM runtimes use SEGV handlers).
3. Linux inherited the 1970s model: handler runs on the interrupted stack, async-signal-safe list, EINTR everywhere, signalfd and pidfd bolted on later, plus real-time queued signals. Pain: handlers re-enter arbitrary code; races between signal delivery and syscalls drove `pselect`, `ppoll`, `signalfd`, `sigtimedwait`; per-thread vs per-process delivery rules; `signal(7)` is 800 lines of caveats. See [signal(7)](https://man7.org/linux/man-pages/man7/signal.7.html), [pidfd_open(2)](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).
4. Native ABI: no signals (already decided). Provide: process handles you wait on (done), an exception channel (D6) for faults and a `notify` to wake a blocked wait. Libc implements `sigaction`/`kill`: a dedicated libc "signal thread" per process, or delivery at syscall boundaries via the wait set; the faulting thread's own SEGV is delivered by the kernel rewinding the thread to a libc trampoline that the libc registered with a `set_exception_handler` call (the only kernel signal-like feature, synchronous only). That keeps async signals out of the kernel entirely.
5. Depends on threads, exception channel, libc; M. Honest cost: some POSIX behaviors (async interruption of a CPU-bound thread by SIGALRM) need a timer thread in libc that stops the target thread via a kernel `suspend` capability; acceptable.

### ptrace and debugging (P1, M)
1. One process inspecting and controlling another (gdb, strace, rr, crash handlers).
2. Developers on every platform; without it nobody can debug; containers/runtime tooling depend on it.
3. Linux: `ptrace(2)` is a single multiplexed syscall with a stateful tracer/tracee protocol layered on signals (stops reported through `waitpid`), a 40-year list of races, and a security-policy tangle (Yama, `ptrace_scope`, per-process dumpable flags). Pain list in [ptrace(2)](https://man7.org/linux/man-pages/man2/ptrace.2.html). It also cannot trace a process from an unrelated PID namespace cleanly, and `PTRACE_SEIZE` fixed only part of it.
4. Copy Fuchsia/Zircon's shape: a "debug handle" to a process or thread with a right (`debug`), operations `read_memory`, `write_memory`, `read_regs`, `suspend`, `resume`, `single_step`, `set_breakpoint`, plus an exception channel (D6). All governed by handle rights, so no `ptrace_scope` policy knobs. gdbstub on top of it in user space.
5. Depends on threads, exception channel, handle rights (exist); M.

### Futex design (P0, M)
1. Userspace atomic word plus a kernel wait queue keyed by address: contended path only enters the kernel.
2. Every threading library, mutex, condvar, semaphore; the backbone of musl pthreads and Go/Java/Rust parking.
3. Linux: [futex(2)](https://man7.org/linux/man-pages/man2/futex.2.html) hashes (mm, address) to a bucket; supports wait, wake, requeue, PI futexes, robust lists, `FUTEX_WAIT_BITSET`, and `futex_waitv` (5.16, from the Wine/Proton need) for waiting on many. Pain: three decades of bugs (the requeue-PI complexity is documented in the kernel docs itself), robust-list cleanup on exit, hash-bucket contention, private vs shared keys, the single `futex()` multiplexer syscall.
4. Copy the idea, not the syscall: `wait(addr, expected, timeout)`, `wake(addr, n)`, `wait_many`, and `lock_pi`/`unlock_pi` on an address (PI exists as a kernel mutex object today; unify). Fixed-size wait table keyed by (address-space id, vaddr) with bounded buckets, no heap. Shared memory keys use physical frame. Robust handling: process exit already releases mutex handles; make the thread-exit path do it for address-based PI locks via an owner list. Completion-style variant: `wait` as an `io_submit` op so a thread can wait on a futex plus other things in one call.
5. Depends on threads, shared mappings; M. This is the first dependency of musl pthreads, so it is the first step after SMP locking.

### vDSO (P1, S)
1. A kernel-provided shared object mapped into every process so `clock_gettime` is a few loads, not a syscall. See [vdso(7)](https://man7.org/linux/man-pages/man7/vdso.7.html).
2. Anything that timestamps (databases, tracing, runtimes); syscall-free time is a standard latency expectation.
3. Linux maps a small ELF image, with a seqlock-protected data page updated by the timekeeping code; arm64 reads `CNTVCT_EL0` directly. Pain: ABI of the data page; each arch/clock mode has its own variant; time namespace needed a special page.
4. On aarch64 the counter is readable from EL0 (`CNTKCTL_EL1`), so much simpler: map one read-only page with frequency and offset; libc reads `CNTVCT_EL0`. No ELF vDSO needed at first. The phase 3 syscall benchmark (28 ns) means the win is mostly for non-Linux-compat code; do it when the Linux compat layer needs `clock_gettime`.
5. Depends on user mapping, time source; S.

### Pluggable scheduler (P3, M)
sched_ext (merged 6.12, [LWN 991205](https://lwn.net/Articles/991205/), [doc](https://docs.kernel.org/scheduler/sched-ext.html)) lets BPF programs be the scheduler. It proved that workload-specific policies (Meta, games) beat a universal one. MogOs should keep the scheduler behind a narrow trait (already static dispatch) and consider a user-space policy server for a non-RT class much later; not before phase 11.

# 2. Memory

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Demand paging (budget as commit) | P0 | M | fault handler, budgets |
| Page cache + writeback | P0 | L | demand paging, FS, lock model |
| mmap of files / shared mappings | P0 | M | page cache |
| Copy-on-write (fork, private file maps) | P0 for compat | M | refcounted frames |
| W^X and kernel ASLR | P0 / P1 | M | higher-half kernel |
| Huge pages | P1 | M | frame allocator contiguity |
| NUMA | P1 (big servers) | M | SMP, DT/ACPI topology |
| Memory cgroups equivalent | P1 | S (exists) | budgets |
| Swap, zswap | P3 | L | page cache, reclaim |
| Memory tagging, mapping sealing | P3 | S | hardware |

### Demand paging (P0, M)
1. Map now, allocate frames on first touch.
2. Servers (large sparse heaps, JVMs reserve GBs), file-heavy programs, fast `exec` (ELF pages mapped lazily).
3. Linux: the fault path walks VMAs, installs a zero page or reads a file page; it relies on overcommit so `mmap` rarely fails. Pain: lazy allocation plus overcommit is what makes the OOM killer necessary (M1). [Overcommit accounting](https://docs.kernel.org/mm/overcommit-accounting.html) documents the heuristic and the strict mode 2, which Linux has but few use because fork-heavy and reserve-heavy applications assume mode 0.
4. D1: keep no-overcommit, add laziness. `map` charges the budget up front (commit charge); frames appear on first touch; the fault cannot fail for lack of memory because the sum of budgets never exceeds RAM (spawn already guarantees this). Zero-on-first-touch speeds up `exec` and sparse arrays. File-backed faults can still hit an I/O or checksum error: deliver it on the exception channel (D6), never as an OOM kill. Keep a `reserve` (address space only) vs `commit` split for GC'd runtimes (Windows MEM_RESERVE/MEM_COMMIT, inferred).
5. Depends on the arch fault handler (exists for faults); M.

### Page cache (P0, L)
1. Kernel cache of file pages shared by `read`, `write` and `mmap`.
2. All users: it is what makes a filesystem fast.
3. Linux: unified page cache, XArray per file, per-device writeback flusher threads, dirty throttling, LRU (MGLRU in 6.1, [doc](https://docs.kernel.org/admin-guide/mm/multigen_lru.html)), readahead heuristics. Pain: accounting (first toucher pays), writeback stalls and `vm.dirty_ratio` folklore, double caching with O_DIRECT databases, `drop_caches` as an admin hack, lock-ordering bugs between truncate, mmap and writeback.
4. D2: kernel-owned, global cap, no per-process charge. Bounded hash of (inode, page index) to frame; MogFS checksums verified on fill so a bad block never enters the cache as good; writeback is a kernel task with a dirty bound derived per device from measured throughput; fixed-window readahead that doubles to a cap on sequential access; direct I/O is a handle flag. Eviction: CLOCK/2Q first, MGLRU only if a benchmark justifies. The cache is the slack: a `spawn` that needs budget can reclaim cache, so no-overcommit holds.
5. Depends on demand paging, MogFS write path, lock model (D3); L. The biggest single jump for file workloads. Today phase-4 step 22 does I/O straight to the block layer.

### mmap of files (P0, M)
1. Map file pages; faults fill from the page cache; `MAP_SHARED` writes become file writes.
2. Databases (LMDB, SQLite), loaders, executables, large read-mostly data.
3. Linux: VMA maple tree (6.1), shared file pages via the page cache, `msync`, `MAP_PRIVATE` is CoW ([mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html)). Pain: I/O errors surface as SIGBUS (no error return path), mmap/truncate races, per-4 KiB fault cost; DBMS authors advise against mmap for transactional safety (Crotty, Leis, Pavlo, "Are You Sure You Want to Use MMAP in Your DBMS?", CIDR 2022; secondary, not re-read this session).
4. One "memory object" handle (Zircon VMO-like, inferred) with anonymous, file-backed and shared variants, so shm, file mmap and `memfd_create` collapse into one mechanism (Linux has three; see [memfd_create(2)](https://man7.org/linux/man-pages/man2/memfd_create.2.html)). Errors go on the exception channel (D6); a populate flag lets a caller fail early. The map/read/write/exec rights already exist (phase 3 step 12).
5. Depends on page cache and a memory-object type; M.

### Copy-on-write and fork (P0 for compat, M)
1. Share frames after a clone; copy on first write.
2. Unmodified POSIX programs (shells, Python `os.fork`, Redis snapshots, Postgres backends); also private file mappings.
3. Linux `fork` marks PTEs read-only with per-page mapcounts. Pain: forking a big process stalls (page-table copy), RSS spikes, fork under overcommit is a reason the OOM killer exists, fork in threaded programs is a hazard. Sources: Baumann et al., [A fork() in the road](https://www.microsoft.com/en-us/research/publication/a-fork-in-the-road) (HotOS 2019); [LWN](https://lwn.net/Articles/785430/).
4. D5: libc `fork` over a kernel `clone_space` that charges the child's budget up front (fork fails cleanly rather than overcommitting), CoW only shares frames. `posix_spawn`/`vfork` map to native `spawn`. Needs per-frame refcounts in the `mm` crate (safe, host-testable).
5. Depends on demand paging, frame refcounts; M.

### Huge pages and THP (P1, M)
1. Map 2 MiB or 1 GiB with one TLB entry.
2. Databases, JVMs, VMs, ML. Gains on TLB-bound loads are workload-specific (benchmark before claiming).
3. Linux: hugetlbfs (explicit, reserved) and THP (transparent, `khugepaged`, [doc](https://docs.kernel.org/admin-guide/mm/transhuge.html)). Pain: compaction stalls and khugepaged latency spikes; several databases tell users to disable THP; fragmentation makes allocation unpredictable.
4. Explicit and deterministic first: a `map` flag asks for 2 MiB blocks from a pool reserved at boot, failing explicitly if exhausted. Transparent promotion only if a benchmark shows wins without tail latency. The kernel already uses 1 GiB identity blocks (phase 3 step 11a).
5. Depends on a contiguity-aware frame allocator; M.

### NUMA (P1 for big servers, M)
1. Memory placement across nodes.
2. Large servers only; irrelevant to desktop, embedded and most cloud VMs.
3. Linux: mempolicy, `numactl`, automatic NUMA balancing ([policy doc](https://docs.kernel.org/admin-guide/mm/numa_memory_policy.html)). Pain: balancing heuristics can hurt; interleave defaults; per-node reclaim.
4. Defer. When needed: per-node frame pools, local-first allocation, an affinity right on a thread group, no automatic migration.
5. Depends on SMP and topology from DT/ACPI; M. Late (hardware phase).

### Swap and zswap (P3, L)
1. Evict anonymous pages to disk or compressed RAM.
2. Desktops under pressure, embedded (zram); less so cloud.
3. Linux: swap, zram, [zswap](https://docs.kernel.org/admin-guide/mm/zswap.html). Pain: swap thrash freezes desktops for minutes; swap and cgroup accounting; unbounded latency.
4. No-overcommit makes swap unnecessary for correctness. If wanted: a per-budget opt-in ("pageable"), compressed RAM first, RT-class tasks never pageable. Pressure metrics (PSI-like, [doc](https://docs.kernel.org/accounting/psi.html)) go to the supervisor, which decides what to stop; the kernel never picks a victim.
5. Depends on page cache, reclaim; L; only if desktop demand shows.

### Memory cgroups (P1, S, mostly exists)
Linux memcg has min/low/high/max with OOM inside the group ([cgroup-v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)). MogOs budgets are hierarchical and hard. Missing: a stats call (current, peak, limit, failures) and a parent-to-child resize (extend spawn's move). S.

### KASLR and W^X (W^X P0, KASLR P1, M)
1. Randomize the kernel base; never writable and executable at once.
2. Everyone, as a baseline; KASLR is weak against local info leaks.
3. Linux arm64: KASLR with `kaslr_seed` from firmware, strict RWX for kernel text/rodata, PAN, PXN, BTI, PAC, MTE ([self-protection](https://docs.kernel.org/security/self-protection.html)); [mseal](https://docs.kernel.org/userspace-api/mseal.html) (6.10) locks mappings. Pain: info leaks (`/proc/kallsyms`, dmesg) forced a stack of restriction knobs.
4. User pages are already W^X (code RX, stack RW+UXN, step 11a). Add: kernel image split into RX text / R rodata / RW data (the kernel leaves its 1 GiB blocks for finer mappings; combine with the higher-half move already noted for Linux compat), PAN, BTI/PAC when the toolchain allows, KASLR from a DT seed, a `seal` handle right (mseal analogue, free). Safe Rust removes most bugs KASLR guards against; leaks via the small unsafe arch/board surface remain.
5. Depends on higher-half kernel; M for W^X plus higher-half, S for KASLR on top.

# 3. I/O

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Completion I/O with shared rings (io_uring done right) | P1 | L | page cache, threads |
| Readiness multiplexing (epoll compat) | P0 for compat | M | completion I/O |
| Zero-copy (sendfile/splice equivalent) | P1 | M | page cache, net stack |
| Block layer (blk-mq) and multi-queue NVMe | P1 | L | SMP, PCIe |
| Async-everything rule | P0 (design) | S | exists |

### io_uring and completion I/O (P1, L)
1. Two shared-memory rings (submission, completion) for batched, asynchronous syscalls ([io_uring(7)](https://man7.org/linux/man-pages/man7/io_uring.7.html), introduction in [LWN 776703](https://lwn.net/Articles/776703/)).
2. High-IOPS servers (databases, proxies, storage), where syscall and wakeup cost per I/O dominates.
3. Linux: bolted onto a blocking-I/O kernel, so many ops still punt to kernel worker threads (`io-wq`); op set grew to 60+ including network, fs, futex; features SQPOLL, fixed files/buffers, linked ops, multishot. Pain: security. Google found 60% of kCTF/VRP exploit submissions in a year used io_uring (about $1M of bounties), disabled it on ChromeOS and production servers, and Android blocks it via seccomp ([Google blog, June 2023](https://security.googleblog.com/2023/06/learnings-from-kctf-vrps-42-linux.html); [Phoronix summary](https://www.phoronix.com/news/Google-Restricting-IO_uring)). Causes (inferred from the CVE pattern): a huge, fast-moving op set sharing kernel state, async workers running with captured credentials, lifetime bugs in registered resources.
4. MogOs already has the right base: every I/O is submit/complete on handles, and `io_submit_wait` is the blocking special case (phase 3 step 15). The phase-4 note defers a shared ring until a benchmark justifies it; keep that. When added (D8): the ring carries only the same ops as the syscall, with rights checked at submit time against the handle; no kernel worker threads with ambient credentials (completions run in the kernel on the submitting task's budget); ring and registered buffers are charged to the budget and torn down with the owner; op set is fixed small (read, write, accept, connect, send, recv, poll, timeout, wait-address, open, close, sync); a fuzzing harness (syzkaller-style, host-side) from day one. Registered buffers/files are handle-table entries, not a parallel table.
5. Depends on page cache and threads (completion context), net stack; L.

### epoll and readiness (P0 for compat, M)
1. Scalable "which of these N fds is ready"; [epoll(7)](https://man7.org/linux/man-pages/man7/epoll.7.html).
2. Every event-loop server (nginx, Node, Redis, Go netpoller), so Linux compat needs it.
3. Linux history: `select`/`poll` are O(n) per call with fd_set limits (FD_SETSIZE 1024, [select(2)](https://man7.org/linux/man-pages/man2/select.2.html)); epoll made it O(ready) but added edge-trigger footguns, thundering herds (`EPOLLEXCLUSIVE`), no support for regular files, fd-reuse/close races, and nested epoll complexity.
4. Native: readiness is just a completion op ("poll handle for events", multishot optional) so no separate mechanism. The compat layer implements `epoll_*` in user space (libc/compat shim) over native poll ops plus a handle-table-indexed map. Do not put epoll in the kernel.
5. Depends on completion I/O with poll ops; M inside the compat phase.

### Zero-copy: sendfile, splice, MSG_ZEROCOPY (P1, M)
1. Move data file-to-socket or pipe-to-pipe without copying through user space ([sendfile(2)](https://man7.org/linux/man-pages/man2/sendfile.2.html), [splice(2)](https://man7.org/linux/man-pages/man2/splice.2.html)).
2. Static web servers, proxies, storage servers, video.
3. Linux: sendfile for file to socket; splice via an in-kernel pipe buffer; pipes holding page references led to Dirty Pipe (CVE-2022-0847; flag-initialization bug in pipe buffers, general knowledge). Pain: splice's pipe-as-intermediary API is awkward, and partial/short transfers complicate callers.
4. With a page cache and refcounted frames (D1, D5), implement one op: `copy(src_handle, dst_handle, len)` that moves page references where both ends support it, falling back to copy. Pipes in MogOs own a 4 KiB buffer (phase 3 step 15); do not let pipe buffers alias page-cache frames. Do zero-copy network receive via buffer handles posted ahead of time (AF_XDP and `MSG_ZEROCOPY` lessons), later.
5. Depends on page cache, net stack; M.

### Block layer and multi-queue NVMe (P1, L)
1. Request queues from CPUs to devices; blk-mq has per-CPU software queues mapped to hardware queues ([blk-mq doc](https://docs.kernel.org/block/blk-mq.html)).
2. NVMe servers (millions of IOPS) and cloud block devices; also fair sharing between tenants.
3. Linux: blk-mq replaced a single-lock request queue (3.13-5.0), schedulers (none, mq-deadline, bfq, kyber), cgroup io controller, plug/unplug batching. Pain: scheduler zoo, writeback cgroup coupling, legacy SCSI paths, bufferheads in older filesystems.
4. Copy the structure: per-CPU submission queues feeding per-device hardware queues, `none` as default, a single simple weighted-deadline scheduler for rotating/slow devices. Current driver is one 4-entry virtio queue with a single request in flight (phase-4 step 20 notes "several scattered requests in flight" not done): first add multiple outstanding requests, then multi-queue virtio-blk and NVMe. The block API stays an async submit/complete trait (matches the kernel's I/O model). End-to-end checksums are MogFS's job, so no integrity layers like dm-integrity needed.
5. Depends on SMP (per-CPU queues), PCIe and NVMe driver; L.

### The async-everything lessons (P0 design, S)
Linux lessons: (1) POSIX AIO failed (glibc thread emulation), `O_NONBLOCK` does not apply to regular files, and every blocking syscall that cannot be made nonblocking (open, stat, getdents, fsync) forced worker thread pools in user space (libuv, Go runtime). io_uring is the retrofit. (2) Cancellation is hard: design cancel as a first-class op from day one (`io_cancel(token)`), with defined results (cancelled, completed-first). (3) Timeouts are ops, not parameters. MogOs already makes every I/O a completion op; the remaining requirements: all blocking kernel operations on files (open, readdir, sync) must be expressible as completion ops, not only read/write; `open` today is a plain syscall (phase-4 step 22), so a slow disk blocks the whole CPU. Make file ops async once disk latency is real (before the page cache lands).

# 4. Filesystems

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| VFS-like file-object interface and dentry/name cache | P0 | M | page cache |
| Mature on-disk format (extents, journal-free CoW, snapshots) | P0/P1 | L | page cache, block layer |
| Correct fsync semantics | P0 | S | exists mostly |
| User-space filesystem servers (FUSE equivalent) | P1 | M | completion I/O, handles |
| Layered/union mounts (overlayfs equivalent) | P1 | M | name resolution |
| Change notification (inotify/fanotify) | P1 | M | file objects |
| Multiple filesystems (FAT/exFAT, ext4 read, tmpfs) | P0/P2 | M each | FS interface |

### VFS and dentry caching (P0, M)
1. A common object interface (inode, dentry, superblock, file) over all filesystems, with a cache of name-to-inode lookups ([VFS doc](https://docs.kernel.org/filesystems/vfs.html)).
2. Everyone; path lookup is the hottest filesystem path (every `open`, `stat`).
3. Linux: dcache with RCU-walk path lookup (lockless fast path), negative dentries, per-sb LRU; mount namespace and bind mounts. Pain: dcache/inode memory accounting under memcg, negative-dentry blowups, the `d_*` locking rules, `..` and symlink races (TOCTOU) that need `openat2` flags (`RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`) to fix.
4. MogOs got the safest part right already: all lookups resolve from directory handles and cannot escape them (phase-4 step 22), the `openat2` retrofit Linux needed. Keep one file-object trait (static dispatch via an enum, as today: `Dir`, `Node`, `Archive`...), add a bounded name cache keyed by (dir inode, name) with checksums-validated fills, and a per-directory sorted/hashed index in MogFS so large directories don't scan linearly (today entries are a flat array, 56-byte entries).
5. Depends on page cache; M.

### ext4, btrfs, XFS lessons (P0/P1, L)
1. Linux ships a journaling extent FS (ext4, XFS) and a CoW checksummed one (btrfs); each teaches something.
2. Servers pick XFS/ext4 for predictability, btrfs/ZFS for snapshots and integrity.
3. Lessons: ext4: extents and delayed allocation made it fast but exposed the rename-without-fsync zero-length file disaster (see M7; [LWN 322846](https://lwn.net/Articles/322846), [LWN 323169](https://lwn.net/Articles/323169)). XFS: scales with allocation groups (parallelism per AG), online repair is finally arriving. btrfs: CoW and checksums and snapshots are the right primitives, but the multi-device RAID5/6 write hole, ENOSPC behavior near full, and fragmentation under random-write workloads gave it a reputation problem. (These btrfs/XFS statements are general knowledge, not re-verified.)
4. MogFS already has the correct primitives (CoW, per-block checksum, atomic superblock commit, 200 seeds of power-cut host tests). It is a toy in capacity: 504 inodes, 14 direct blocks, max file 57,232 bytes, 51-byte names, whole-FS bitmap per commit. Needed for competing: extents (or a B-tree of extents) with a growable inode table; allocation groups to parallelize across cores; checksums at the extent level with a Merkle root so snapshots are cheap; snapshots and clones (copy-on-write makes them nearly free); online scrub; ENOSPC reserved space (never let commit fail for lack of space for metadata, the btrfs lesson); 64-bit everything; TRIM/discard batching; compression optional. Ordering guarantee: data blocks are written before the superblock that references them (it already is), so the ext4 rename-over-old-file hazard cannot occur: state this as a documented contract ("the committed state is always a consistent snapshot").
5. Depends on page cache, block layer; L (a phase of its own).

### fsync semantics (P0, S)
1. What a successful `fsync` promises, and what an error means. [fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html).
2. Every database and package manager.
3. Linux: fsync errors were cleared after being reported once, so a retry returned success though data was lost; discovered by PostgreSQL in 2018 ([PostgreSQL wiki: Fsync Errors](https://wiki.postgresql.org/wiki/Fsync_Errors), [danluu: fsyncgate](https://danluu.com/fsyncgate/)); 4.13+ improved reporting per fd. Plus `data=ordered` vs writeback behaviors, `O_DIRECT` plus flush, barrier/FUA semantic drift.
4. Already ahead: MogFS step 21 makes any `Io` error sticky ("after `Io` from a change, writes and commits fail until the next `mount`"), and virtio-blk requires and tests flush reaching the host (blkdebug test, step 20 review). Keep both as contracts: sync means "committed generation N is durable or an error that never clears". Add `sync_range` / per-file sync and a group-commit path later for throughput.
5. Depends on nothing; S (mostly done).

### FUSE and user-space filesystems (P1, M)
1. Kernel forwards file operations to a user-space daemon ([FUSE doc](https://docs.kernel.org/filesystems/fuse/fuse.html)).
2. Cloud (S3 mounts, sshfs), desktop (MTP, NTFS-3G), containers (virtiofs).
3. Linux: `/dev/fuse` request protocol; pain: kernel-user round trips are slow (passthrough and io_uring-based FUSE are 2023-2025 fixes), deadlock hazards when the daemon touches its own mount, and privileges (`fusermount`).
4. In MogOs, a filesystem server is a process that serves a directory handle: the kernel translates file ops on that directory object into messages on a channel it owns. Same mechanism as microkernel servers; no `/dev` node, no setuid helper, no deadlock when the daemon holds budget. Priority inheritance applies across the call (server inherits caller priority), which Linux FUSE cannot do. Build once page cache exists. Bonus: this is how to add FAT/exFAT/ext4-read without trusting C parsers in-kernel (a memory-safe in-kernel crate is also possible; the choice is per filesystem).
5. Depends on channel object, page cache; M.

### overlayfs and layered images (P1, M)
1. Union of read-only lower layers with a writable upper, via copy-up ([overlayfs doc](https://docs.kernel.org/filesystems/overlayfs.html)).
2. Containers (OCI image layers).
3. Linux: overlayfs plus whiteouts, redirect and metacopy options; pain: inode-number and hardlink semantics, copy-up latency of large files, rename of directories (`EXDEV`), mount option sprawl.
4. MogFS snapshots/clones make layers native: an image layer is a read-only snapshot subtree, a container's writable layer is a CoW clone, so copy-up needs no union driver. Provide a "union directory" handle only if non-MogFS lowers are needed.
5. Depends on MogFS snapshots; M.

### inotify and fanotify (P1, M)
1. Event streams about file changes ([inotify(7)](https://man7.org/linux/man-pages/man7/inotify.7.html), [fanotify(7)](https://man7.org/linux/man-pages/man7/fanotify.7.html)).
2. Desktops (file managers, IDEs), build systems, antivirus/audit.
3. Linux: inotify is per-inode watches with queue overflow and no recursion, so large trees exhaust `max_user_watches`; fanotify adds mount/filesystem scope and permission events; both are separate APIs with separate quirks.
4. One "watch" handle: a directory handle plus event mask yields a stream of completion events; scope is a subtree of the handle (so it is inherently namespaced); overflow is an explicit event. Because MogFS is CoW, a commit generation number gives a cheap "what changed since generation N" query that replaces most watcher use cases (build tools) without any queue.
5. Depends on file-object layer; M.

# 5. Networking

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| virtio-net driver | P1 | M | virtio-mmio (exists), DMA buffers |
| TCP/IP stack (v4+v6, TCP, UDP, ICMP, ARP/NDP, DHCP) | P1 (P0 for any server) | L-XL | timers, page-sized buffers |
| Sockets as handles + BSD sockets in libc | P1 | M | stack, completion I/O |
| Congestion control (CUBIC, BBR) | P1 | S-M | stack |
| Packet filtering (nftables equivalent) | P1 | M | stack |
| eBPF/XDP-style programmable datapath | P3 | L | verifier, see Observability |
| TLS offload, tunnels, bridges, VLAN | P2/P3 | M each | stack |

### TCP/IP stack and sockets API (P1, XL)
1. The reason people run Linux on servers: a complete, fast, battle-tested stack.
2. Servers and cloud first; desktop and embedded second. Without networking there is no server adoption, full stop.
3. Linux: `sk_buff`-based stack, 30+ years of fixes; netfilter hooks, qdiscs, GRO/GSO/TSO offloads, RSS/RPS/XPS per-CPU scaling, socket API with hundreds of options. Pain: huge attack surface in C (the network stack is a permanent CVE source), `setsockopt` and `ioctl` sprawl, lock contention on shared sockets, TCP behavior accrued through compatibility quirks. Even Linux's own developers note fundamental TCP modification difficulty from middleboxes (general knowledge).
4. Write the stack as a safe `no_std` crate over a `NetDevice` port trait (same pattern as `Disk`), host-tested against recorded traces and fuzzed with packet fuzzers (the strongest win from memory safety: the parser of untrusted bytes cannot corrupt the kernel). The Rust `smoltcp` crate exists as a base or reference (inferred; check license and fixed-memory story, since MogOs bans heap in hot paths and wants budgets). Sockets are handles with rights (`bind`, `connect`, `send`, `recv`) and a socket-factory handle grants network access, so no ambient authority: a process without a net handle cannot reach the network. Options are typed calls, not `setsockopt` strings. Buffers charged to the owner's budget (no sysctl `tcp_mem` global pools that surprise). Per-CPU stack instances with flow steering (RSS) from day one, building on D3.
5. Depends on SMP lock model, timers, virtio-net, completion I/O; XL, split into: driver, L2/ARP/IP/ICMP/UDP, TCP, sockets+libc, DHCP/DNS (libc resolver).

### virtio-net (P1, M)
1. The paravirtual NIC every cloud VM uses ([virtio spec](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)).
2. Cloud, QEMU testing. First network driver to write: no real hardware needed.
3. Linux: virtio-net with mergeable RX buffers, multiqueue, checksum/TSO offload negotiation, XDP support. Pain: feature negotiation matrix.
4. Reuse the existing virtio-mmio transport code (phase-4 step 20, modern v2 negotiated). Start with one queue pair, no offloads, then checksum offload and multiqueue. Pre-posted receive buffers come from a fixed pool (budgeted).
5. Depends on virtio transport (exists); M.

### Congestion control, BBR (P1, S-M)
Linux made congestion control pluggable ([tcp congestion docs](https://docs.kernel.org/networking/index.html)); defaults: CUBIC; BBR (Google, 2016) models bandwidth and RTT rather than loss and helped on lossy long paths, but BBRv1 was unfair to loss-based flows (secondary knowledge, not re-verified here). MogOs: ship CUBIC (or NewReno) first in the safe stack; make the congestion controller a trait with a small interface (on_ack, on_loss, cwnd, pacing_rate) so BBRv2/v3 can be added. Needs fine-grained pacing timers (the stack's timer wheel). S-M.

### netfilter/nftables (P1, M)
Linux: netfilter hooks with iptables then nftables (a bytecode VM in the kernel) plus conntrack; vulnerability rich (nftables use-after-free CVEs every year, general knowledge). MogOs: a firewall is a small verified-by-construction rule engine in the safe stack with a table-driven match (no VM in kernel at first), policy set by a handle to the "net admin" object; conntrack only if NAT is needed (containers). M.

### eBPF and XDP (P3, L)
Linux: eBPF programs verified and JITed run at hooks (XDP at the driver, tc, sockets, tracing); [BPF doc](https://docs.kernel.org/bpf/index.html), [AF_XDP](https://docs.kernel.org/networking/af_xdp.html). The most successful Linux extension mechanism of the last decade (Cilium, Katran, Cloudflare). Pain: verifier complexity, a history of verifier bugs as privilege escalations (a verifier is a trusted-computing-base proof checker written in C), unprivileged BPF now disabled by default in most distros (general knowledge). MogOs: a WASM-like or restricted-Rust-subset safe VM is plausible, but verifier work is large; defer past phase 10. Kernel-bypass alternative: a net-queue handle giving a user process direct ring access to a NIC queue (rights-gated), which covers DPDK-class use without a VM. P3 for BPF, P2 for the net-queue handle.

# 6. Security

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Capability model (done) + audit of ambient leftovers | P0 | S | exists |
| Sandboxing primitive (seccomp/Landlock equivalent) | P1 | S | handles (mostly free) |
| LSM-like policy | P3 | M | not needed for capabilities |
| Spectre/Meltdown mitigations | P0 | M | arch work, per-CPU |
| Signed boot and modules | P1 | M | UEFI, key store |
| Rust as the default (share of CVEs) | done | n/a | policy |

### Capabilities(7) failure and what to copy (P0, S)
1. POSIX capabilities split root into about 40 bits ([capabilities(7)](https://man7.org/linux/man-pages/man7/capabilities.7.html)).
2. Everyone: the privilege model sets the blast radius of any bug.
3. Linux: `CAP_SYS_ADMIN` became the catch-all ("the new root"), `CAP_NET_ADMIN` and others are also huge; capabilities are per-process ambient bits (not tied to objects), with inheritable/permitted/effective/bounding/ambient sets, file capabilities, setuid-root still dominant, and `no_new_privs` added later to make sandboxing work. Authority is not tied to an object, so confused-deputy problems abound.
4. MogOs' handle table with rights is the right replacement (object capabilities); keep the rule that nothing is ambient. Audit points: `spawn` priority is capped at the caller's (already), `kill` needs a right on a process handle (already), but check future additions (clock set, reboot/power-off, raw device access, mount, net admin) are each handles, never bits on a process. Rights should attenuate on `dup` (exists) and be non-amplifiable on `transfer`. Add: revocation (a handle to a "revoker" object or generation bump) for long-lived grants; Linux has none either.
5. Depends on nothing new; S audit per new subsystem.

### seccomp, Landlock, LSMs (P1, S)
1. Syscall filtering ([seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html), [filter doc](https://docs.kernel.org/userspace-api/seccomp_filter.html)); unprivileged filesystem sandboxing ([Landlock](https://docs.kernel.org/userspace-api/landlock.html), design discussion [LWN 715203](https://lwn.net/Articles/715203)); and Linux Security Modules (SELinux, AppArmor, Smack, BPF-LSM; [LSM doc](https://docs.kernel.org/admin-guide/LSM/index.html)).
2. Cloud multi-tenancy, desktop sandboxing (browsers, Flatpak), compliance (SELinux in regulated industries).
3. Linux: seccomp-bpf filters by syscall number and args (TOCTOU on pointer args, cannot inspect strings), SELinux policy complexity (famously turned off by many admins), AppArmor path-based rules, LSM stacking complexity, Landlock as the first unprivileged self-restriction design (landed 5.13). Pain: all of these exist because ambient authority exists; each is a patch on the model.
4. In a capability OS the sandbox is "pass fewer handles" at `spawn` (already the model: "a child using a handle value it was not given gets an error", phase 3 step 14). No seccomp-equivalent needed in the native ABI. For the Linux compat layer, a per-process syscall allowlist is trivial in the dispatcher. No LSM framework: it is the main source of kernel complexity on Linux and an LSM is policy for ambient authority. An audit log of handle grants and denials is useful (compliance), P2.
5. Depends on phase 9 compat; S.

### User namespaces CVE history (lesson, P0 as a rule)
User namespaces let unprivileged users get a fake root with capabilities over namespaced objects, reaching kernel code (netfilter, mount, overlayfs, BPF) that was written assuming only real root can call it. Ubuntu 24.04 restricts it through AppArmor and researchers then published bypasses ([Qualys, three bypasses](https://www.qualys.com/2025/three-bypasses-of-Ubuntu-unprivileged-user-namespace-restrictions.txt); [Ubuntu docs](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007); [LWN 796877](https://lwn.net/Articles/796877/)). Rule for MogOs: never give an unprivileged process authority over a kernel interface that was designed for a trusted caller; the compat layer fakes uid 0 without kernel privilege (D7).

### Spectre/Meltdown mitigations (P0, M)
1. Mitigating speculative-execution leaks ([hw-vuln index](https://docs.kernel.org/admin-guide/hw-vuln/index.html)).
2. Cloud (multi-tenant, so mandatory), browsers, anything running untrusted code.
3. Linux: KPTI (Meltdown), retpolines / IBRS / eIBRS / BHI barriers (Spectre v2), array_index_nospec (v1), SSBD, MDS clears, L1TF flush, `mitigations=` boot flag. Pain: performance costs up to double-digit percent on syscall-heavy loads, per-CPU-model matrix, constant new variants (Retbleed, Downfall, Inception, GhostRace...).
4. aarch64 on a72 is affected by some variants (Spectre v1/v2); Arm features (CSV2, SSBS, BTI, branch predictor invalidation via firmware SMCCC workarounds) cover it. Plan: user/kernel separate tables already (TTBR0 user, kernel identity map is EL1-only), which gives Meltdown-class isolation more cheaply than KPTI because TTBR0/TTBR1 are split once the kernel moves to the higher half. Add SMCCC `ARCH_WORKAROUND_1/2/3` calls on entry where needed, bounds-clamp (index masking) in the handle-table lookups, and a per-thread-group "untrusted" flag that scrubs predictors on switch. Measure with the syscall benchmark (28 ns now): regression rule applies. Single-address-space capability checks are less speculation-prone because there are few pointer-chased permission structures. M.
5. Depends on higher-half kernel, firmware interface; M.

### Signed modules and secure boot (P1, M)
Linux: kernel module signing and lockdown mode ([module signing](https://docs.kernel.org/admin-guide/module-signing.html)), UEFI Secure Boot with shim; pain: the key-management ecosystem, out-of-tree modules (NVIDIA) needing MOK enrollment, lockdown breaking tracing. MogOs: no loadable kernel modules (drivers are crates compiled in or user-space servers, see section 8), so signing reduces to a signed kernel image and signed boot archive/initial servers: measured boot (hash chain into a TPM when present) plus per-binary signature or fs-verity-style Merkle roots on MogFS files ([fs-verity](https://docs.kernel.org/filesystems/fsverity.html)). P1 for cloud/enterprise, M, after UEFI.

### Rust-for-Linux (context, not a task)
- Linux decided at the 2025 Maintainers Summit that "the Rust experiment is concluded" and Rust is a permanent kernel language ([LWN 1050174](https://lwn.net/Articles/1050174)); Rust binder merged in 6.18, Nova (NVIDIA) is in; Android 16 ships Rust ashmem on kernel 6.12 (via the LWN and press coverage above; secondary). The kernel doc is [docs.kernel.org/rust](https://docs.kernel.org/rust/index.html).
- Pain points that Linux hits and MogOs avoids by design: Rust and C object lifetimes meeting at FFI boundaries (the real cost is the large `unsafe` binding layer and its semantics), maintainer friction (reviewers refusing to learn bindings), a single global allocator that can abort on OOM (Linux added fallible allocation APIs), no stable internal ABI so bindings churn.
- Memory-safety share, the case for MogOs' moat: Chromium measured about 70% of severe bugs as memory-safety issues ([chromium.org](https://chromium.org/Home/chromium-security/memory-safety)); Microsoft reported about 70% of its CVEs; Android's memory-safety share fell from 76% (2019) to under 20% (2025) as new code moved to Rust ([Google blog](https://blog.google/security/rust-in-android-move-fast-fix-things/); numbers via search summaries). Linux itself issues CVEs at a very high rate since becoming a CNA in February 2024 (the 2026 per-release counts in aggregator and AI-news sources are unverified; use as direction only, secondary).
- MogOs' honest limit: `unsafe` still exists in `arch`/`board`, and correctness bugs (logic, races across SMP, capability confusion) are not prevented by safe Rust. The moat claim is "no memory-corruption class from the kernel's logic crates", not "no CVEs". Keep the arch/board unsafe surface small and fuzzed (host-side harness for page-table and ELF loader code), and add a Miri/loom-style check for lock-free pieces.

# 7. Observability

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Tracepoints + ring buffer (ftrace/perf core) | P1 | M | SMP, per-CPU buffers |
| Sampling profiler (perf) | P1 | M | PMU, timer NMI-ish |
| Programmable tracing (eBPF equivalent) | P3 | L | verifier |
| Introspection API (procfs/sysfs lessons) | P0 | M | handles |
| Crash dump (kdump) | P1 | M | boot, kexec-like |
| Panic and fault dumps | P0 | done | exists |

### eBPF tracing, perf, ftrace (P1 core, P3 BPF)
1. Cheap always-available instrumentation: static tracepoints, function tracing, sampling, in-kernel aggregation ([ftrace](https://docs.kernel.org/trace/ftrace.html), [perf](https://docs.kernel.org/admin-guide/perf/index.html), [BPF](https://docs.kernel.org/bpf/index.html)).
2. Everyone running production; Linux's tracing story (bpftrace, perf, flame graphs) is a main reason SREs prefer it.
3. Linux: ftrace (function entry via compiler `mcount`/patching), tracepoints with per-CPU ring buffers, perf_event_open (one giant multiplexed syscall with a notorious attribute struct), eBPF (programs attached to kprobes/tracepoints). Pain: three overlapping subsystems, perf's security knob `perf_event_paranoid`, tracefs text interface, kprobes can crash the kernel in unlucky places, BPF verifier is a large attack surface.
4. Build one thing: static tracepoints compiled in (with a no-op fast path when disabled: an `AtomicBool` check, no code patching needed since no unsafe in logic crates) writing fixed-size binary records into per-CPU ring buffers that a handle (`trace` right) can read via completion I/O. Counters are the same interface. Sampling: timer-interrupt sampling of PC and call stack with frame pointers (aarch64 PMU later). Output in a standard format (Perfetto/CTF/Chrome trace JSON) via a user-space tool. This gives 80% of perf+ftrace. A programmable layer (WASM/restricted bytecode) is P3. Benchmarks demand it (BENCHMARKS.md's regression rule needs attribution of slowdowns).
5. Depends on SMP per-CPU storage; M. Adding the tracepoint skeleton early (phase 5) means every later subsystem lands instrumented.

### /proc and /sys, good and bad (P0, M)
1. Files that expose kernel state: [proc(5)](https://man7.org/linux/man-pages/man5/proc.5.html), sysfs ABI ([ABI doc](https://docs.kernel.org/admin-guide/abi.html)).
2. All tools (`ps`, `top`, `lsof`, container runtimes, monitoring agents) depend on them, so Linux compat needs them.
3. Good: discoverable, scriptable, no new syscall per question; sysfs is one-value-per-file with a stable ABI doc. Bad: procfs grew organically and inconsistently (hundreds of text formats parsed by regex, `/proc/PID/stat` fields with spaces in command names, `smaps` cost, `/proc/self/mem` and `/proc/PID/*` as security hazards (information leaks, e.g. ASLR addresses), races between listing and reading, and files that are ABI by accident, so changes are regressions (M10).
4. Native: typed introspection calls on handles (query process stats via the process handle, memory via the budget handle, trace via trace handle) returning fixed binary structs with version and size fields (the "botching-up-ioctls" rules: [kernel guide](https://docs.kernel.org/process/botching-up-ioctls.html)). Optionally expose a read-only introspection filesystem server (user-space, from the typed calls) for humans and scripts: a `ps` compat file tree is generated in the compat layer, not the kernel. Gain: no text parsing in the kernel, no accidental ABI.
5. Depends on handle types; M for the typed calls, S for a generated tree later.

### kdump and crash analysis (P1, M)
1. On panic, a second kernel boots and writes a memory dump ([kdump doc](https://docs.kernel.org/admin-guide/kdump/kdump.html)).
2. Servers and cloud fleets (post-mortem of rare bugs).
3. Linux: kexec loads a crash kernel in reserved memory; dump to `/proc/vmcore`. Pain: reserved memory cost, driver state after crash (devices still DMA-ing), complexity of `makedumpfile`, fragile on new hardware.
4. Simpler: on a kernel panic, a reserved region holds a compact structured dump (registers, per-CPU trace rings, scheduler/handle tables, last N log lines) written to a persistent dump partition via a polled, interrupt-free minimal virtio-blk/NVMe write path in `board`. Full memory dump optional later. Panic and fault dumps already print readable output (ROADMAP "Ongoing in every phase"). Core dumps for user processes: the exception channel (D6) delivers the fault to a supervisor which can read memory via the debug handle and write the core, so no kernel core-dump code.
5. Depends on trace rings, a polled storage path; M.

# 8. Drivers and hardware

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Device model + discovery (DT, ACPI, PCIe) | P0 (virt), P1 (HW) | L | boot work |
| Hotplug | P1 | M | device model |
| PCIe + NVMe + xHCI USB | P1 | L | device model, DMA/IOMMU |
| DRM/KMS-equivalent graphics | P2 | XL | device model, memory objects |
| Power management (suspend, cpufreq, cpuidle) | P2 (laptops, embedded) / P1 (cloud energy) | L | SMP, drivers |
| IOMMU / DMA safety | P1 | M | PCIe |
| Driver policy (no stable in-kernel ABI) | P0 decision | S | n/a |

### Device model and hotplug (P0/P1, L)
1. A bus/driver/device tree with binding, probe, remove, power state ([driver-model](https://docs.kernel.org/driver-api/driver-model/overview.html)).
2. All; Linux's driver breadth (thousands of devices) is its real moat and cannot be matched in the short term.
3. Linux: `struct device` hierarchy, bus types (platform, PCI, USB), deferred probe, devres, uevents and udev for hotplug, sysfs exposure. Pain: lifetime bugs (use-after-remove) are a top bug class in C; probe ordering by `initcall` levels; udev as a user-space policy daemon with races.
4. Define a `Driver` port trait per bus in the kernel crate (static dispatch via an enum or generics over a bounded driver set, no `dyn`) and keep each driver a safe crate talking to a small, audited unsafe MMIO/DMA wrapper in `board`/`arch` (volatile register blocks with typed fields; DMA buffers as a safe type that guarantees ownership during transfer). Device hotplug = a handle appearing in a device-manager directory handle; remove invalidates by generation (the handle table already prevents use-after-free). Strongest answer to Linux's driver-lifetime bugs: handle generations plus typed DMA ownership.
5. Depends on boot/device-tree work; L, the board-support effort of the hardware phase.

### Stable-ABI-free driver policy (decision)
Linux's policy: no stable in-kernel API ([stable-api-nonsense](https://docs.kernel.org/process/stable-api-nonsense.html)); drivers are in-tree and updated by whoever changes the API. Helps: kernel internals evolve freely; drivers get maintained collectively; no binary-driver lock-in for most devices. Hurts: out-of-tree drivers break per release (NVIDIA, ZFS), vendors get BSPs frozen on old kernels (Android's Generic Kernel Image and stable KMI is the fix, see below), hardware without upstreaming effort stays unsupported.
MogOs decision: copy the policy for in-kernel crates (no stable internal ABI), and avoid its pain by making the third-party driver path user space: drivers as unprivileged servers holding only device handles (MMIO region, IRQ, DMA window with IOMMU), speaking the stable native ABI. That gives vendors a stable ABI (the syscall surface) and takes drivers out of the TCB, at some latency cost that completion rings and polled queues recover. Keep in-kernel only: interrupt controller, timer, console, block and network drivers on the hot path until measured otherwise. This is an OPEN DECISION for the owner: it is compatible with "monolithic kernel" only if limited to third-party and non-hot-path devices; the monolithic core stays the default. Recommend deciding before the PCIe phase.

### PCIe, NVMe, USB (P1 for PCIe/NVMe, P2 for USB, L)
Linux has PCIe enumeration (ECAM), MSI-X, NVMe with per-CPU queues, xHCI for USB 3 with class drivers (HID, storage, audio, video). Pain: USB class driver quirks tables, descriptor-parsing bugs (a large fuzzing target), USB stack security (BadUSB). MogOs: virt machine exposes PCIe (ECAM in DT) and QEMU NVMe/xHCI devices for testing. Sequence: ECAM enumeration, MSI via GICv2m/ITS (GICv3 needed for ITS, so GICv2-to-v3 migration is a prerequisite on real boards), NVMe (best server payoff, simple spec), then xHCI + HID + mass storage. Safe-Rust descriptor parsers are an easy win. IOMMU (SMMU) support: required to let user-space drivers hold DMA safely, P1.

### DRM/KMS graphics (P2, XL)
Linux's DRM/KMS ([doc](https://docs.kernel.org/gpu/drm-kms.html)) is a good split: KMS does display modesetting atomically (planes, CRTCs, connectors), GEM/dma-buf shares buffers across devices, render nodes give unprivileged GPU access; Wayland rests on it. Pain: each GPU driver is huge and firmware-dependent, uAPI ioctls per driver (the reason for the botching-up-ioctls doc), fence/sync semantics took a decade (explicit vs implicit sync), no stable user-mode driver interface (Mesa moves in lockstep). The Rust path is real: Asahi's AGX driver, Nova ([Phoronix on Asahi DRM in Rust](https://www.phoronix.com/news/Asahi-Apple-DRM-In-Rust)).
MogOs: much later. First virtio-gpu (2D scanout, then virgl/venus) on QEMU; graphics buffers are the same memory-object handle (section 2) so dma-buf is not a separate concept; a display-controller handle with atomic commit semantics; explicit-fence objects only (no implicit sync). Wayland compat is a user-space compositor; P2.

### Power management: suspend, cpufreq, cpuidle (P2, L)
Linux: [sleep states](https://docs.kernel.org/admin-guide/pm/sleep-states.html), [cpufreq](https://docs.kernel.org/admin-guide/pm/cpufreq.html) governors (schedutil ties frequency to scheduler utilization), [cpuidle](https://docs.kernel.org/admin-guide/pm/cpuidle.html) governors, runtime PM per device, energy-aware scheduling on big.LITTLE. Pain: suspend/resume reliability is the perennial laptop complaint (every driver must implement suspend/resume; firmware bugs), governor heuristics, PM and RT conflicts. MogOs: `wfi` idle already exists (phase 3 step 15). For embedded and mobile, PSCI `CPU_SUSPEND` and `SYSTEM_SUSPEND` and DVFS through SCMI are the aarch64 tools. Steps: tickless idle (needed first: no periodic timer when idle), idle states via PSCI, utilization-driven DVFS keyed off the fair class (same idea as schedutil, inferred), device runtime PM as part of the driver trait (`suspend`/`resume` mandatory methods, so no driver can forget). Datacenter energy proportionality makes P1 for cloud cost, P2 for desktop; L.

# 9. Ecosystem

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| Stable syscall ABI discipline | P0 (policy) | S | n/a |
| Linux binary compatibility layer | P1 (the adoption bridge) | XL | fork/CoW, signals, futex, epoll, /proc fakes |
| ELF + dynamic linking (ld.so, TLS, vDSO) | P0 | L | file mmap, threads |
| Packaging and distribution | P1 | M | MogFS, signing |
| Containers / OCI runtime | P1 | M | compat layer, resource groups |
| Virtualization: guest (virtio) and host (KVM equiv.) | P1 guest, P2 host | M / XL | virtio, EL2 |
| Live patching / live update | P3 | L | SMP, checkpointing |
| Release and maintainer process | P0 (policy) | S | n/a |

### "We don't break userspace" (P0 policy, S)
1. The kernel-to-user ABI never changes incompatibly ([regressions doc](https://docs.kernel.org/admin-guide/reporting-regressions.html)).
2. All users: this is why a 15-year-old binary runs and why vendors trust Linux; it is the opposite of Windows' compat shims and macOS' deprecations.
3. Linux: the rule holds for syscalls and what is observable (even `/proc` formats and accidentally-working behavior, which is the pain: ABI by accident and permanent cruft such as `ioctl` numbers, `stat` struct variants, `clone` flag soup, three `select` generations, `statx` replacing `stat`, `openat2` and `clone3` fixing missing extensibility). Lesson: syscalls without extensibility (fixed args) get replaced.
4. Copy the policy as soon as there are third-party binaries. Design every new syscall with: a size-prefixed, versioned argument struct (not register-packed args beyond 4-5), flags field that must be zero-checked (reject unknown flags: Linux's `clone`/`openat` ignored unknown flags and could not extend; `openat2` and `clone3` fixed this), and a documented error set. Today's ABI packs args in x0-x5 (phase 3 convention); fine for hot calls, struct form for the rest. Freeze the native ABI at a declared milestone (e.g. phase 9) with a published numbering and an ABI test corpus run in CI (binaries built once, run on every kernel; a regression test per syscall). Keep the Linux compat layer separate (it must never shape kernel internals, AGENTS.md).
5. S for policy, M for the corpus.

### Linux binary compatibility (P1, XL)
1. Run unmodified Linux aarch64 ELF binaries by translating syscalls to native ones.
2. The adoption bridge: containers, Go/Java/Python workloads, vendor software. Without it the ecosystem argument is lost; with musl source compatibility alone, only recompiled software works.
3. Precedents: FreeBSD Linuxulator, WSL1 (translation layer; filesystem performance and incomplete syscalls were its downfall, replaced by a real Linux VM in WSL2), gVisor (user-space kernel; performance and compat gaps), Fuchsia starnix (runs Linux binaries on Zircon: closest to MogOs; [starnix docs](https://fuchsia.dev/fuchsia-src/concepts/starnix)). Lessons: compat surface is dominated by a long tail (`/proc`, `/sys`, `ioctl` on devices, `clone` flag combinations, `futex`, `epoll`, signals, `ptrace`, `io_uring`, `eventfd`, `timerfd`, `memfd`, `inotify`). WSL1's file I/O slowness came from translating Linux VFS semantics onto NTFS.
4. Do it as a user-space "starnix-like" process per Linux program set (the compat server holds the Linux-visible state: fd table, signals, pids, `/proc`), calling native syscalls. Pros: contained blast radius (a bug there compromises only that container), no kernel changes for Linux quirks, matches "must never shape kernel internals". Cost: an extra hop per syscall unless the syscall is directly forwardable; the existing plan (musl's Linux-number dispatcher in libc) is the static case; real Linux binaries need `svc` interception: a kernel feature "redirect syscalls of this process to a handler" (like seccomp user-notif or Zircon's syscall redirection), the one kernel hook starnix style needs. Order: pass musl-static binaries first, then glibc dynamic, then container images. Cover top ~120 syscalls, then add by tracing real workloads.
5. Depends on fork/CoW, threads, futex, signals, epoll, `/proc` fakes, memory objects, ELF loader with dynamic linking; XL, spans phase 9.

### ELF and dynamic linking (P0, L)
1. Loading shared objects at run time (`ld.so`), TLS, relocations.
2. Everyone: nothing from a distribution runs statically; plugins, Python modules, glibc.
3. Linux: kernel loads ELF and an interpreter (`PT_INTERP`), auxv passes entry info; symbol versioning; vDSO. Pain: `LD_PRELOAD` and `LD_LIBRARY_PATH` as ambient attack surface (setuid environment sanitizing), ASLR of the loader, kernel-side ELF parsing in C (historic CVE source).
4. The ELF parser already exists in the kernel crate (`PT_LOAD` only, step 14); a safe parser is cheap. Move the work to user space: kernel maps only the interpreter and the main image from a memory-object handle (loader as a user-space service), TLS via `TPIDR_EL0` (exists), dynamic loading uses `map` with W^X (map RW, relocate, `protect` to RX; needs a `protect` call with rights, since JITs need it too). musl's `ldso` can be used. glibc compat via the compat layer. Auxv replaced by a startup message with handles (as spawn passes).
5. Depends on file mmap, threads (TLS), `protect`; L.

### Packaging, containers (OCI), virtualization (P1)
- Packaging: Linux has no standard; distros use deb/rpm/pacman, containers and Nix/Flatpak/Snap fill gaps; pain is dependency hell and version skew. MogOs advantage: capability model plus CoW snapshots allow content-addressed, atomic, rollback-able system images (ostree/Nix-like, inferred): a package is a signed subtree, a "system" is a set of subtrees composed by directory handles. M, after MogFS snapshots; do not write a package manager before there is software to package.
- Containers (OCI): the runtime-spec (config.json) maps onto spawn args plus resource-group handle plus restricted directory handle (D7), a thin `runc`-compatible shim; images are layers on MogFS clones (section 4). M, depends on compat layer.
- Virtualization: guest side, virtio-blk (done), virtio-net, virtio-console, virtio-rng, virtio-balloon, vsock (key for cloud agents), vhost; this is how you run on every cloud: P1 and cheap (spec at [virtio v1.2](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)). Host side: Linux KVM ([API doc](https://docs.kernel.org/virt/kvm/api.html)) made Linux the cloud hypervisor base (with QEMU/Firecracker/Cloud Hypervisor on top). MogOs host side would use EL2 (VHE) with a VM as a process holding vCPU handles; a small safe hypervisor could be a market differentiator (smaller TCB than KVM+QEMU) but is XL and P2/P3.
- Live patching: Linux livepatch ([doc](https://docs.kernel.org/livepatch/livepatch.html)) replaces functions at run time using ftrace and consistency models; used by cloud fleets that cannot reboot. Pain: only function-level, hard to review, tooling per distro. MogOs: a kernel built from crates with narrow ports can support whole-component live update later (replace a driver server, replace a filesystem server) via user-space server restart; for the monolithic core, fast reboot with kexec-like handoff and preserved user-space state is simpler. P3.

### Release and maintainer process (P0 policy, S)
Linux: a 9-10 week merge-window-plus-rc cycle, LTS branches for 2-6 years, stable-tree backports via the CNA tagging, subsystem maintainers with pull-based trees, mailing-list review, `Fixes:` tags, the regression tracker (regzbot), linux-next, syzbot and KernelCI. Pain: reviewer burnout (the 2024-2025 Rust and maintainer friction episodes, [LWN 1050174](https://lwn.net/Articles/1050174) discusses the Rust side), email workflow as a barrier, huge CVE volume since 2024 from CNA policy of "every fix is a CVE" (secondary). MogOs: it is a small project, so copy the cheap parts: a time-based release train, a regression rule (benchmark gate already exists, AGENTS.md "A tracked benchmark slowing down is a failure"), a fuzzing CI (host fuzz for parsers, QEMU fuzz for syscalls), `Fixes:` tags, one reviewer pass per change (already in WORKFLOW.md). S.

# 10. Boot

| Item | Tier | Size | Depends on |
| --- | --- | --- | --- |
| UEFI boot (aarch64) | P1 | M | PE/COFF loader, memory map handoff |
| Device tree and ACPI | P0 (DT, exists), P1 (ACPI for servers) | M each | dtb crate exists |
| Initramfs / boot archive | P0 | exists | cpio exists |
| kexec / fast reboot | P2 | M | quiesce drivers |
| Boot-time budget | P0 | exists | `boot: <N> us` line |

1. UEFI: the firmware standard on servers and arm64 PCs; needed to boot on real servers or a Pi 4/5 with edk2 firmware. Linux has an EFI stub (the kernel is its own PE/COFF bootloader, [EFI stub doc](https://docs.kernel.org/admin-guide/efi-stub.html)). MogOs: produce a PE/COFF image with an EFI stub that gets the memory map and DTB/ACPI pointers; QEMU can boot via edk2 (`-bios`). M.
2. Device tree vs ACPI: Linux supports both on arm64; DT dominates embedded, ACPI (SBBR/SBSA) dominates arm servers. The DT parser exists (`crates/dtb`). ACPI is a large table-driven spec with an AML interpreter (needed for power buttons, hot-plug, thermal); a safe minimal reader for static tables (MADT for GIC, MCFG for PCIe, SRAT for NUMA, GTDT) is enough initially, skipping AML. P1 for servers, M. Pain on Linux side: ACPI AML bugs in firmware, DT binding churn.
3. initramfs: Linux boots a cpio into tmpfs as the early root so it can load storage drivers ([initrd doc](https://docs.kernel.org/admin-guide/initrd.html)). MogOs already bundles a cpio archive (phase 3 step 14). Keep: initial archive holds init, driver servers (if user-space drivers are chosen), and the loader. Lesson from Linux: the initramfs needed generators (dracut, mkinitcpio) because modules and firmware vary by machine; with in-kernel crates and no modules this problem is smaller. Firmware blobs (Wi-Fi, GPU) are the remaining reason for a data archive.
4. kexec: boot a new kernel without firmware/POST (seconds saved on servers; also the kdump mechanism). Pain: device quiescing, EFI runtime state. MogOs: P2 as "fast reboot / live update" after drivers have a `quiesce` method.
5. Boot time: MogOs already prints `boot: <N> us`. Competitive advantage: sub-100 ms kernel boot matters for serverless/microVM (Firecracker competes on this). Keep the metric gated (BENCHMARKS.md).

# 11. Linux's biggest mistakes and regrets, with MogOs status

Status key: AVOIDS (design already prevents it), PARTIAL (direction right, work remains), NEEDS (not addressed yet; where).

| # | Mistake | MogOs status | Where it is handled |
| --- | --- | --- | --- |
| M1 | fork + overcommit + OOM killer | AVOIDS (spawn, no overcommit, budgets); PARTIAL for fork compat | phase 6 step 34: budget-charged fork |
| M2 | ioctl sprawl, multiplexed syscalls | AVOIDS by rule (phase 3 ABI rules: "no ioctl-style multiplexers"); NEEDS enforcement | phase 9 step 58: ABI audit, versioned structs |
| M3 | Signals | PARTIAL (libc-synthesized; no kernel exception path yet) | phase 9 step 53 |
| M4 | select/poll scalability, API accretion | AVOIDS (poll is a completion op); NEEDS cancel and multishot | phase 7 step 39 |
| M5 | procfs/sysfs API sprawl | AVOIDS today (nothing exists); NEEDS typed introspection | phase 9 step 56, phase 10 |
| M6 | capabilities(7) granularity | AVOIDS (handles with rights, no ambient bits) | audit per subsystem |
| M7 | user namespaces' attack surface | AVOIDS (no userns; compat fakes uid) | rule D7 |
| M8 | fsync and ext4 data loss, fsyncgate | AVOIDS (sticky I/O error, flush tested, CoW commit); NEEDS per-file sync and group commit | phase 6 step 36, phase 7 |
| M9 | Blocking-I/O legacy, then io_uring and its security record | PARTIAL: completion I/O is native, but `open`/`readdir` are synchronous; ring not built yet | phase 7 steps 39, 45 (D8) |
| M10 | TTY layer | PARTIAL: line discipline lives in the kernel (`kernel::console::Line`); no ptys | phase 9 step 53 |
| M11 | ptrace limits | NEEDS (debug handle) | phase 10 step 61 |
| M12 | C memory-safety share of CVEs | AVOIDS in logic crates; residual `unsafe` in arch/board | fuzz + audit, phase 10 |
| M13 | Retrofit of preemption/RT and big kernel lock | AVOIDS if the lock model is decided now (D3) | phase 5 step 24 |
| M14 | Page cache charged to first toucher (memcg) | AVOIDS if D2 adopted | phase 6 step 35 |
| M15 | POSIX file locks and `O_PONIES` semantics | NEEDS to choose: do not implement POSIX `fcntl` locks in kernel; use handle-bound locks (OFD semantics) | phase 9 compat |
| M16 | ABI by accident (`/proc` formats, ignored unknown flags) | PARTIAL (rule exists for handles; flags rule not written) | phase 9 step 58 |
| M17 | Driver lifetimes and the in-tree-only driver treadmill | PARTIAL (handle generations; driver model undecided) | phase 11 decision |
| M18 | cgroup v1/v2 split, namespace-by-namespace container construction | AVOIDS (one resource-group handle, D7) | phase 5 step 30 |
| M19 | THP/compaction latency, swap thrash | AVOIDS (explicit huge pages, no swap) | phase 6 step 37 |
| M20 | Time and ids: `time_t` 2038, uid/gid models, clock sprawl | AVOIDS if native ABI uses 64-bit ns time and no uid | rule |

### Notes and sources per mistake

- M1. Overcommit lets `malloc` succeed without backing, so under pressure the kernel must pick a victim: [Taming the OOM killer](https://lwn.net/Articles/317814/) (2009); the OOM killer has since sprouted `oom_score_adj`, cgroup OOM, and user-space killers (systemd-oomd uses PSI and cgroup v2, [systemd-oomd(8)](https://www.mankier.com/8/systemd-oomd.service), search-confirmed). Fork is the structural reason: a 10 GiB process cannot fork under strict accounting ([overcommit accounting](https://docs.kernel.org/mm/overcommit-accounting.html)). Paper: [A fork() in the road](https://www.microsoft.com/en-us/research/publication/a-fork-in-the-road) (fork is not thread-safe, slow, insecure by default inheritance). MogOs: spawn plus budgets are in place (phase 3 steps 13-14).
- M2. [Botching up ioctls](https://docs.kernel.org/process/botching-up-ioctls.html) is the kernel's own admission; [ioctl(2)](https://man7.org/linux/man-pages/man2/ioctl.2.html) is the catch-all. Lesson list: size/flag fields, zero-check unknown flags, no pointer-width-dependent structs, one verb per call.
- M3. See [signal(7)](https://man7.org/linux/man-pages/man7/signal.7.html); Linux later added `signalfd`, `pidfd` ([pidfd_open(2)](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)) to escape signals, evidence that the model itself was the problem. MogOs already has process handles with `wait`.
- M4. [select(2)](https://man7.org/linux/man-pages/man2/select.2.html) BUGS section: FD_SETSIZE and O(n) cost; epoll fixed scaling but not regular files; io_uring as the third generation.
- M5. See section 7. Parsing `/proc/PID/stat` is a known footgun (secondary, [proc(5)](https://man7.org/linux/man-pages/man5/proc.5.html) documents the format).
- M6. See section 6; [capabilities(7)](https://man7.org/linux/man-pages/man7/capabilities.7.html) itself has a section on `CAP_SYS_ADMIN` overload.
- M7. See section 6 (user namespace bypass and restriction history).
- M8. [LWN 322846](https://lwn.net/Articles/322846) and [323169](https://lwn.net/Articles/323169) cover ext4 delayed-allocation zero-length files (2009; the `auto_da_alloc` mitigation); [PostgreSQL fsync errors](https://wiki.postgresql.org/wiki/Fsync_Errors) and [danluu](https://danluu.com/fsyncgate/) cover 2018.
- M9. [Google, June 2023](https://security.googleblog.com/2023/06/learnings-from-kctf-vrps-42-linux.html): io_uring accounted for 60% of kernel-exploit submissions that year and was disabled on ChromeOS and production servers. Origin story: [LWN 776703](https://lwn.net/Articles/776703/).
- M10. Linux's TTY layer carries line disciplines, N_TTY legacy, multiple locking rewrites (Big TTY Mutex removal around 3.x; locking fixes still appear in 5.10 pull requests, via commit-log search results; secondary), and `TIOCSTI` input injection that was finally disabled by default. Recommendation: the console is a raw byte-stream handle; line editing/echo lives in a user-space tty service or libc (pty = a pair of stream objects); only the interrupt-driven UART receive stays in the kernel. Today's `console::Line` is small (256 bytes) but it is policy in the kernel: move it when pty/termios arrives.
- M11. [ptrace(2)](https://man7.org/linux/man-pages/man2/ptrace.2.html) BUGS and caveats section; one tracer per tracee, signal-based stops, race windows.
- M12. [Chromium](https://chromium.org/Home/chromium-security/memory-safety) (about 70% of severe bugs), [Android](https://blog.google/security/rust-in-android-move-fast-fix-things/) (memory-safety share under 20% in 2025 from 76% in 2019). Linux's response is Rust-for-Linux ([LWN 1050174](https://lwn.net/Articles/1050174)), a decade-long incremental path MogOs does not have to take.
- M13. Linux's PREEMPT_RT took from 2005 to 6.12 (2024) ([LWN 992184](https://lwn.net/Articles/992184)) because spinlock-everywhere had to be audited after the fact.
- M15 (inferred, general knowledge): POSIX record locks are per-process and drop when any fd to the file closes; OFD locks (Linux 3.15) fixed it. MogOs: locks as handle-bound objects only if needed.
- M17. [stable-api-nonsense](https://docs.kernel.org/process/stable-api-nonsense.html), and Android's Generic Kernel Image effort (general knowledge) show the cost of out-of-tree drivers on a moving internal API.

# 12. Recommended roadmap after phase 4 (phases 5 and up)

Assumption: phase 4 closes with step 23 (musl + busybox, no-fork, one core). Steps keep global numbering (24 onward). Each phase is about 7 steps, like phases 3 and 4. Ordering rules: (1) anything that creates concurrent data structures waits for the locking model; (2) memory primitives (refcounted frames, memory objects, page cache) come before the filesystem and network features that depend on them; (3) after dependencies, order by competitive gain per effort. Every step keeps the existing gates: e2e test, no `panic:`, tracked benchmarks (syscall 28 ns, yield 65 ns, pipe 363 ns under hvf must not regress on 1 core; SMP adds new benchmarks).

## Phase 5: SMP, threads, fair scheduling

Goal: several cores run threads of one process under a fair scheduler with resource groups, with the locking model fixed for everything that follows.

| # | Step | Done when |
| --- | --- | --- |
| 24 | Lock model and trace skeleton | `arch` provides an IRQ-safe `SpinLock<T>` (ticket) and per-CPU cells (TPIDR_EL1) behind a safe API; scheduler, pipes, mutexes and handle tables sit behind it on one core; static tracepoints write to a per-CPU ring readable over the console. All e2e pass and syscall/yield/pipe benchmarks are within noise. |
| 25 | Wait-on-address | `wait(addr, expected, timeout)`, `wake(addr, n)` and a PI lock on an address, bounded table keyed by (space, address). e2e: two tasks contend on a user-space lock, the uncontended path makes no syscall, and a PI test mirrors `test=pi`. |
| 26 | Threads | `thread_create(entry, stack, tls)` in the caller's address space, thread handle with join, TLS via `TPIDR_EL0`, budget charged for stack and kernel stack, process exit and `kill` reap all threads. e2e: N threads increment a counter under the step-25 lock; join returns each status. |
| 27 | SMP bring-up | PSCI `CPU_ON`, per-CPU stack and idle, GICv2 SGIs as IPIs, TLB shootdown on unmap and ASID reuse; `-smp 4`. e2e: four cores report online and each takes timer ticks; killing a thread running on another core completes. |
| 28 | Per-CPU run queues | Per-CPU queues, idle pull, affinity as a handle right, wake preemption (local and by IPI). e2e: four spinners each get a core; a higher-priority wake preempts within a bounded number of ticks; `test=pi` passes with the mutex owner on another core. |
| 29 | Fair class | EEVDF-style class below the RT levels: weights, slice as latency hint, lag. e2e: weights 2:1 yield CPU within 5% over a second; an interactive waker's latency stays under a set bound beside four hogs. |
| 30 | Resource groups | A group handle with CPU weight, CPU bandwidth, task cap; budget stats and resize calls. e2e: a 25% bandwidth cap holds within 3%; a spawn over the task cap fails with a named error. |
| 31 | musl pthreads and SMP scaling | musl's thread primitives over wait-on-address; a C pthread test (mutex, condvar, join) passes; benchmarks for syscall, pipe and yield on 4 cores plus lock-contention counters are recorded. |

Why this order: the lock model (24) is the only item that gets more expensive every phase it is deferred (page cache, net stack, more object tables all assume it), so it goes first while the kernel is still small. Wait-on-address and threads (25-26) are testable on one core and are the P0 gaps for musl and every server runtime; doing them before SMP lets bugs show up without races. SMP and the fair class (27-29) are the largest competitive gain; groups (30) are the cheap cloud-readiness win because memory budgets already exist.

## Phase 6: Virtual memory

Goal: demand paging under no-overcommit, a page cache, file mapping, CoW fork, hardened kernel mappings.

| # | Step | Done when |
| --- | --- | --- |
| 32 | Higher-half kernel, W^X kernel map | Reverses the phase 3 step 11 decision ("no higher-half kernel until Linux compatibility needs it"): needed now for the W^X kernel map, KASLR and the TTBR split. Kernel runs from TTBR1 with RX text, R rodata, RW data; a test write to kernel text faults; user and kernel tables are separate. PAN is enabled where the CPU has it (ARMv8.1; cortex-a72 does not), otherwise not claimed. Syscall benchmark does not regress. |
| 33 | Refcounted frames, demand paging | Per-frame counts in `mm`; `map` charges the budget up front, frames appear on first touch; failure only at `map`. e2e: a 64 MiB `map` touching 1 MiB uses about 1 MiB of frames, a map over the budget fails at `map` time, and a `reserve` of 1 GiB of address space (no charge) followed by a 1 MiB `commit` works; `exec` maps ELF pages lazily; exec benchmark recorded. |
| 34 | CoW `clone_space` and libc `fork` | Child budget charged up front. e2e: child writes do not reach the parent; fork under an insufficient budget fails with `ENOMEM`; a 32 MiB process (parent plus child budgets fit in 128 MiB RAM) forks within a recorded time. |
| 35 | Memory objects, file mmap, page cache read path | One memory-object handle type (anonymous, file, shared); cache keyed by (inode, page) under a global cap, checksum verified on fill; spawn reclaims cache. e2e within MogFS v1 limits (files up to 57,232 bytes): a second read of a 56 KB file causes zero disk reads (counter); mmap'd file reads match `read`. |
| 36 | Writeback and dirty bound | Writeback task, per-device dirty limit, `msync`/`sync` through the cache, sticky errors preserved. e2e within v1 limits: rewriting a 56 KB file in a loop for 100 MiB of total writes stays under the bound; a QEMU kill mid-write mounts to a consistent state; an injected flush error surfaces and sticks. |
| 37 | Explicit huge pages | A `map` flag yields 2 MiB mappings from a boot-time pool and fails explicitly when empty. A TLB-bound benchmark improves (number recorded, step dropped if it does not). |
| 38 | KASLR, mapping seal, first Spectre baseline | Base differs across boots (DT seed); `seal` right blocks later remap/protect; SMCCC workarounds and index clamping in handle lookup; benchmarks hold. |

Why this order: the page cache (35-36) is deliberately built against MogFS v1 limits, since it only needs the block-level interface, and v2 (phase 7 step 40) is a host-tested pure-crate change that can land in either order; if the owner wants large-file cache tests earlier, pull step 40 ahead of 35. Higher-half first because KASLR, TTBR-split isolation, and compat all need it; refcounts before CoW and the cache; the cache before anything storage- or network-heavy; huge pages and KASLR last because they are polish on a working base. Compat cannot start without fork (34) and file mmap (35).

## Phase 7: Storage that scales

Goal: MogFS holds real data volumes with snapshots; the block path is asynchronous and multi-queue.

| # | Step | Done when |
| --- | --- | --- |
| 39 | Async file ops, cancel, queue depth | `open`, `readdir`, `sync` become completion ops; `io_cancel(token)`; virtio-blk takes many requests in flight. e2e: 32 reads in flight beat queue depth 1 by a recorded factor; cancel returns defined outcomes. |
| 40 | MogFS v2 format | Extents, growable inode table, hashed directories, long names, 64-bit sizes, `mkfs`. Host: a 1 GiB file and 100k entries in one directory; the 200-seed power-cut test passes on the new format. |
| 41 | Snapshots, clones, reserve, scrub | O(1) snapshot, writable clone, reserved metadata space so commit never fails for space, scrub finds injected bit flips. Host and e2e tests cover each. |
| 42 | Change notification | Watch handle on a directory subtree with an overflow event; `changed_since(generation)` lists touched paths. e2e: create, write, rename are seen; overflow is explicit. |
| 43 | Filesystem server channel | A user-space process serves a directory handle (FUSE equivalent) with PI across the call; a FAT or tmpfs server mounts. e2e: files read through it; server death gives clean errors. |
| 44 | PCIe ECAM and NVMe | Enumerate PCIe from the DT, NVMe with per-CPU queues (interrupts via GICv2m MSI on QEMU virt; real-board MSI waits for step 67); boot a disk on NVMe. Throughput benchmark recorded. |
| 45 | Shared completion ring (conditional) | Built only if a benchmark shows at least 2x over `io_submit_wait` at batch 32; same ops and rights as syscalls (D8), no privileged workers, host fuzzer for the ring protocol runs clean. Otherwise record the result and skip. |

Why this order: the 57 KB file cap and flat directories block every real program, so MogFS v2 gates compat and packaging; snapshots are what make containers and packaging cheap later. NVMe waits behind per-CPU queues (phase 5) and is movable to phase 11 if hardware work starts earlier.

## Phase 8: Networking

Goal: a safe TCP/IP stack with capability-scoped sockets, reaching a web server and client on QEMU.

| # | Step | Done when |
| --- | --- | --- |
| 46 | virtio-net and buffer pool | RX/TX through QEMU user networking from a budgeted fixed pool. e2e: a frame round-trips; counters visible. |
| 47 | L2/L3 crate: Ethernet, ARP, IPv4, ICMP, UDP | Safe `no_std` crate over a `NetDevice` trait, host-tested and fuzzed. e2e: `ping` to the QEMU gateway; UDP echo. |
| 48 | TCP | Reliable stream with timers wheel, NewReno then CUBIC behind a trait. Host: loss, reorder and duplicate simulation; e2e: echo server over host forwarding. |
| 49 | Sockets as handles | Net-stack handle grants access; socket handles with rights; typed options; libc BSD sockets, DHCP, DNS resolver. e2e: fetch a page from a host HTTP server; a child without the net handle cannot connect. |
| 50 | Scaling and zero-copy | Per-CPU flow steering, multiqueue virtio-net, `copy(src, dst, len)` op. iperf-class benchmark recorded with before/after. |
| 51 | IPv6, NDP | Dual stack; host tests and an e2e ping6. |
| 52 | Packet filter and net-queue handle | Rule table gated by a net-admin handle; a queue handle gives direct ring access to a NIC queue. e2e: a rule drops traffic; a user-space program receives raw frames. |

Why this order: after SMP and memory (buffers, refcounts, per-CPU), before compat (most Linux workloads are networked). The stack is the largest concurrent data structure, hence phase 5 first. BBR and eBPF-style programmability are deliberately left off (P3).

## Phase 9: POSIX completeness and Linux compatibility

Goal: unmodified Linux aarch64 programs and container images run; the native ABI is frozen with a test corpus.

| # | Step | Done when |
| --- | --- | --- |
| 53 | Exception channel, libc signals, ptys | Faults delivered to a supervisor handle; libc builds `sigaction`, `kill`, SIGSEGV handlers, job control; pty pair objects; line discipline moves out of the kernel. e2e: a SEGV handler runs; `^C` and `^Z` work in the shell. |
| 54 | Dynamic linking | `protect` call, user-space loader, musl `ldso`; a program loads a shared object. e2e: `dlopen` works and W^X holds. |
| 55 | Syscall redirection and compat server | A per-process kernel hook redirects `svc` to a compat server; static Linux/musl binaries run with the top 60 syscalls. e2e: a Linux static busybox runs `ls` and `cat`. |
| 56 | Event and notify shims, generated `/proc` | `epoll`, `eventfd`, `timerfd`, `signalfd`, `inotify` shims over native ops; `/proc` and `/sys` trees generated in the compat layer. e2e: a Python or Node event loop runs. |
| 57 | glibc dynamic binaries | A Debian-based rootfs runs a shell, coreutils, and a package-manager read-only command; failures tracked by syscall coverage. |
| 58 | ABI freeze | Versioned size-prefixed structs, unknown-flag rejection audit, published numbering, a corpus of prebuilt binaries run in CI on every build. |
| 59 | Containers | Resource-group handle, restricted directory handles, an OCI runtime shim, image layers as MogFS clones. e2e: run an Alpine rootfs container with a CPU cap and no network handle. |

Why this order: signals and ptys (53) and loader (54) are prerequisites for any real program; the compat server (55-57) is the adoption bridge and has to sit on phases 5-8; freezing the ABI (58) only after the compat work has exercised it; containers last because they combine groups, restricted handles and MogFS clones.

## Phase 10: Observability, debugging, security hardening

Goal: production-grade visibility and a hardened, fuzzed trust boundary.

| # | Step | Done when |
| --- | --- | --- |
| 60 | Trace tooling and profiler | Trace rings exported to a Chrome-trace/Perfetto file by a user tool; timer-sample profiler with frame pointers. e2e: a flame graph from a benchmark run. |
| 61 | Debug handle and gdbstub | Read/write memory and registers, suspend, step, breakpoints through a `debug` right; gdbstub in user space. e2e: scripted debugger session breaks at a function. |
| 62 | Crash dumps | Structured panic dump (registers, rings, scheduler and handle tables) written to a reserved partition by a polled path; user cores via the exception channel. e2e: induce a panic and read the dump next boot. |
| 63 | Audit and revocation | Log of handle grants and denials; revocable handles. e2e: a revoked handle fails with a named error. |
| 64 | Fuzzing CI | Host syscall fuzzer against the kernel crates, parser fuzzers (ELF, cpio, MogFS, packets), loom-style checks for lock code. Clean for a fixed time budget on every release. |
| 65 | Signed images and file integrity | Signed kernel image and boot archive; Merkle-root verification of MogFS files (fs-verity style). e2e: a tampered binary refuses to run. |
| 66 | Mitigation completion | Predictor scrub for untrusted flag, side-channel regression tests, benchmark gate holds. |

Why this order: tracing is needed to find regressions in the earlier phases, but only the skeleton (24) is on the critical path; the rest follows once compat and network exist to be observed. Fuzzing comes after the surfaces it covers exist.

## Phase 11: Real hardware and boot

Goal: boot and run on a real arm64 board and an arm64 server, with power management.

| # | Step | Done when |
| --- | --- | --- |
| 67 | GICv3 and MSI | GICv3 with ITS for MSI-X; the QEMU `gic-version=3` machine boots. |
| 68 | UEFI boot and static ACPI tables | EFI stub image; MADT, MCFG, GTDT, SRAT read; boots under edk2 on QEMU. |
| 69 | Driver model decision and IOMMU | Decide in-kernel vs user-space drivers for third-party devices; SMMU support; DMA windows per device handle. A driver server gets only its device handles. |
| 70 | xHCI USB with HID and mass storage | A USB keyboard and a USB disk work on QEMU, then hardware. Descriptor parsers fuzzed. |
| 71 | First real board | One board (for example a Raspberry Pi 5 or an arm64 server) boots to msh with console, storage and network. |
| 72 | Tickless idle, idle states, DVFS | No periodic tick when idle; PSCI idle states; utilization-driven frequency. Idle power measured on the board. |
| 73 | NUMA and kexec-style fast reboot | Per-node frame pools and affinity; fast reboot with quiesced devices. |

Why this order: hardware work is large and mostly board-specific; it benefits from everything above being stable, and from the driver decision (69) being made with real evidence from the compat and network work.

## Phase 12: Graphics, desktop, virtualization

Goal: a desktop session and a cloud-ready guest and host.

| # | Step | Done when |
| --- | --- | --- |
| 74 | virtio-gpu and display handle | 2D scanout through atomic-commit display handle, shared buffers as memory objects. |
| 75 | Input and compositor | HID events as streams; a minimal compositor on the Wayland protocol or a native one. |
| 76 | Audio | virtio-sound or HDA with a mixer service. |
| 77 | Full virtio guest set and vsock | virtio-rng, console, balloon, vsock; a cloud-image boot under 100 ms recorded. |
| 78 | EL2 hypervisor | A VM as a process holding vCPU handles; boot a MogOs guest inside MogOs. |
| 79 | Live update | Replace a user-space server (filesystem, network driver) without dropping clients. |

Why last: graphics and virtualization are the widest and least differentiated areas (Linux's breadth is hardest to match), so they come after the server story is complete; the handle and memory-object model built earlier is what makes them smaller than on Linux.

# 13. Open questions I could not resolve

- Whether user-space drivers (section 8) are compatible with the "monolithic kernel" decision; needs an owner decision before phase 11, and arguably before phase 7 (the NVMe driver is the first candidate).
- Safe TCP/IP: build on `smoltcp` or write fresh; smoltcp's fixed-memory and licensing fit was not checked (inferred only).
- Cost of syscall redirection for Linux compat versus the 28 ns native syscall; needs a spike before phase 9.
- Several figures are from secondary summaries (Android 76% to under 20%; Microsoft 70%; per-release CVE counts; BBR fairness; THP benefits) and are marked as such; re-verify before quoting externally.
- No numbers in this survey are measured on MogOs; all sizes are rough estimates based on the existing phase sizes (about 6 steps per phase).
