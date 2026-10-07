# Phase 8: Networking

Goal (milestone): MogOs serves a web page to the Mac host through QEMU user networking, and fetches one. The e2e test runs an HTTP server on the host and boots MogOs with `-netdev user,id=n0,hostfwd=tcp:127.0.0.1:<port>-:80 -device virtio-net-device,netdev=n0`; the test GETs the guest's page from `127.0.0.1:<port>` and checks its body, and the guest's `fetch 10.0.2.2:<host port>` prints the host page's body. (QEMU user networking puts the guest at 10.0.2.15 and the host at 10.0.2.2; verify on QEMU 9.2 in step 49.)

## Steps

Steps 46-48 are pure: a new crate, `crates/net` (safe, `no_std`, no `alloc`, no dependencies), host-tested on the Mac over a simulated link, needing no SMP and no virtual memory change, so they run in parallel with phases 5 and 6 and with phase 7's steps 39-41. They run in order (48 is conditional). Steps 49-52 integrate with the kernel and wait on phases 5 and 6 (each lists what it needs); 49 and 50 run in parallel, 51 joins them, 52 follows 51.

| # | Step | Done when |
| --- | --- | --- |
| 46 | L2/L3 crate: Ethernet, ARP, IPv4, ICMP, UDP (pure) | `crates/net` over a `Nic` trait, with the smoltcp decision below recorded. Host: two stacks over the simulated link resolve each other by ARP, answer ICMP echo, and exchange UDP datagrams under loss, reordering and duplication (200 seeds); a seeded mutation test over recorded frames never panics and every bad frame is dropped and counted; an ARP or socket table at capacity gives a named error. |
| 47 | TCP (pure) | RFC 9293 state machine with MSS, window scaling, RFC 6298 retransmission timeout and NewReno (RFC 6582) behind a congestion trait (`on_ack`, `on_loss`, `on_timeout`, `cwnd`, `pacing_rate`). Host, 200 seeds each: a 64 MiB transfer arrives intact under 0%, 1% and 5% loss with reordering and duplication; open, half-close, reset and every timeout path; a full listen backlog drops SYNs (counted); our TCP interoperates with smoltcp's over the same link. |
| 48 | BBR (pure, conditional) | BBR behind the congestion trait, paced by the stack's timer wheel. Kept only if, on the simulated link, it beats NewReno's goodput at 1% loss and 50 ms RTT and a BBR flow beside a NewReno flow leaves NewReno at least a recorded share; otherwise the numbers are recorded and the step is skipped. |
| 49 | virtio-net | Board driver: one RX/TX queue pair, no offloads, RX buffers pre-posted from a fixed pool, interrupt completion; the stack runs in a kernel net task woken by the NIC, its next timer deadline and socket submits. The step-20 probe, which stops at the first block device, finds both devices (net ID 1, blk ID 2). Address from bootargs (`net=10.0.2.15/24,gw=10.0.2.2`). e2e: ICMP echo to 10.0.2.2 replies (verify QEMU answers it) and a UDP datagram round-trips through an echo the test runs on the host; frame counters printed; boot without a NIC unchanged. |
| 50 | Sockets as handles | A `NetStack` handle (rights: connect, listen) grants network access; `socket`, `bind`, `listen`, `connect`, `accept` return socket handles with read and write rights; send, receive, accept and connect are completion ops; options are typed calls; socket buffers are charged to the owner's budget. A loopback `Nic` in the kernel. musl maps BSD sockets (`AF_INET` only) to them. e2e: two processes run a TCP echo over 127.0.0.1; a child spawned without the `NetStack` handle cannot connect; buffers over the budget fail with `ENOBUFS`. |
| 51 | Web server and client (milestone) | `httpd` (serves one page) and `fetch` (GET by IP, prints the body) in `crates/user`. e2e: the milestone; the server stays up for 100 sequential GETs from the host. |
| 52 | Scaling and zero-copy | virtio-net negotiates `VIRTIO_NET_F_MQ` with a queue pair per CPU; flows steer by 4-tuple hash to one CPU's stack instance; `copy(src, dst, len)` moves file pages to a socket without a user-space copy. e2e on `-smp 4`: four connections on four cores all complete and a file served by `copy` matches its bytes; throughput before and after recorded. |

### Step details

