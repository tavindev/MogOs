# Phase 8: Networking

Goal (milestone): MogOs serves a web page to the Mac host through QEMU user networking, and fetches one. The e2e test runs an HTTP server on the host and boots MogOs with `-netdev user,id=n0,hostfwd=tcp:127.0.0.1:<port>-:80 -device virtio-net-device,netdev=n0`; the test GETs the guest's page from `127.0.0.1:<port>` and checks its body, and the guest's `fetch 10.0.2.2:<host port>` prints the host page's body. (QEMU user networking puts the guest at 10.0.2.15 and the host at 10.0.2.2; verify on QEMU 9.2 in step 49.)

## Steps

Steps 46-48 are pure: a new crate, `crates/net` (safe, `no_std`, no `alloc`, no dependencies), host-tested on the Mac over a simulated link, needing no SMP and no virtual memory change, so they run in parallel with phases 5 and 6 and with phase 7's steps 39-41. They run in order; outside `crates/net` they touch only the workspace members list, `Cargo.lock` (the smoltcp dev-dependency) and AGENTS.md's Layout. Steps 49-52 integrate with the kernel and wait on phases 5, 6 and 7 (each lists what it needs); 49 and 50 run in parallel, 51 joins them, 52 follows 51.

| # | Step | Done when |
| --- | --- | --- |
| 46 | L2/L3 crate: Ethernet, ARP, IPv4, ICMP, UDP (pure) | `crates/net` over a `Nic` trait. Host: two stacks over the simulated link resolve each other by ARP, answer ICMP echo, and exchange UDP datagrams under loss, reordering and duplication (200 seeds); a seeded mutation test over recorded frames never panics and every bad frame is dropped and counted; a flood of ARP traffic not aimed at us never evicts a live neighbour; a full socket table gives a named error. |
| 47 | TCP (pure) | RFC 9293 state machine with MSS, window scaling, RFC 6298 retransmission timeout and NewReno (RFC 6582) with byte counting (RFC 3465). Host: a transfer arrives intact under 0%, 1% and 5% loss with reordering and duplication (1 MiB for 200 seeds, 64 MiB for 3), with ISNs seeded near 2^32 so sequence numbers wrap; open, half-close, reset and every timeout path; a reader that stalls for 10 retransmission timeouts then resumes (persist timer); 10k spoofed SYNs, then a real client still connects within one retransmission timeout; in-window but inexact RST and SYN get a challenge ACK, rate-limited per socket (RFC 5961); ACKs above `snd_nxt` are ignored and ACK division does not grow cwnd; ICMP errors are checked against the connection's sequence numbers and a hard error does not abort an established connection (RFC 5927); a full connection table reuses its oldest TIME_WAIT entry (counted); the seeded mutation test covers TCP segments; our TCP interoperates with smoltcp's over the same link. |
| 48 | SACK recovery (pure) | SACK (RFC 2018) with loss recovery per RFC 6675. Host: at 5% loss, goodput beats step 47's NewReno by a recorded factor; crafted SACK blocks (out of window, overlapping, more than fit) are ignored, never trusted; the step-47 tests still pass. |
| 49 | virtio-net | Board driver: one RX/TX queue pair, no offloads, RX buffers pre-posted from a fixed pool, interrupt completion; the stack runs in a kernel net task woken by the NIC, its next timer deadline and socket submits. The step-20 probe, which stops at the first block device, finds both devices (net ID 1, blk ID 2). Address from bootargs (`net=10.0.2.15/24,gw=10.0.2.2`). e2e: ICMP echo to 10.0.2.2 replies (verify QEMU answers it) and a UDP datagram round-trips through an echo the test runs on the host; frame counters printed; boot without a NIC unchanged. |
| 50 | Sockets as handles | A `NetStack` handle (rights: connect, listen) grants network access; `socket`, `bind`, `listen`, `connect`, `accept` return socket handles with read and write rights; send, receive, accept and connect are completion ops; `io_wait` returns the next completion of any op the caller has in flight (wait-any, from a bounded per-process queue charged to its budget), so one thread serves many connections; options are typed calls; socket buffers are charged to the owner's budget. A loopback `Nic` in the kernel. musl maps BSD sockets (`AF_INET` only) to them. e2e: two processes run a TCP echo over 127.0.0.1; one process serves 8 connections at once through `io_wait`; a child spawned without the `NetStack` handle cannot connect; buffers over the budget fail with `ENOBUFS`. |
| 51 | Web server and client (milestone) | `httpd` (serves one page) and `fetch` (GET by IP, prints the body) in `crates/user`. e2e: the milestone; the server answers more sequential GETs from the host than its connection table holds (TIME_WAIT reuse). |
| 52 | Software flow steering and zero-copy | One RX queue: the core taking the IRQ hashes the 4-tuple and hands each frame to the owning CPU's stack instance; `copy(src, dst, len)` moves file pages to a socket without a user-space copy. e2e on `-smp 4`: four connections served on four cores all complete and a file served by `copy` matches its bytes; throughput before and after recorded. |

