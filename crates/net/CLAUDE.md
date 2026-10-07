# `crates/net` - the network stack

## What this crate is

`Stack<'a>` over a `Nic` port trait: Ethernet, ARP, IPv4, ICMP echo and UDP (phase 8 step 46). `Stack::new(config,
neighbors, sockets)` takes its ARP cache (`[Neighbor::EMPTY; N]`) and socket table (`Socket::new(buf)` per slot) from
the caller; then `bind` (a UDP port or an ICMP echo identifier), `close`, `send_to`, `recv_from`, and
`poll(nic, now)`, which handles every received frame and due timer and returns the next deadline. `Counters`
counts frames by outcome. L2/L3 and UDP in `src/lib.rs`, design notes at its top.

TCP (step 47) is `src/tcp.rs`, design notes at its top: `Stack::with_tcp(Tcp::new(key, connections, half_open,
time_wait))` adds connection slots (`TcpSocket::new(rx, tx)`, the rings from the caller), a half-open table
(`[HalfOpen::EMPTY; N]`), a TIME_WAIT table (`[TimeWait::EMPTY; N]`) and the 128-bit seed every key is derived
from. Then `listen`, `accept`, `connect(now, local, to)` (local 0 is ephemeral), `send`, `recv` (`Ok(0)` is the end
of the stream, `WouldBlock` is nothing yet), `shutdown` (half-close), `tcp_close` (release; a RST if data was left
unread), `abort` and `tcp_info` (state, error, cwnd, ssthresh, send window, RTO, the deadline the next `poll` acts
on, bytes queued). Segments go out from `poll`.

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
  (RFC 3465) and go-back-N after a timeout, a persist timer that probes a zero window as long as the peer answers
  and gives up (`TimedOut`) after 10 unanswered probes. Released (orphan) connections are bounded like Linux's:
  FIN-WAIT-2 ends 60 s after our FIN was acknowledged whatever the peer sends (an open half-closed connection
  waits as long as it likes), and a zero window gets at most 8 probes even if answered. A FIN (or SYN) owed but not
  yet sent, say while the next hop does not resolve, runs the retransmission timer like one in flight. TIME_WAIT
  lasts 60 s, restarted only by the retransmitted FIN. Sender silly-window avoidance applies to new data only, receiver avoidance (RFC 9293 3.8.6.2.2) moves the window's edge by min(MSS, ring / 2) or not at
  all, and out-of-window segments get at most one ACK per 500 ms per connection. Out-of-order data is kept in the
  receive ring (4 ranges). No timestamps: RFC 7323 timestamps with PAWS were built and measured, and cost 3-5% of
  loss-free goodput and 7% of connect + close beyond noise, so the ISS rule below is the only wrapped-sequence
  protection.

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
- Timers: one deadline per connection, derived from its state by `deadline()` from a single start time and cached at
  the end of every event that can move it (a segment in, a timer firing, an output attempt, which every caller
  action leads to). No purpose arms or cancels another's. The tests check after every poll that a connection with
  work outstanding (anything but idle in ESTABLISHED or CLOSE-WAIT, or an open FIN-WAIT-2; `tcp_info().released`
  tells them apart) has a deadline, and that a silent peer always ends in CLOSED or idle. The link tests also lose
  30% of ARP frames (all of them for a quarter of the silent-peer seeds), so the rule covers neighbours that never
  resolve. `Stack::poll` runs TCP before the ARP walk, so a request TCP just started reports its retry deadline.
- The half-open table is found in O(1) at any size: a keyed multiply-xorshift mix of the connection picks a run of 8
  slots, every lookup checks the whole run (so freeing is just clearing), and a SYN whose run is full gets a cookie.
  The mix is not SipHash on purpose: SipHash cost 20 ns per handshake, and steering SYNs into one run only sends them
  to cookies. Filling a table to the last slot sends about 3% (random connections: up to about 11%) to cookies.
  `poll` walks the half-open table only when its earliest SYN-ACK retransmission is due (idle poll 7 ns at any
  size, 1.2 us at 4096 entries before). Still linear: the connection slots per `poll`, and the TIME_WAIT lookup.