- **46.** Benchmark (host, `cargo bench-host`): receive path per frame (parse, demux, deliver) for UDP, in ns; UDP datagrams per second over the loss-free in-memory link. Invariants: no clock (every entry point takes `now` in ns; `poll(now)` returns the next deadline), so a seed reproduces a run; the caller gives every table its fixed memory, full is an error, never a silent eviction of a socket; every field is range-checked once when decoded, a crafted frame is dropped and counted, never a panic, overflow or out-of-range index; IPv4 fragments are dropped and counted (no reassembly memory to exhaust). Avoids: a network stack as a permanent C CVE source (M12): the parser of untrusted bytes cannot corrupt the kernel.
- **47.** Benchmark (host): TCP goodput over the simulated link at 0% loss (CPU-bound MiB/s), and at 1% loss with 10 ms and 50 ms RTT; receive path per segment (ns). Invariants: 46's; initial sequence numbers and ephemeral ports from a keyed hash seeded by the caller (the DT seed in the kernel, RFC 6528); every connection's memory is the caller's fixed buffers; a listener's backlog is bounded. Avoids: TCP behavior shaped by decades of compatibility quirks: one RFC-cited path per mechanism, nothing else.
- **48.** Benchmark (host): goodput and fairness share against NewReno on the simulated link. Invariants: the congestion trait is the only seam; the default stays NewReno unless this step's numbers change it. Avoids: BBRv1's unfairness to loss-based flows, by gating on the fairness test.
- **49.** Needs phase 5 step 24 (the IRQ handler, net task and syscalls share the stack's state). Benchmark (hvf): UDP round trip to the host (median us), frames per second each way, boot time with and without a NIC (the probe). Invariants: DMA only into the fixed pool in the identity map; the pool is sized at boot and charged once, never grown; a full RX ring drops (counted), never allocates. Avoids: sk_buff-style allocation per packet on the hot path.
- **50.** Needs phase 5 step 24, and step 28 for a completion that wakes a task on another core. Benchmark (hvf): loopback TCP MiB/s, a 64-byte send + receive round trip against the pipe's 375 ns, connect + close. Invariants: no ambient authority (no handle, no network); rights only narrow on `dup`; ops are checked against rights at submit (D8); a socket's buffers are charged at creation and freed with the last handle. Avoids: `setsockopt`/`ioctl` multiplexers (M2), global `tcp_mem` pools that fail the wrong process, and capability bits (M6).
- **51.** Needs 49 and 50. Benchmark (hvf): HTTP GET round trip host to guest and guest to host (median us); 64 MiB through `hostfwd` each way (MiB/s; QEMU's user networking bounds it, recorded as found). Invariants: `httpd` needs only a `NetStack` handle with listen and a read-only directory handle. Avoids: a web server that must start as root to bind port 80; binding is a right on the handle.
- **52.** Needs phase 5 step 28 (per-CPU) and phase 6 steps 33 and 35 (refcounted frames, page cache). Benchmark (hvf, `-smp 4`): aggregate TCP MiB/s on 1 against 4 cores; serving a 64 MiB file with `copy` against read + send. Invariants: a flow's state lives on one CPU (no socket lock shared across CPUs); `copy` never lets a socket or pipe buffer alias a page another writer can change. Avoids: the big shared-socket locks RSS/RPS retrofitted, and Dirty Pipe-style aliasing of page-cache pages.

## smoltcp or write fresh (decided in step 46)

Checked against smoltcp 0.12 (0BSD) on 2026-10-07:

| Rule | smoltcp | Verdict |
| --- | --- | --- |
| `unsafe` only in `arch` and board crates | The crate denies `unsafe_code` outside its std-only `phy/sys`, but `heapless` 0.8, a required dependency, uses `unsafe` (about 230 occurrences); the workspace `forbid` covers only our crates | Fails |
| Pure crates have no dependencies | Five required (`bitflags`, `byteorder`, `cfg-if`, `heapless`, `managed`) | Fails |
| Fixed memory, fallible allocation | Socket buffers are borrowed from the caller; no `alloc` needed | Passes |
| Time as an input (reproducible tests) | `Interface::poll(now, ..)` | Passes |
| Per-CPU stacks (step 52) | One `Interface` owns every socket on a device | Works, as one instance per CPU |
| Speed | Congestion: Reno, CUBIC or none; it sends SACK blocks, but no SACK-driven recovery was found in `tcp.rs` (verify in step 46) | A fork for SACK, BBR, pacing |

Decision: write `crates/net` fresh. smoltcp becomes a host-only dev-dependency of `crates/net`'s tests (never linked into the kernel; needs owner approval), the interop oracle step 47 tests against. Reopen the decision if step 47 cannot match smoltcp's goodput on the simulated link.

## Notes

- `Nic` port trait, like `Disk`: `mac`, `mtu`, `transmit(frame) -> Result<(), Full>`, `receive(|frame| ..)`; one copy per frame until step 52.
- Simulated link (host test support in `crates/net`): seeded loss, reordering, duplication, delay and bandwidth in virtual time, so a failing seed replays exactly. Fuzzing is a seeded mutation test on stable (`cargo-fuzz` needs nightly).
- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12) against this doc: survey 46 is step 49, 47 is 46, 48 is 47-48, 49 is 50-51, 50 is 52. Deferred: DHCP and the DNS resolver (static address and IP literals reach the milestone) and IPv6 with NDP (survey 51) move to phase 9; the packet filter and the net-queue handle (survey 52) follow it. Renumber if phase 5, 6 or 7 changes length.

## What was done

Filled in as each step lands.