### Step details

- **46.** Benchmark (host, `cargo bench-host`): receive path per frame (parse, demux, deliver) for UDP, in ns; UDP datagrams per second over the loss-free in-memory link. Invariants: no clock (every entry point takes `now` in ns; `poll(now)` returns the next deadline), so a seed reproduces a run; the caller gives every table its fixed memory; the ARP cache evicts its least recently used entry and learns only from replies to our own requests (a request aimed at us is answered, never learned); a full socket table is an error, never a silent eviction; every field is range-checked once when decoded, a crafted frame is dropped and counted, never a panic, overflow or out-of-range index; IPv4 fragments are dropped and counted (no reassembly memory to exhaust). Avoids: a network stack as a permanent C CVE source (M12): the parser of untrusted bytes cannot corrupt the kernel.
- **47.** Benchmark (host): TCP goodput over the simulated link at 0% loss (CPU-bound MiB/s), and at 1% loss with 10 ms and 50 ms RTT (with fixed buffers the 50 ms number mostly reflects buffer size over RTT); receive path per segment (ns). Invariants: 46's; initial sequence numbers and ephemeral ports from a keyed hash seeded by the caller (the DT seed in the kernel, RFC 6528); half-open connections live in their own fixed table, and when it is full SYNs are answered with SYN cookies (the user approved cookies over oldest-first eviction after review, 2026-10-07); TIME_WAIT entries are compact and the oldest is reused when the table is full, or a new SYN with a higher sequence number is accepted into one; challenge ACKs are rate-limited per socket; congestion control is NewReno as plain code (a trait comes with a second controller). Avoids: the global challenge-ACK limit that leaked connection state off-path (CVE-2016-5696), and TCP behavior shaped by decades of compatibility quirks: one RFC-cited path per mechanism.
- **48.** RFC 6675 over RACK-TLP because it extends the duplicate-ACK counting NewReno already has with one scoreboard and needs no new timer. Benchmark (host): goodput at 1% and 5% loss against step 47. Invariants: 47's; the scoreboard is fixed memory per connection, and SACK blocks only ever mark data the sender sent. Avoids: SACK-processing resource exhaustion (the 2019 SACK Panic CVEs) by bounding the scoreboard.
- **49.** Needs phase 5 step 24 (the IRQ handler, net task and syscalls share the stack's state). Benchmark (hvf): UDP round trip to the host (median us), frames per second each way, boot time with and without a NIC (the probe). Invariants: DMA only into the fixed pool in the identity map; the pool is sized at boot and charged once, never grown; a full RX ring drops (counted), never allocates. Avoids: sk_buff-style allocation per packet on the hot path.
- **50.** Needs phase 7 step 42 (non-blocking submit and complete, `io_cancel`), phase 5 step 24, and step 28 for a completion that wakes a task on another core. Benchmark (hvf): loopback TCP MiB/s, a 64-byte send + receive round trip against the pipe's 375 ns, connect + close. Invariants: no ambient authority (no handle, no network); rights only narrow on `dup`; ops are checked against rights at submit (D8); a socket's buffers are charged at creation and freed with the last handle. Avoids: `setsockopt`/`ioctl` multiplexers (M2), global `tcp_mem` pools that fail the wrong process, capability bits (M6), and a separate readiness API (M4: waiting is a completion).
- **51.** Needs 49 and 50. Benchmark (hvf): HTTP GET round trip host to guest and guest to host (median us); 64 MiB through `hostfwd` each way (MiB/s). `hostfwd` ends TCP inside QEMU's user networking, so these numbers measure it more than our stack; they are recorded as found. Invariants: `httpd` needs only a `NetStack` handle with listen and a read-only directory handle. Avoids: a web server that must start as root to bind port 80; binding is a right on the handle.
- **52.** Needs phase 5 step 28 (per-CPU) and phase 6 steps 33 and 35 (refcounted frames, page cache). QEMU 9.2's user netdev has no queues (`queues=4` is rejected) and tap needs a kext macOS no longer ships, so negotiating `VIRTIO_NET_F_MQ` waits for a tap or vhost-user backend. Benchmark (hvf, `-smp 4`): aggregate TCP MiB/s on 1 against 4 cores; serving a 64 MiB file with `copy` against read + send. Invariants: a flow's state lives on one CPU (no socket lock shared across CPUs); `copy` never lets a socket or pipe buffer alias a page another writer can change. Avoids: the big shared-socket locks RSS/RPS retrofitted, and Dirty Pipe-style aliasing of page-cache pages.

## smoltcp or write fresh (decided)

Checked against smoltcp 0.12 (0BSD) on 2026-10-07:

| Rule | smoltcp | Verdict |
| --- | --- | --- |
| `unsafe` only in `arch` and board crates | The crate denies `unsafe_code` outside its std-only `phy/sys`, but `heapless` 0.8, a required dependency, uses `unsafe` (about 230 occurrences); the workspace `forbid` covers only our crates | Fails |
| Pure crates have no dependencies | Five required (`bitflags`, `byteorder`, `cfg-if`, `heapless`, `managed`) | Fails |
| Fixed memory, fallible allocation | Socket buffers are borrowed from the caller; no `alloc` needed | Passes |
| Time as an input (reproducible tests) | `Interface::poll(now, ..)` | Passes |
| Per-CPU stacks (step 52) | One `Interface` owns every socket on a device | Works, as one instance per CPU |
| Speed | Congestion: Reno, CUBIC or none; it sends SACK blocks, but no SACK-driven recovery was found in `tcp.rs` | A fork for SACK recovery (step 48) |

Decision: write `crates/net` fresh. smoltcp is a host-only dev-dependency of `crates/net`'s tests, approved by the owner for interop tests only (never linked into the kernel; `cargo clippy` without `--all-targets` never builds it): the oracle step 47 tests against. Reopen the decision if step 47 cannot match smoltcp's goodput on the simulated link.

## Notes

- `Nic` port trait, like `Disk`: `mac`, `mtu`, `transmit(len, |buf| ..)` (the stack writes the frame straight into the driver's buffer; the TCP checksum is computed during the copy from the socket buffer), `receive(|frame| ..)`.
- Simulated link (host test support in `crates/net`): seeded loss, reordering, duplication, delay and bandwidth in virtual time, so a failing seed replays exactly. Fuzzing is a seeded mutation test on stable (`cargo-fuzz` needs nightly).
- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12) against this doc: survey 46 is step 49, 47 is 46, 48 is 47-48, 49 is 50-51, 50 is 52. Deferred: BBR and CUBIC until a tracked benchmark asks (survey: P3); multiqueue virtio-net until a backend with queues; DHCP and the DNS resolver (static address and IP literals reach the milestone) and IPv6 with NDP (survey 51) move to phase 9; the packet filter and the net-queue handle (survey 52) follow it. Renumber if phase 5, 6 or 7 changes length.

## What was done

- **46.** `crates/net` ([CLAUDE.md](../../crates/net/CLAUDE.md)): `Stack` over the `Nic` trait as in Notes, with
  the ARP cache, socket table and socket buffers from the caller and `now` passed in. UDP sockets and ICMP echo
  sockets (Linux ping-socket shape) share one table; an unresolved next hop returns `Unresolved` after sending the
  ARP request (no datagram queue), and `poll` retries ARP every second, three times, returning the next deadline.
  ARP also requires the sender MAC to equal the Ethernet source, which keeps a corrupted frame from poisoning the
  cache. Replies (ARP, echo) are built in one stack-owned frame buffer while the received frame is borrowed, then
  sent. Tests (`tests/stack.rs`, simulated link in `tests/sim/mod.rs`): 200 seeds of ping and UDP echo under loss,
  reordering, duplication and corruption; 100k seeded mutations of recorded frames; ARP spoof, flood, LRU and retry;
  named socket errors; fragments. Checksum: copy then sum in 32-bit words measured faster than a fused copy-and-sum
  loop on the host, so the copy is a plain `memcpy`. Benchmarks in `docs/BENCHMARKS.md`.
- **46 (security fix).** ARP requests never learn: a request aimed at us is answered from its own sender fields, and
  only a reply to our own request writes the cache, so a host on the link cannot overwrite the gateway's entry by
  asking about us. Cost: one ARP round trip the first time we talk back to a host that asked.
- **47.** `crates/net/src/tcp.rs`: `Stack::with_tcp(Tcp::new(key, connections, half_open, time_wait))`, every table
  and ring from the caller. RFC 9293 with MSS and window scaling, RFC 6298 RTO (floor 200 ms, Linux's; the RFC's 1 s
  is a SHOULD), NewReno with RFC 3465 byte counting and go-back-N after a timeout, a persist timer, out-of-order data
  kept in the receive ring (4 ranges). Half-open table with oldest eviction rather than SYN cookies: simpler (no
  options to encode) and a real SYN keeps its entry for a round trip unless a table's worth of SYNs arrives within
  it. ISNs and ephemeral ports from SipHash-2-4 under the caller's key. A pure ACK carries `snd_max` (BSD's rule):
  with `snd_nxt` both ends of a go-back-N recovery rejected each other's ACKs forever, which the 64 MiB runs found.
  Segments about a connection go to the MAC it resolved, so a challenge ACK never answers a spoofed frame's source.
  Tests (`tests/tcp.rs`): 1 MiB each way for 200 seeds and 64 MiB for 3 at 0%, 1% and 5% loss with reordering,
  duplication and corruption, the client's ISN half a transfer below 2^32 (so its send and the server's receive
  sequence spaces wrap); the attack list on a scripted peer; every timeout;
  half-close, abort, refusal, simultaneous open; a reader stalled for 10 RTOs; 21 sequential connections through one
  slot; the mutation test over TCP segments. The 64 MiB soak over 200 seeds is `--ignored` (about 43 s). The
  gate's TCP tests take under a second. Interop (`tests/interop.rs`): our TCP and smoltcp 0.12's, each side opening,
  256 KiB each way, 20 seeds at each loss rate; smoltcp 0.12 cancels its retransmission timer on entering CLOSING
  with data in flight, so the test never closes both sides at once. The simulated link's queues became a binary
  heap (the same delivery order), since the scan per frame made 1 MiB windows quadratic. Benchmarks in
  `docs/BENCHMARKS.md`; the UDP rows did not move (interleaved with step 46, best minimums 29.6 against 29.8 ns and
  69.9 against 69.1 ns, machine at load 15-18). Not done: a smoltcp-to-smoltcp goodput figure for the decision's
  reopen clause.