- TCP's attack surface, each with a test: a SYN flood fills only the half-open table and answers to the frame's
  source, so it never touches the ARP cache or a slot; a full table answers with SYN cookies (user-approved,
  replacing oldest-first eviction; `syn_cookies`): the ISS is a 2-bit MSS index and 30 bits of SipHash over the
  clock (16 s periods; the clock is not sent, so an ACK is checked against this period and the last, two hashes at
  most, on the flood path only), the connection and the peer's ISN, and a cookie ACK is accepted only
  while that listener has sent cookies in the last two periods (Linux's per-listener overflow time), never with SYN
  set, also when it does not match a half-open entry for the same connection; failures are counted (`bad_cookies`)
  and reset; a cookie connection runs without window scaling. Odds: a blind guess matches either period's cookie
  with probability 2 x 2^-30 = 2^-29 per ACK, yet each cookie is 30 bits; about 36 s of guessing at 10 GbE line rate (14.9 M minimum frames per second) once the attacker floods that same
  listener to open its gate; a flood on one listener no longer opens guessing on another. Residual: a cookie ACK
  replayed after its connection closed without TIME_WAIT (an `abort`), within two periods, opens a connection
  again; it needs the original ACK, so the attacker is on-path, as with Linux's cookies. RFC 5961 challenge
  ACKs (inexact in-window RST, any SYN, an ACK outside `snd_una - max window ..= snd_max`) at most 10 per second per
  connection (`challenge_acks`), never one global limit (CVE-2016-5696); an ACK above `snd_max` drops the whole
  segment; cwnd grows by bytes acknowledged, so ACK division gains nothing; an ICMP error must quote a sequence
  number in `snd_una..snd_max`, and a hard error aborts only a SYN-SENT connection (RFC 5927); a RST never ends
  TIME_WAIT (RFC 1337); a full TIME_WAIT table reuses its oldest entry (`time_wait_reused`), and a SYN above an
  entry's sequence starts a new connection whose ISS is the old `snd_nxt` plus 65537 plus 24 keyed bits, above
  anything the old connection sent and unpredictable, but only with a listener and room in the half-open table for
  that ISS (never a cookie); otherwise TIME_WAIT stays and answers the SYN with an ACK; a window update needs `snd_una <= ack`.
- ISNs are SipHash-2-4 of the connection plus a 4 us clock (RFC 6528); ephemeral ports are RFC 6056 algorithm 3.
  No key is used twice: `Tcp::new` derives one per use from the caller's seed (SipHash of the seed and a label):
  ISNs, ports, cookies, the TIME_WAIT takeover bits and the half-open mix, so the weak mix's observable collisions
  reveal nothing about the cookie or ISN keys. A cookie's clock and MSS index go into the hashed message, never the
  key. The kernel's seed comes from the DT seed (step 49).
- Segments about a connection go to the MAC from its last ARP resolution (each send resolves the next hop; until
  the first, a passive open's SYN source), never to a received frame's source; a pure ACK carries `snd_max`, so a
  go-back-N `snd_nxt` never starts an ACK war. Demux tries the last matched slot first.
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
  dry run's ISN), so A's send and B's receive sequence numbers wrap; every step checks the liveness rule. `sixty_four_mib_soak` (`--ignored`, about 43 s) runs 64 MiB for
  200 seeds at each rate. A scripted peer on a `Tap` drives the attack tests (SYN flood, RFC 5961, ACK above
  `snd_max` and ACK division, RFC 5927), every timeout (SYN, data, SYN-ACK, FIN-WAIT-2, TIME_WAIT), and the link
  drives half-close, abort and refusal, simultaneous open, a reader stalled for 10 RTOs, 21 sequential connections
  through one slot and four TIME_WAIT entries, and full tables. Review regressions each have a test: a window update
  in FIN-WAIT-2, silent zero-window probes, a timeout into a window below one MSS, a pure ACK during go-back-N, the
  TIME_WAIT takeover's ISS (an old duplicate is rejected; the ISS is keyed), out-of-window ACK limiting, a silent
  peer cut at a random point for 40 seeds, receiver silly-window avoidance, a SYN-ACK's window, simultaneous-open
  scaling, an ACK below `snd_una`. SYN cookies: a flood inside the client's round trip keeps the real handshake,
  forged, wrong-ISN and expired cookies are rejected and counted, a replayed ACK reaches the open connection, a
  stack that sent no cookie accepts none, and a SYN-ACK is never a cookie's ACK. The mutation test replays 100k
  mutated copies of a recorded TCP exchange into the same connection (same keys and times, so the same ISNs) and
  checks the liveness rule after every frame. `src/tcp.rs` unit-tests
  SipHash against `std`'s and out-of-order reassembly across the wrap.
- Interop (`tests/interop.rs`): our TCP against smoltcp 0.12's (pinned in `Cargo.lock`, a dev-dependency only) over
  the simulated link, each side opening in turn, 256 KiB each way for 20 seeds at 0%, 1% and 5% loss. smoltcp 0.12
  drops its retransmission timer when a FIN moves it to CLOSING or CLOSE-WAIT, so the test sends it a FIN only after
  its data has all arrived, and has it close only after ours has.
- Benchmark: `cargo bench-host` runs `benches/net.rs` (includes `tests/sim/mod.rs`): UDP over the link and its
  receive path; TCP goodput on the loss-free link (64 KiB and 1 MiB windows), the per-segment receive path, connect
  plus close, and simulated goodput at 1% and 5% loss with 10 and 50 ms RTT (step 48's baseline). Rows in
  `docs/BENCHMARKS.md`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
