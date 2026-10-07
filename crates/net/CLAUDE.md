# `crates/net` - the network stack

## What this crate is

`Stack<'a>` over a `Nic` port trait: Ethernet, ARP, IPv4, ICMP echo and UDP (phase 8 step 46). `Stack::new(config,
neighbors, sockets)` takes its ARP cache (`[Neighbor::EMPTY; N]`) and socket table (`Socket::new(buf)` per slot) from
the caller; then `bind` (a UDP port or an ICMP echo identifier), `close`, `send_to`, `recv_from`, and
`poll(nic, now)`, which handles every received frame and due timer and returns the next deadline. `Counters`
counts frames by outcome. L2/L3 and UDP in `src/lib.rs`, design notes at its top.

TCP (step 47) is `src/tcp.rs`, design notes at its top: `Stack::with_tcp(Tcp::new(key, connections, half_open,
time_wait))` adds connection slots (`TcpSocket::new(rx, tx)`, the rings from the caller), a half-open table
(`[HalfOpen::EMPTY; N]`), a TIME_WAIT table (`[TimeWait::EMPTY; N]`) and the 128-bit key for ISNs and ephemeral
ports. Then `listen`, `accept`, `connect(now, local, to)` (local 0 is ephemeral), `send`, `recv` (`Ok(0)` is the end
of the stream, `WouldBlock` is nothing yet), `shutdown` (half-close), `tcp_close` (release; a RST if data was left
unread), `abort` and `tcp_info` (state, error, cwnd, ssthresh, send window, RTO). Segments go out from `poll`.