- **47 (review).** Timers: one deadline per connection derived from its state (retransmission, persist, FIN-WAIT-2)
  and cached at the end of each event, replacing a field armed and cancelled in many places; that fixed a window
  update in FIN-WAIT-2 pinning a released slot, a timeout into a window below one MSS sending nothing, and
  zero-window probes never giving up on a silent peer (now after 10). Tests check after every step that a connection
  with work outstanding has a deadline and that a silent peer always ends in CLOSED. SYN cookies replace oldest-first
  eviction (user decision): a 29-bit SipHash over the connection and the peer's ISN, an MSS index and one clock bit,
  accepted only within two 16 s periods of the stack sending one. A SYN taking over TIME_WAIT starts 65537 plus 24
  keyed bits above the old sequence space. Out-of-window ACKs are limited to one per 500 ms per connection; receiver
  silly-window avoidance; a SYN-ACK offers a connection slot's window; simultaneous open scales only if the peer
  offered; an ACK below `snd_una` never updates the window; demux tries the last matched slot first (63 idle
  connections ahead: 92 ns per segment against 80 alone). A security scan found three holes, each now with an attack
  test: cookies accepted with no flood running (brute-forceable), a SYN-ACK checked as a cookie ACK, and a TIME_WAIT
  ISS of exactly old `snd_nxt` + 65537. RFC 7323 timestamps with PAWS were built and measured interleaved: the
  receive path did not move, but loss-free goodput fell 3-5% (64 KiB window medians 6025-6223 against 6403-6470
  MiB/s) and connect + close rose 7% (minimums 443-466 against 418-423 ns), so they were dropped and the ISS rule is
  the wrapped-sequence protection. smoltcp 0.12 also drops its retransmission timer on ESTABLISHED + FIN ->
  CLOSE-WAIT; the interop test works around both of its cases. FIN-WAIT-2 times out only once the caller releases
  the connection (Linux's rule, the coordinator's decision): an open half-closed connection waits for a slow peer.
  The half-open table is found in O(1) (a keyed mix picks a run of 8 slots; a full run means a cookie), for tables
  sized from RAM: filling 4096 entries costs 45 ns per SYN, down from 2.0 us for the linear scan, and 64 entries 48 ns
  against 68. Still linear: the per-`poll` walks over the half-open table and the slots, and the TIME_WAIT lookup.
  A security scan flagged key reuse (one key for ISNs, ports, cookies, TIME_WAIT and the weak half-open mix, and the
  cookie clock XORed into the key): each use now has a key derived from the seed, and the cookie's clock and index
  are hashed as message words. Cost: 1.5 ns per cookie SYN (a third message word); connect + close unchanged.
- **47 (second review).** An owed FIN or SYN that never left (next hop unresolved) now runs the retransmission timer,
  and `poll` reports the ARP retry of a request TCP just started; the link tests lose ARP frames so the liveness rule
  covers it. Released connections are bounded like Linux's orphans (FIN-WAIT-2 60 s from our FIN's ACK whatever the
  peer sends, at most 8 zero-window probes); `tcp_info` reports `released`. `poll` walks the half-open table only when
  a SYN-ACK retransmission is due (idle poll 7 ns at any size, from 1.2 us at 4096). A SYN takes TIME_WAIT over only
  with a listener and room for the gap ISS, never via a cookie. A cookie ACK that misses a half-open entry for the
  same connection is still checked as a cookie. The cookie gate is per listener. Interleaved against 9c4ca51: TCP
  rows within noise, connect + close 398-407 against 427-430 ns; UDP 1472-byte receive reads 2 ns slower
  (66.3-67.8 against 64.3-65.2) with no change on its path, and both builds match with loops aligned to 64 bytes
  (65.5-67.7 against 65.2-66.1), so it is code placement; `-C llvm-args=-align-loops=64` in the build config is the
  fix outside this crate.