The `Nic` trait: `mac`, `mtu`, `transmit(len, |buf| ..)` (the stack writes the frame into the driver's buffer) and
`receive(|frame| ..)`.

It is **NOT** SACK (step 48), a driver (virtio-net is the board's, step 49), socket handles, rights or budgets (the
kernel, step 50), DHCP, DNS or IPv6 (phase 9), or IPv4 fragment reassembly.

## Responsibilities

- Parse and answer: ARP requests aimed at us, ICMP echo requests; deliver UDP datagrams and ICMP echo replies to
  bound sockets (an ICMP socket sends and receives whole echo messages, like a Linux ping socket; the stack sets the
  identifier and checksum).
- Route: on-link unicast directly, anything else off the subnet through `Config::gateway` (`NoRoute` without one).
- Resolve: `send_to` to an unknown next hop sends an ARP request and returns `Unresolved` (the datagram is not queued;
  the caller retries after `poll`). `poll` re-sends every second, gives up after 3 requests and frees the entry. A
  neighbour learned 60 s ago is asked again while still used.

- TCP: RFC 9293 with MSS and window scaling, RFC 6298 RTO (initial 1 s, floor 200 ms, max 60 s; SYN given up
  after 6 retransmissions, data after 10, a SYN-ACK after 5), NewReno (RFC 5681, RFC 6582) with byte counting
  (RFC 3465) and go-back-N after a timeout, a persist timer that probes a zero window forever, a FIN-WAIT-2 timeout
  (60 s once released) and a 60 s TIME_WAIT. Out-of-order data is kept in the receive ring (4 ranges).

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, no `alloc`, workspace `forbid(unsafe_code)`. smoltcp is approved
  only as a host-only dev-dependency for step 47's interop tests; never a normal dependency.
- No clock: every entry point that can time out takes `now` (ns), so a seed replays a run exactly.
- Memory is the caller's: the ARP table, the socket table and each socket's receive buffer, and TCP's connection
  slots with their rings, half-open and TIME_WAIT tables; `Stack` itself is about 1.6 KiB (its reply buffer,
  `MAX_FRAME`).
- Leaf crate: the kernel (step 50) and the board (step 49) will depend on it; it depends on none of them.

## Invariants & rules

- Every frame is untrusted. Each field is range-checked once when decoded (slices via `get`/`split_at_checked`); a
  frame that fails is dropped and counted in exactly one `Counters` field (`malformed`, `checksum`, `fragments`,
  `ignored`, `no_socket`, `socket_full`, `unacceptable`), never a panic, overflow or out-of-range index; a frame TCP
  takes is counted in `tcp`.
- TCP verifies the checksum over the whole segment before reading a field or writing a byte, so a corrupt
  retransmission can never overwrite out-of-order data already kept.
- TCP's attack surface, each with a test: a SYN flood fills only the half-open table, which evicts its oldest entry
  (`syn_evicted`) and answers to the frame's source, so it never touches the ARP cache or a slot; RFC 5961 challenge
  ACKs (inexact in-window RST, any SYN, an ACK outside `snd_una - max window ..= snd_max`) at most 10 per second per
  connection (`challenge_acks`), never one global limit (CVE-2016-5696); an ACK above `snd_max` drops the whole
  segment; cwnd grows by bytes acknowledged, so ACK division gains nothing; an ICMP error must quote a sequence
  number in `snd_una..snd_max`, and a hard error aborts only a SYN-SENT connection (RFC 5927); a RST never ends
  TIME_WAIT (RFC 1337); a full TIME_WAIT table reuses its oldest entry (`time_wait_reused`), and a SYN above an
  entry's sequence starts a new connection.
- ISNs are SipHash-2-4 of the connection under the caller's key plus a 4 us clock (RFC 6528); ephemeral ports are
  RFC 6056 algorithm 3 under the same key. The kernel's key comes from the DT seed (step 49).
- Segments about a connection go to the MAC it resolved (a passive open: the SYN's source), never to a received
  frame's source; a pure ACK carries `snd_max`, so a go-back-N `snd_nxt` never starts an ACK war.
- Congestion control is NewReno as plain code; a trait comes with a second controller.
- IPv4 fragments (MF set or an offset) are dropped and counted: no reassembly memory to exhaust.
- ARP learns only from a reply to an entry we asked about (first reply wins); a request aimed at our IP is answered
  from its own sender fields but never learned, so talking back to a host that asked costs one ARP round trip. The
  ARP sender MAC must equal the Ethernet source and the sender IP must be an on-link unicast address other than
  ours. Everything else is `ignored`, so ARP traffic not aimed at us never evicts a neighbour. A full cache evicts
  its least recently used entry. ARP has no checksum: the MAC check rejects a flipped bit in either MAC, but a flip
  in the sender IP of a reply we asked for is learned (the NIC's FCS catches it on a real link). Pending requests
  live in the same bounded table and are freed after 3 unanswered tries.
- An echo reply goes to the request's Ethernet source; received IP traffic never writes the ARP cache.
- Residual risk: only an on-path attacker who answers our own request first can poison the cache.
- A full socket table is `TableFull`, a bound port `InUse`; nothing is evicted silently. A full socket buffer drops
  the datagram (`socket_full`).
- Replies built while the received frame is borrowed (ARP reply, echo reply) go through one buffer, sent after the
  NIC releases the frame.
- Checksums: summed once over the payload as big-endian 32-bit words, right after it is copied (`copy_sum`: a
  `memcpy`, then the sum over the copied bytes, which are hot in L1). A fused copy-and-sum loop measured slower on the
  host (64-byte receive path 35 against 31 ns, equal at 1472 bytes). A received datagram is copied into the socket
  buffer, then committed only if its checksum holds.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p net`. `tests/stack.rs` runs two stacks over the simulated link
  (`tests/sim/mod.rs`: seeded loss, reordering, duplication, single-bit corruption and delay in virtual time): ping
  and UDP echo of 32 messages each over a clean link and under 10% loss, 10% reordering, 5% duplication and 5%
  corruption for 200 seeds, every reply checked byte for byte. A seeded mutation test feeds 100k mutated copies of
  the frames recorded from a clean run (bit flips, byte and 16-bit field overwrites, truncation, extension) and checks
  each is counted exactly once, and that a single checksum-detectable mutation never delivers changed data. ARP:
  spoofed and unsolicited traffic, unasked and invalid senders (broadcast, zero, multicast MAC; our IP; off-link),
  a 10k-frame flood not aimed at us, LRU eviction, retries, stale refresh and the `poll` deadline.
  Also routing, named socket errors, a full socket buffer, fragments. `src/lib.rs` unit-tests the socket ring's
  wrap-around checksum.
- TCP (`tests/tcp.rs`): A and B send 1 MiB each way at once and close, for 200 seeds at 0%, 1% and 5% loss (with 2%
  reordering, 1% duplication, 0.5% corruption, 1 ms delay), and 64 MiB each way for 3 seeds at each rate, every byte
  and both ends of stream checked; A's ISN starts half the transfer below 2^32 (the test picks the start time from a
  dry run's ISN), so sequence numbers wrap. `sixty_four_mib_soak` (`--ignored`, about two minutes) runs 64 MiB for
  200 seeds at each rate. A scripted peer on a `Tap` drives the attack tests (SYN flood, RFC 5961, ACK above
  `snd_max` and ACK division, RFC 5927), every timeout (SYN, data, SYN-ACK, FIN-WAIT-2, TIME_WAIT), and the link
  drives half-close, abort and refusal, simultaneous open, a reader stalled for 10 RTOs, 21 sequential connections
  through one slot and four TIME_WAIT entries, and full tables. The mutation test replays 100k mutated copies of a
  recorded TCP exchange into the same connection (same keys and times, so the same ISNs). `src/tcp.rs` unit-tests
  SipHash against `std`'s and out-of-order reassembly across the wrap.
- Interop (`tests/interop.rs`): our TCP against smoltcp 0.12's (pinned in `Cargo.lock`, a dev-dependency only) over
  the simulated link, each side opening in turn, 256 KiB each way for 20 seeds at 0%, 1% and 5% loss. smoltcp 0.12
  drops its retransmission timer on entering CLOSING, so the test has it close only after its data is acknowledged.
- Benchmark: `cargo bench-host` runs `benches/net.rs` (includes `tests/sim/mod.rs`); baseline rows in
  `docs/BENCHMARKS.md`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
